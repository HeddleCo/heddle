//! Inbound Iroh router for direct device operations and account-claim calls.
//!
//! Uses the shared framed native RPC transport. Public endpoint discovery is
//! separated from claim methods, which require exact browser proof and an
//! expiring local claim secret before foreground signing. Device operations use
//! independently admitted owner authority and shared checkout/run stores.

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

/// Unauthenticated input and short commands share a small admission budget.
/// Only a successfully authenticated stream can exchange its admission slot
/// for a retained slot. Both permits are released on cancellation or failure.
#[derive(Debug)]
pub(crate) struct CallBudget {
    admission: Option<tokio::sync::OwnedSemaphorePermit>,
    retained: Option<tokio::sync::OwnedSemaphorePermit>,
    streams: Arc<tokio::sync::Semaphore>,
}
impl CallBudget {
    pub(crate) fn retain(&mut self) -> Result<(), &'static str> {
        if self.retained.is_none() {
            let permit = Arc::clone(&self.streams)
                .try_acquire_owned()
                .map_err(|_| "device retained stream capacity exhausted")?;
            self.retained = Some(permit);
            drop(self.admission.take());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClaimProtocol<V, H> {
    verifier: Arc<V>,
    handler: Arc<H>,
    endpoint_key: [u8; 32],
    permits: Arc<tokio::sync::Semaphore>,
    stream_permits: Arc<tokio::sync::Semaphore>,
    device: Option<Arc<super::super::device_rpc::DeviceRpc>>,
}

impl<V, H> ClaimProtocol<V, H> {
    pub(crate) fn with_device(mut self, device: Arc<super::super::device_rpc::DeviceRpc>) -> Self {
        self.device = Some(device);
        self
    }
    #[cfg(test)]
    pub(crate) fn budgets(&self) -> (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>) {
        (Arc::clone(&self.permits), Arc::clone(&self.stream_permits))
    }
    pub(crate) fn new(verifier: Arc<V>, handler: Arc<H>, endpoint_key: [u8; 32]) -> Self {
        Self {
            verifier,
            handler,
            device: None,
            endpoint_key,
            permits: Arc::new(tokio::sync::Semaphore::new(32)),
            stream_permits: Arc::new(tokio::sync::Semaphore::new(2048)),
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
                    let mut budget = CallBudget {
                        admission: Some(permit),
                        retained: None,
                        streams: Arc::clone(&self.stream_permits),
                    };
                    let endpoint_key = self.endpoint_key;
                    let peer_key = *connection.remote_id().as_bytes();
                    let device = self.device.clone();
                    let verifier = Arc::clone(&self.verifier);
                    let handler = Arc::clone(&self.handler);
                    calls.spawn(async move {
                        {
                            handle_call(verifier.as_ref(), handler.as_ref(), endpoint_key, device.as_deref(), peer_key, send, recv, &mut budget).await
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

#[allow(clippy::too_many_arguments)]
async fn handle_call<V, H>(
    verifier: &V,
    handler: &H,
    endpoint_key: [u8; 32],
    device: Option<&super::super::device_rpc::DeviceRpc>,
    peer_key: [u8; 32],
    mut send: SendStream,
    mut recv: RecvStream,
    budget: &mut CallBudget,
) -> Result<(), ClaimProtocolError>
where
    V: ClaimSecretVerifier,
    H: ClaimHandler,
{
    // Read the bounded routing prelude without waiting for FIN: native bidi
    // input remains open while responses are being delivered.
    let mut request = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut header = [0u8; 6];
        recv.read_exact(&mut header)
            .await
            .map_err(ClaimProtocolError::transport)?;
        let method_len = u16::from_be_bytes([header[0], header[1]]) as usize;
        let context_len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
        if method_len == 0 || method_len > MAX_METHOD_PATH || context_len > MAX_CALL_CONTEXT {
            return Err(ClaimProtocolError::transport(
                "request prelude exceeds budget",
            ));
        }
        let mut prelude = vec![0; 6 + method_len + context_len];
        prelude[..6].copy_from_slice(&header);
        recv.read_exact(&mut prelude[6..])
            .await
            .map_err(ClaimProtocolError::transport)?;
        Ok(prelude)
    })
    .await
    .map_err(ClaimProtocolError::transport)??;
    let (prelude, _) = api::framing::decode_request_prelude(&request)
        .map_err(ClaimProtocolError::transport)?
        .ok_or_else(|| ClaimProtocolError::transport("incomplete request prelude"))?;
    if let Some(device) = device
        && super::super::device_rpc::STREAM_METHODS.contains(&prelude.method)
    {
        return device
            .serve_stream(
                prelude.method,
                &prelude.context,
                peer_key,
                send,
                recv,
                budget,
            )
            .await
            .map_err(ClaimProtocolError::transport);
    }
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        recv.read_to_end(MAX_REQUEST_BODY + 1),
    )
    .await
    .map_err(ClaimProtocolError::transport)?
    .map_err(ClaimProtocolError::transport)?;
    request.extend(body);
    let mut successful_call = None;
    let response = match decode_request_frame(&request) {
        Ok(frame) if frame.body.len() > MAX_REQUEST_BODY => Err(failure(
            CallFailureCode::InvalidArgument,
            "claim request exceeds the body budget",
        )),
        Ok(frame) if frame.method == DESCRIBE_METHOD => {
            describe(endpoint_key, frame.body, device.is_some())
        }
        Ok(frame)
            if device.is_some() && super::super::device_rpc::METHODS.contains(&frame.method) =>
        {
            if let Some(device) = device {
                return device
                    .serve(frame.method, &frame.context, frame.body, send, budget)
                    .await
                    .map_err(ClaimProtocolError::transport);
            }
            return Err(ClaimProtocolError::transport(
                "device configuration changed",
            ));
        }
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

fn describe(endpoint_key: [u8; 32], body: &[u8], device: bool) -> Result<Vec<u8>, CallFailure> {
    use api::heddle::api::v2alpha1::*;
    use prost::Message;
    DescribeEndpointRequest::decode(body)
        .map_err(|_| failure(CallFailureCode::InvalidArgument, "invalid endpoint request"))?;
    let mut description = DescribeEndpointResponse {
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
    if device {
        description.implemented_methods.extend(
            super::super::device_rpc::METHODS
                .iter()
                .map(|method| (*method).to_owned()),
        );
    }
    encode_success_response(&description.encode_to_vec()).map_err(|_| {
        failure(
            CallFailureCode::Internal,
            "cannot encode endpoint description",
        )
    })
}

#[cfg(test)]
mod budget_tests {
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    use super::CallBudget;

    #[test]
    fn retained_capacity_and_failed_promotion_release_exact_permits() {
        let admission = Arc::new(Semaphore::new(2));
        let streams = Arc::new(Semaphore::new(1));
        let make = || CallBudget {
            admission: Some(
                Arc::clone(&admission)
                    .try_acquire_owned()
                    .expect("admission"),
            ),
            retained: None,
            streams: Arc::clone(&streams),
        };
        let mut first = make();
        assert_eq!(
            admission.available_permits(),
            1,
            "pre-auth admission remains bounded"
        );
        first.retain().expect("first retained stream");
        first.retain().expect("promotion idempotent");
        assert_eq!(admission.available_permits(), 2);
        assert_eq!(streams.available_permits(), 0);
        let mut second = make();
        assert_eq!(
            second.retain(),
            Err("device retained stream capacity exhausted")
        );
        assert_eq!(
            admission.available_permits(),
            1,
            "failed promotion keeps request bound until it returns"
        );
        drop(second);
        assert_eq!(admission.available_permits(), 2);
        drop(first);
        assert_eq!(
            streams.available_permits(),
            1,
            "retained slot released exactly once"
        );
    }
}
