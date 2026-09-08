//! Inbound Iroh protocol seam for browser-to-agent claim calls.
//!
//! Uses the shared framed native RPC transport. Public endpoint discovery is
//! separated from claim methods, which require exact browser proof and an
//! expiring local claim secret before foreground signing.

// `CallFailure` carries structured error detail and intentionally crosses the
// protocol/handler seam by value, matching Weft's native Iroh dispatcher.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use api::{
    framing::{
        MAX_CALL_CONTEXT, MAX_METHOD_PATH, decode_request_frame, encode_failure_response,
        encode_success_response,
    },
    heddle::api::v1alpha1::{CallContext, CallFailure, CallFailureCode},
};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};

pub(crate) const NATIVE_ALPN: &[u8] = api::HOSTED_ALPN_V1;
pub(crate) const CLAIM_PREPARE_METHOD: &str =
    "/heddle.api.v2alpha1.OwnerAuthorizationService/PrepareAccountClaim";
pub(crate) const CLAIM_SIGN_METHOD: &str =
    "/heddle.api.v2alpha1.OwnerAuthorizationService/SignAccountClaim";
const DESCRIBE_METHOD: &str = "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint";

const MAX_REQUEST_BODY: usize = 256 * 1024;
const MAX_REQUEST_FRAME: usize = 6 + MAX_METHOD_PATH + MAX_CALL_CONTEXT + MAX_REQUEST_BODY;

/// The account identity established by a valid short-lived claim secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedClaimPrincipal {
    pub(crate) subject: String,
    pub(crate) authorization_hash: String,
    pub(crate) browser_public_key: Vec<u8>,
}

pub(crate) trait ClaimSecretVerifier: Send + Sync + std::fmt::Debug + 'static {
    fn verify(
        &self,
        method: &str,
        context: &CallContext,
        body: &[u8],
    ) -> impl std::future::Future<Output = Result<VerifiedClaimPrincipal, CallFailure>> + Send;
}

pub(crate) trait ClaimHandler: Send + Sync + std::fmt::Debug + 'static {
    fn call(
        &self,
        method: &str,
        principal: VerifiedClaimPrincipal,
        body: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<u8>, CallFailure>> + Send;

    fn response_delivered(
        &self,
        _method: &str,
        _body: &[u8],
    ) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClaimProtocol<V, H> {
    verifier: Arc<V>,
    handler: Arc<H>,
    endpoint_key: [u8; 32],
    permits: Arc<tokio::sync::Semaphore>,
}

impl<V, H> ClaimProtocol<V, H> {
    pub(crate) fn new(verifier: Arc<V>, handler: Arc<H>, endpoint_key: [u8; 32]) -> Self {
        Self {
            verifier,
            handler,
            endpoint_key,
            permits: Arc::new(tokio::sync::Semaphore::new(32)),
        }
    }
}

impl<V, H> ProtocolHandler for ClaimProtocol<V, H>
where
    V: ClaimSecretVerifier,
    H: ClaimHandler,
{
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let mut calls = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = connection.accept_bi() => {
                    let Ok((send, recv)) = incoming else {
                        break;
                    };
                    let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
                        drop((send, recv));
                        continue;
                    };
                    let endpoint_key = self.endpoint_key;
                    let verifier = Arc::clone(&self.verifier);
                    let handler = Arc::clone(&self.handler);
                    calls.spawn(async move {
                        {
                            let _permit = permit;
                            tokio::time::timeout(std::time::Duration::from_secs(30), handle_call(verifier.as_ref(), handler.as_ref(), endpoint_key, send, recv)).await
                                .map_err(ClaimProtocolError::transport)?
                        }
                    });
                }
                completed = calls.join_next(), if !calls.is_empty() => {
                    match completed {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(error))) => {
                            tracing::warn!(%error, "agent claim call failed");
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "agent claim task failed");
                        }
                        None => {}
                    }
                }
            }
        }
        while let Some(completed) = calls.join_next().await {
            if let Err(error) = completed {
                tracing::warn!(%error, "agent claim task failed while closing");
            }
        }
        Ok(())
    }
}

