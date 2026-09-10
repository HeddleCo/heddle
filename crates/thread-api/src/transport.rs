// SPDX-License-Identifier: Apache-2.0
//! Transport-neutral credentials and errors for typed Thread RPC clients.
use std::future::Future;

use api::{
    framing,
    heddle::api::v1alpha1::{CallContext, CallFailure},
    v2::MethodDescriptor,
};
use prost::Message;

#[cfg(feature = "iroh")]
mod iroh;
#[cfg(feature = "iroh")]
pub use iroh::{IrohTransport, Reader, Writer, accepted_stream};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("transport I/O: {0}")]
    Io(String),
    #[error("RPC made no progress before its timeout")]
    Timeout,
    #[error("invalid v2 transport: {0}")]
    Protocol(&'static str),
    #[error("RPC failed: {0:?}")]
    Remote(RemoteFailure),
    #[error(transparent)]
    Framing(#[from] framing::FrameError),
    #[error(transparent)]
    Metadata(#[from] api::RequestMetadataError),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
}

/// Keep large optional challenge/conflict details in their wire representation
/// until an application needs them. Common error handling needs only code and
/// message; every original typed detail remains available without heap boxing.
#[derive(Debug)]
pub struct RemoteFailure {
    pub code: i32,
    pub message: String,
    detail: Option<Vec<u8>>,
}
impl RemoteFailure {
    pub fn detail(
        &self,
    ) -> Result<Option<api::heddle::api::v1alpha1::ErrorDetail>, prost::DecodeError> {
        self.detail
            .as_deref()
            .map(prost::Message::decode)
            .transpose()
    }
}
impl From<CallFailure> for RemoteFailure {
    fn from(failure: CallFailure) -> Self {
        Self {
            code: failure.code,
            message: failure.message,
            detail: failure.error.map(|e| e.encode_to_vec()),
        }
    }
}

/// Existing Heddle signer/broker integration plugs in here. Sign exactly the
/// supplied method and encoded body; carry the owner's Biscuit and attachment.
/// Shared CallContext/signing formats are retained data, not old RPC methods.
pub trait Authorize: Send + Sync {
    fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> impl Future<Output = Result<CallContext, Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_failure_retains_typed_details_without_boxing_the_error_path() {
        use api::heddle::api::v1alpha1::{ErrorDetail, ErrorReason};
        let detail = ErrorDetail {
            reason: ErrorReason::PolicyDenied as i32,
            resource: "thread".into(),
            ..Default::default()
        };
        let failure = RemoteFailure::from(CallFailure {
            code: 7,
            message: "review required".into(),
            error: Some(detail.clone()),
        });
        assert_eq!(failure.detail().expect("typed detail"), Some(detail));
        assert_eq!(failure.message, "review required");
    }
}
