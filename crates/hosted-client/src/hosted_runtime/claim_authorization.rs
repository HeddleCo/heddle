//! Native device account-claim admission. Every call requires the one-time
//! claim-link secret and exact request possession by the preparing browser key.
#![allow(clippy::result_large_err)]
use anyhow::{Context as _, bail};
use api::{
    heddle::api::{
        v1alpha1::{CallContext, CallFailure, CallFailureCode},
        v2alpha1::{PrepareAccountClaimRequest, SignAccountClaimRequest},
    },
    v2::client::Rpc as _,
};
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;

use super::{
    auth::headless_token_metadata,
    hosted::claim_protocol::{
        CLAIM_PREPARE_METHOD, CLAIM_SIGN_METHOD, ClaimHandler, ClaimSecretVerifier,
        VerifiedClaimPrincipal,
    },
    identity_state::{self, ClaimState},
    root_mint::is_local_agent_root,
};
#[derive(Clone, Debug)]
pub(crate) struct StoredClaimAuthorization {
    completion: tokio::sync::watch::Sender<bool>,
    owner_root_calls: tokio::sync::mpsc::Sender<ClaimOwnerRootCall>,
}
impl StoredClaimAuthorization {
    pub(crate) fn new() -> (
        Self,
        tokio::sync::watch::Receiver<bool>,
        tokio::sync::mpsc::Receiver<ClaimOwnerRootCall>,
    ) {
        let (completion, receiver) = tokio::sync::watch::channel(false);
        let (owner_root_calls, calls) = tokio::sync::mpsc::channel(1);
        (
            Self {
                completion,
                owner_root_calls,
            },
            receiver,
            calls,
        )
    }
}
#[derive(Debug)]
pub(crate) struct ClaimOwnerRootCall {
    principal: VerifiedClaimPrincipal,
    method: String,
    body: Vec<u8>,
    response: tokio::sync::oneshot::Sender<Result<Vec<u8>, CallFailure>>,
}
impl ClaimOwnerRootCall {
    pub(crate) fn principal(&self) -> &VerifiedClaimPrincipal {
        &self.principal
    }
    pub(crate) fn method(&self) -> &str {
        &self.method
    }
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
    pub(crate) fn respond(self, response: Result<Vec<u8>, CallFailure>) {
        let _ = self.response.send(response);
    }
}