async fn handle_call<V, H>(
    verifier: &V,
    handler: &H,
    endpoint_key: [u8; 32],
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<(), ClaimProtocolError>
where
    V: ClaimSecretVerifier,
    H: ClaimHandler,
{
    let request = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        recv.read_to_end(MAX_REQUEST_FRAME + 1),
    )
    .await
    .map_err(ClaimProtocolError::transport)?
    .map_err(ClaimProtocolError::transport)?;
    let mut successful_call = None;
    let response = match decode_request_frame(&request) {
        Ok(frame) if frame.body.len() > MAX_REQUEST_BODY => Err(failure(
            CallFailureCode::InvalidArgument,
            "claim request exceeds the body budget",
        )),
        Ok(frame) if frame.method == DESCRIBE_METHOD => describe(endpoint_key, frame.body),
        Ok(frame) => match validate_auth_shape(frame.method, &frame.context) {
            Ok(()) => match verifier
                .verify(frame.method, &frame.context, frame.body)
                .await
            {
                Ok(principal) => match handler.call(frame.method, principal, frame.body).await {
                    Ok(body) => {
                        let response = encode_success_response(&body)
                            .map_err(|error| failure(CallFailureCode::Internal, error.to_string()));
                        if response.is_ok() {
                            successful_call = Some((frame.method, frame.body));
                        }
                        response
                    }
                    Err(failure) => Err(failure),
                },
                Err(failure) => Err(failure),
            },
            Err(failure) => Err(failure),
        },
        Err(error) => Err(failure(CallFailureCode::InvalidArgument, error.to_string())),
    };
    let response = match response {
        Ok(response) => response,
        Err(failure) => encode_failure_response(&failure)
            .map_err(|error| ClaimProtocolError::framing(error.to_string()))?,
    };
    send.write_all(&response)
        .await
        .map_err(ClaimProtocolError::transport)?;
    send.finish().map_err(ClaimProtocolError::transport)?;
    if let Some((method, body)) = successful_call
        && matches!(send.stopped().await, Ok(None))
    {
        handler.response_delivered(method, body).await;
    }
    Ok(())
}

fn validate_auth_shape(method: &str, context: &CallContext) -> Result<(), CallFailure> {
    if !matches!(method, CLAIM_PREPARE_METHOD | CLAIM_SIGN_METHOD) {
        return Err(failure(
            CallFailureCode::Unimplemented,
            "unknown claim method",
        ));
    }
    if context.request_proof.is_none() {
        return Err(failure(
            CallFailureCode::Unauthenticated,
            "browser request proof is required",
        ));
    }
    Ok(())
}

fn failure(code: CallFailureCode, message: impl Into<String>) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: message.into(),
        error: None,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("agent claim protocol: {0}")]
struct ClaimProtocolError(String);

impl ClaimProtocolError {
    fn transport(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }

    fn framing(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

fn describe(endpoint_key: [u8; 32], body: &[u8]) -> Result<Vec<u8>, CallFailure> {
    use api::heddle::api::v2alpha1::*;
    use prost::Message;
    DescribeEndpointRequest::decode(body)
        .map_err(|_| failure(CallFailureCode::InvalidArgument, "invalid endpoint request"))?;
    let description = DescribeEndpointResponse {
        endpoint: Some(EndpointRef {
            public_key: endpoint_key.to_vec(),
            kind: EndpointKind::Device as i32,
        }),
        supported_packages: vec!["heddle.api.v2alpha1".into()],
        implemented_methods: vec![
            DESCRIBE_METHOD.into(),
            CLAIM_PREPARE_METHOD.into(),
            CLAIM_SIGN_METHOD.into(),
        ],
        default_read_budget: Some(ReadBudget {
            max_items: 100,
            max_frame_bytes: 256 * 1024,
            max_snapshot_bytes: 1024 * 1024,
        }),
        max_pending_batch_bytes: 1024 * 1024,
        ..Default::default()
    };
    encode_success_response(&description.encode_to_vec()).map_err(|_| {
        failure(
            CallFailureCode::Internal,
            "cannot encode endpoint description",
        )
    })
}
