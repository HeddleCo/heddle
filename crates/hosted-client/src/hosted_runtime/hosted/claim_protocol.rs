//! Inbound Iroh protocol seam for browser-to-agent claim calls.
//!
//! Claim request and response messages land with the account flow. This module
//! owns the transport invariant they depend on now: a dedicated versioned
//! ALPN, the same call framing used by Weft's Iroh surface, and a fail-closed
//! claim-secret gate before a method-specific handler can observe a body.

// `CallFailure` carries structured error detail and intentionally crosses the
// protocol/handler seam by value, matching Weft's native Iroh dispatcher.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use api::{
    framing::{
        MAX_CALL_CONTEXT, MAX_CONTROL_BODY, MAX_METHOD_PATH, decode_request_frame,
        encode_failure_response, encode_success_response,
    },
    heddle::api::v1alpha1::{CallContext, CallFailure, CallFailureCode},
};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};

pub(crate) const CLAIM_ALPN_V1: &[u8] = b"heddle-claim/1";
pub(crate) const CLAIM_RESOLVE_METHOD: &str = "/heddle.claim.v1.ClaimService/Resolve";
pub(crate) const CLAIM_CONSENT_METHOD: &str = "/heddle.claim.v1.ClaimService/Consent";

const MAX_REQUEST_FRAME: usize = 6 + MAX_METHOD_PATH + MAX_CALL_CONTEXT + MAX_CONTROL_BODY;

/// The account identity established by a valid short-lived claim secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedClaimPrincipal {
    pub(crate) subject: String,
    pub(crate) authorization_hash: String,
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
}

