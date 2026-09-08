//! Caller-supplied request credentials, independent of repository storage.
use std::time::{SystemTime, UNIX_EPOCH};

use api::{
    heddle::api::v1alpha1::{CallContext, RequestProof},
    v2::MethodDescriptor,
};
use crypto::{Ed25519Signer, Signer};

use crate::transport::{Authorize, Error};

/// A caller-owned credential and signer. Browser/device-root attachment is
/// supplied by the application, exactly as it is for a Weft request.
pub struct CredentialSigner {
    pub signer: Ed25519Signer,
    /// Raw serialized Biscuit, directly usable from IssuedCredential.biscuit.
    pub bearer: Vec<u8>,
    pub grant_envelope: Vec<u8>,
}
impl Authorize for CredentialSigner {
    async fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> Result<CallContext, Error> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Io(e.to_string()))?
            .as_millis();
        let timestamp = i64::try_from(timestamp).map_err(|_| Error::Protocol("invalid clock"))?;
        let identity = format!(
            "principal:device-key:{}",
            hex::encode(self.signer.public_key())
        );
        // UUID v4 is an OS-random nonce; there is no authority generation here.
        let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
        let signature = self
            .signer
            .sign(&api::signing::unary_bytes(
                &identity,
                method.path,
                timestamp,
                &nonce,
                body,
            ))
            .map_err(|e| Error::Io(e.to_string()))?;
        Ok(CallContext {
            bearer_capability: self.bearer.clone(),
            bearer_grant_envelope: self.grant_envelope.clone(),
            request_proof: Some(RequestProof {
                algorithm: "ed25519".into(),
                signing_identity: identity,
                timestamp_millis: timestamp,
                nonce,
                signature,
            }),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use api::v2::client::Rpc;
    use prost::Message;

    use super::*;

    #[test]
    fn request_context_verifies_without_a_transport_rewriting_its_identity() {
        let signer = Ed25519Signer::from_seed(&[17; 32]).expect("fixture signer");
        let key: [u8; 32] = signer.public_key().try_into().expect("key");
        let credentials = CredentialSigner {
            signer,
            bearer: vec![0, 255, 7, 128],
            grant_envelope: b"grant fixture".to_vec(),
        };
        let body = crate::contract::RenameThreadRequest {
            client_operation_id: "same-durable-operation".into(),
            name: "renamed".into(),
            ..Default::default()
        }
        .encode_to_vec();
        let method = crate::rpc::ThreadServiceRenameThread::METHOD;
        let context = ready(credentials.context(method, &body)).expect("context");
        assert_eq!(
            context.bearer_capability,
            [0, 255, 7, 128],
            "Biscuit stays raw binary"
        );
        assert_eq!(context.bearer_grant_envelope, b"grant fixture");
        let time = context
            .request_proof
            .as_ref()
            .expect("proof")
            .timestamp_millis;
        crate::request_proof::verify(&context, method, &body, &key, time)
            .expect("context independently binds the request operation identity");
    }

    fn ready<T>(future: impl Future<Output = T>) -> T {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("in-memory credential construction cannot wait on I/O"),
        }
    }
}