impl ClaimSecretVerifier for StoredClaimAuthorization {
    async fn verify(
        &self,
        method: &str,
        context: &CallContext,
        body: &[u8],
    ) -> Result<VerifiedClaimPrincipal, CallFailure> {
        let (secret, browser_key, operation, descriptor) = match method {
            CLAIM_PREPARE_METHOD => {
                let request = PrepareAccountClaimRequest::decode(body).map_err(invalid)?;
                (
                    request.claim_authorization,
                    request.browser_public_key,
                    request.client_operation_id,
                    thread_api::rpc::OwnerAuthorizationServicePrepareAccountClaim::METHOD,
                )
            }
            CLAIM_SIGN_METHOD => {
                let request = SignAccountClaimRequest::decode(body).map_err(invalid)?;
                let key = request
                    .registration
                    .as_ref()
                    .ok_or_else(|| invalid("missing registration"))?
                    .caller_public_key
                    .clone();
                (
                    request.claim_authorization,
                    key,
                    request.client_operation_id,
                    thread_api::rpc::OwnerAuthorizationServiceSignAccountClaim::METHOD,
                )
            }
            _ => {
                return Err(failure(
                    CallFailureCode::Unimplemented,
                    "unknown native claim method",
                ));
            }
        };
        uuid::Uuid::parse_str(&operation).map_err(invalid)?;
        let key: [u8; 32] = browser_key
            .as_slice()
            .try_into()
            .map_err(|_| invalid("browser proof key must contain 32 bytes"))?;
        let now = chrono::Utc::now().timestamp_millis();
        thread_api::request_proof::verify(context, descriptor, body, &key, now).map_err(|_| {
            failure(
                CallFailureCode::Unauthenticated,
                "browser request possession required",
            )
        })?;
        let state = identity_state::load()
            .map_err(internal)?
            .ok_or_else(unauthorized)?;
        if !(state.accepts(&secret, now) || state.accepts_consent_retry(&secret, now)) {
            return Err(unauthorized());
        }
        Ok(VerifiedClaimPrincipal {
            subject: state.owner_id.to_string(),
            authorization_hash: state.authorization_hash().into(),
            browser_public_key: browser_key,
        })
    }
}
impl ClaimHandler for StoredClaimAuthorization {
    async fn call(
        &self,
        method: &str,
        principal: VerifiedClaimPrincipal,
        body: &[u8],
    ) -> Result<Vec<u8>, CallFailure> {
        let state = identity_state::load()
            .map_err(internal)?
            .ok_or_else(unauthorized)?;
        if state.owner_id.to_string() != principal.subject
            || state.authorization_hash() != principal.authorization_hash
            || !state.consent_unexpired(chrono::Utc::now().timestamp_millis())
        {
            return Err(unauthorized());
        }
        let operation = match method {
            CLAIM_PREPARE_METHOD => {
                PrepareAccountClaimRequest::decode(body)
                    .map_err(invalid)?
                    .client_operation_id
            }
            CLAIM_SIGN_METHOD => {
                SignAccountClaimRequest::decode(body)
                    .map_err(invalid)?
                    .client_operation_id
            }
            _ => return Err(invalid("unknown claim method")),
        };
        if let Some(response) = state
            .cached_command(method, &operation, body)
            .map_err(invalid)?
        {
            return Ok(response);
        }
        if !state.is_active(chrono::Utc::now().timestamp_millis()) {
            return Err(unauthorized());
        }
        let (response, receive) = tokio::sync::oneshot::channel();
        self.owner_root_calls
            .send(ClaimOwnerRootCall {
                principal,
                method: method.into(),
                body: body.to_vec(),
                response,
            })
            .await
            .map_err(|_| internal("foreground claim signer unavailable"))?;
        receive
            .await
            .map_err(|_| internal("foreground claim signer stopped"))?
    }
    async fn response_delivered(&self, method: &str, _body: &[u8]) {
        if method == CLAIM_SIGN_METHOD {
            self.completion.send_replace(true);
        }
    }
}
fn failure(code: CallFailureCode, message: impl Into<String>) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: message.into(),
        error: None,
    }
}
fn invalid(error: impl std::fmt::Display) -> CallFailure {
    failure(CallFailureCode::InvalidArgument, error.to_string())
}
fn internal(error: impl std::fmt::Display) -> CallFailure {
    failure(CallFailureCode::Internal, error.to_string())
}
fn unauthorized() -> CallFailure {
    failure(
        CallFailureCode::Unauthenticated,
        "claim authorization unavailable or expired",
    )
}

pub(crate) fn validate_stored_claim_signer(state: &ClaimState) -> anyhow::Result<()> {
    stored_claim_signer(state).map(|_| ())
}

fn stored_claim_signer(state: &ClaimState) -> anyhow::Result<Ed25519Signer> {
    let store = config::credentials::load_credentials()?;
    let credential = store
        .servers
        .get(&state.server)
        .with_context(|| format!("no agent credential is stored for {}", state.server))?;
    let metadata = headless_token_metadata(&credential.token)
        .context("reading the stored agent credential")?;
    if metadata.subject != state.subject
        || !(metadata.is_derived
            || is_local_agent_root(&metadata.subject, &metadata.proof_public_key_hex))
    {
        bail!("the stored credential is not the agent root recorded by this claim account");
    }
    let pem = credential
        .private_key_pem
        .as_deref()
        .context("the stored agent credential has no consent-signing key")?;
    let signer = Ed25519Signer::from_pem(pem).context("loading the agent consent-signing key")?;
    if hex::encode(signer.public_key()) != state.node_id
        || !metadata
            .proof_public_key_hex
            .eq_ignore_ascii_case(&state.node_id)
    {
        bail!("the stored agent credential does not match agent-node-identity.toml");
    }
    Ok(signer)
}