impl<V, H> ClaimProtocol<V, H> {
    pub(crate) fn new(verifier: Arc<V>, handler: Arc<H>) -> Self {
        Self { verifier, handler }
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
                    let verifier = Arc::clone(&self.verifier);
                    let handler = Arc::clone(&self.handler);
                    calls.spawn(async move {
                        handle_call(verifier.as_ref(), handler.as_ref(), send, recv).await
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
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<(), ClaimProtocolError>
where
    V: ClaimSecretVerifier,
    H: ClaimHandler,
{
    let request = recv
        .read_to_end(MAX_REQUEST_FRAME + 1)
        .await
        .map_err(ClaimProtocolError::transport)?;
    let mut successful_call = None;
    let response = match decode_request_frame(&request) {
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
    if !matches!(method, CLAIM_RESOLVE_METHOD | CLAIM_CONSENT_METHOD) {
        return Err(failure(
            CallFailureCode::Unimplemented,
            "unknown claim method",
        ));
    }
    if context.bearer_capability.is_empty() {
        return Err(failure(
            CallFailureCode::Unauthenticated,
            "a claim secret is required",
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

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use api::{
        framing::{ResponseFrame, decode_response_frame, encode_request_frame},
        heddle::api::v1alpha1::CallContext,
    };
    use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

    use super::*;

    #[derive(Debug)]
    struct ExactSecretVerifier {
        calls: AtomicUsize,
    }

    impl ClaimSecretVerifier for ExactSecretVerifier {
        async fn verify(
            &self,
            method: &str,
            context: &CallContext,
            body: &[u8],
        ) -> Result<VerifiedClaimPrincipal, CallFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if context.bearer_capability != b"valid-secret" {
                return Err(failure(
                    CallFailureCode::Unauthenticated,
                    "invalid claim secret",
                ));
            }
            assert_eq!(method, CLAIM_RESOLVE_METHOD);
            assert_eq!(body, b"resolve");
            Ok(VerifiedClaimPrincipal {
                subject: "agent:test".to_string(),
                authorization_hash: "test-generation".to_string(),
            })
        }
    }

    #[derive(Debug)]
    struct EchoClaimHandler {
        calls: AtomicUsize,
    }

    impl ClaimHandler for EchoClaimHandler {
        async fn call(
            &self,
            method: &str,
            principal: VerifiedClaimPrincipal,
            body: &[u8],
        ) -> Result<Vec<u8>, CallFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok([
                principal.subject.as_bytes(),
                b":",
                method.as_bytes(),
                b":",
                body,
            ]
            .concat())
        }
    }

    fn context(bearer: &[u8]) -> CallContext {
        CallContext {
            bearer_capability: bearer.to_vec(),
            ..CallContext::default()
        }
    }

    async fn request(
        client: &Endpoint,
        server: iroh::EndpointAddr,
        alpn: &[u8],
        context: CallContext,
    ) -> Result<Vec<u8>, String> {
        let connection = client
            .connect(server, alpn)
            .await
            .map_err(|error| error.to_string())?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| error.to_string())?;
        let frame = encode_request_frame(CLAIM_RESOLVE_METHOD, &context, b"resolve")
            .map_err(|error| error.to_string())?;
        send.write_all(&frame)
            .await
            .map_err(|error| error.to_string())?;
        send.finish().map_err(|error| error.to_string())?;
        recv.read_to_end(MAX_CONTROL_BODY + 1)
            .await
            .map_err(|error| error.to_string())
    }

    async fn endpoints(
        verifier: Arc<ExactSecretVerifier>,
        handler: Arc<EchoClaimHandler>,
    ) -> (Router, Endpoint, iroh::EndpointAddr) {
        let server = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("server bind addr")
            .bind()
            .await
            .expect("server bind");
        let address = server.addr();
        let router = Router::builder(server)
            .accept(CLAIM_ALPN_V1, ClaimProtocol::new(verifier, handler))
            .spawn();
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("client bind addr")
            .bind()
            .await
            .expect("client bind");
        (router, client, address)
    }

    #[tokio::test]
    async fn claim_alpn_routes_authenticated_calls() {
        let verifier = Arc::new(ExactSecretVerifier {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(EchoClaimHandler {
            calls: AtomicUsize::new(0),
        });
        let (router, client, address) =
            endpoints(Arc::clone(&verifier), Arc::clone(&handler)).await;

        let response = request(&client, address, CLAIM_ALPN_V1, context(b"valid-secret"))
            .await
            .expect("authenticated claim call");
        let ResponseFrame::Success(body) = decode_response_frame(&response).expect("response")
        else {
            panic!("expected success response");
        };
        assert_eq!(
            body,
            [
                b"agent:test:".as_slice(),
                CLAIM_RESOLVE_METHOD.as_bytes(),
                b":resolve",
            ]
            .concat()
        );
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

        client.close().await;
        router.shutdown().await.expect("router shutdown");
    }

    #[tokio::test]
    async fn missing_secret_never_reaches_verifier_or_handler() {
        let verifier = Arc::new(ExactSecretVerifier {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(EchoClaimHandler {
            calls: AtomicUsize::new(0),
        });
        let (router, client, address) =
            endpoints(Arc::clone(&verifier), Arc::clone(&handler)).await;

        let response = request(&client, address, CLAIM_ALPN_V1, context(b""))
            .await
            .expect("claim refusal response");
        let ResponseFrame::Failure(failure) =
            decode_response_frame(&response).expect("failure response")
        else {
            panic!("expected authentication failure");
        };
        assert_eq!(failure.code, CallFailureCode::Unauthenticated as i32);
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);

        client.close().await;
        router.shutdown().await.expect("router shutdown");
    }

    #[tokio::test]
    async fn rejected_secret_never_reaches_handler() {
        let verifier = Arc::new(ExactSecretVerifier {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(EchoClaimHandler {
            calls: AtomicUsize::new(0),
        });
        let (router, client, address) =
            endpoints(Arc::clone(&verifier), Arc::clone(&handler)).await;

        let response = request(&client, address, CLAIM_ALPN_V1, context(b"forged-secret"))
            .await
            .expect("claim refusal response");
        let ResponseFrame::Failure(failure) =
            decode_response_frame(&response).expect("failure response")
        else {
            panic!("expected authentication failure");
        };
        assert_eq!(failure.code, CallFailureCode::Unauthenticated as i32);
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);

        client.close().await;
        router.shutdown().await.expect("router shutdown");
    }

    #[tokio::test]
    async fn unrelated_alpn_is_rejected_before_dispatch() {
        let verifier = Arc::new(ExactSecretVerifier {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(EchoClaimHandler {
            calls: AtomicUsize::new(0),
        });
        let (router, client, address) =
            endpoints(Arc::clone(&verifier), Arc::clone(&handler)).await;

        let error = client
            .connect(address, b"not-heddle-claim")
            .await
            .expect_err("wrong ALPN must be rejected");
        assert!(!error.to_string().is_empty());
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);

        client.close().await;
        router.shutdown().await.expect("router shutdown");
    }
}
