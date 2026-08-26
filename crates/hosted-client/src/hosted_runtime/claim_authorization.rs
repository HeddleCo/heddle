//! Data-only browser claim resolution and two-phase promotion consent.

// The transport contract owns `CallFailure` by value at this seam.
#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{CallContext, CallFailure, CallFailureCode};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto::{Ed25519Signer, Signer as _};
use serde::{Deserialize, Serialize};

use super::{
    auth::headless_token_metadata,
    hosted::claim_protocol::{
        CLAIM_CONSENT_METHOD, CLAIM_RESOLVE_METHOD, ClaimHandler, ClaimSecretVerifier,
        VerifiedClaimPrincipal,
    },
    identity_state::{self, ClaimState},
    root_mint::is_local_agent_root,
};

const PRE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-pre-consent-v1";
const PROMOTE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-promote-consent-v1";

#[derive(Clone, Debug)]
pub(crate) struct StoredClaimAuthorization {
    completion: tokio::sync::watch::Sender<bool>,
}

impl StoredClaimAuthorization {
    pub(crate) fn new() -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (completion, receiver) = tokio::sync::watch::channel(false);
        (Self { completion }, receiver)
    }
}

impl ClaimSecretVerifier for StoredClaimAuthorization {
    async fn verify(
        &self,
        method: &str,
        context: &CallContext,
        _body: &[u8],
    ) -> Result<VerifiedClaimPrincipal, CallFailure> {
        let state = identity_state::load().map_err(internal_failure)?;
        let Some(state) = state else {
            return Err(auth_failure());
        };
        let now = chrono::Utc::now().timestamp_millis();
        let authorized = state.accepts(&context.bearer_capability, now)
            || (method == CLAIM_RESOLVE_METHOD
                && state.accepts_claimed_resolve(&context.bearer_capability, now));
        if !authorized {
            return Err(auth_failure());
        }
        Ok(VerifiedClaimPrincipal {
            subject: state.owner_id.to_string(),
            authorization_hash: state.authorization_hash().to_string(),
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
        // Serialize the generation check and consent write. Without this
        // lock, two browser requests verified against the same one-time
        // secret could both load Active state before either consumed it.
        let _guard = identity_state::write_lock().map_err(internal_failure)?;
        let mut state = identity_state::load_while_locked()
            .map_err(internal_failure)?
            .ok_or_else(auth_failure)?;
        let now = chrono::Utc::now().timestamp_millis();
        let method_is_available = if method == CLAIM_RESOLVE_METHOD {
            state.is_active(now) || (state.is_claimed() && state.consent_unexpired(now))
        } else {
            state.is_active(now)
        };
        if state.owner_id.to_string() != principal.subject
            || state.authorization_hash() != principal.authorization_hash
            || !method_is_available
        {
            return Err(auth_failure());
        }
        match method {
            CLAIM_RESOLVE_METHOD => resolved_reply(&state, body),
            CLAIM_CONSENT_METHOD => {
                let signer = claim_signer(&state)?;
                let reply = consent_reply(&mut state, body, &signer)?;
                identity_state::store_while_locked(&state).map_err(internal_failure)?;
                Ok(reply)
            }
            _ => Err(failure(
                CallFailureCode::Unimplemented,
                "unknown claim method",
            )),
        }
    }

    async fn response_delivered(&self, method: &str, body: &[u8]) {
        if method == CLAIM_CONSENT_METHOD
            && matches!(parse_request(body), Ok(ClaimRequest::PromoteConsent { .. }))
        {
            self.completion.send_replace(true);
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ClaimRequest {
    Resolve,
    PreConsent {
        handle: String,
        nonce: String,
    },
    PromoteConsent {
        handle: String,
        credential_id: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ClaimReply {
    Resolved { agent: AgentAccountSummary },
    PreConsented { consent: AgentConsent },
    PromoteConsented { consent: AgentConsent },
    Refused { refusal: &'static str },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentAccountSummary {
    account_id: String,
    pet_name: String,
    created_at: String,
    last_active_at: Option<String>,
    spools: Vec<serde_json::Value>,
    repos: Vec<serde_json::Value>,
    change_count: Option<u64>,
    agent_label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConsent {
    pub(crate) account_id: String,
    pub(crate) node_id: String,
    pub(crate) signature: String,
    pub(crate) authorization_hash: String,
    pub(crate) expires_at: i64,
}

pub(crate) fn resolved_reply(state: &ClaimState, body: &[u8]) -> Result<Vec<u8>, CallFailure> {
    if !matches!(parse_request(body)?, ClaimRequest::Resolve) {
        return Err(invalid_failure("resolve body has the wrong kind"));
    }
    let reply = if state.is_claimed() {
        ClaimReply::Refused { refusal: "claimed" }
    } else {
        ClaimReply::Resolved {
            agent: AgentAccountSummary {
                account_id: state.owner_id.to_string(),
                pet_name: state.pet_name.clone(),
                created_at: state.created_at.clone(),
                last_active_at: None,
                spools: Vec::new(),
                repos: Vec::new(),
                change_count: None,
                agent_label: None,
            },
        }
    };
    serde_json::to_vec(&reply).map_err(internal_failure)
}

pub(crate) fn consent_reply(
    state: &mut ClaimState,
    body: &[u8],
    signer: &Ed25519Signer,
) -> Result<Vec<u8>, CallFailure> {
    if state.is_claimed() {
        return Err(failure(
            CallFailureCode::FailedPrecondition,
            "account claim is already complete",
        ));
    }
    if !state.consent_unexpired(chrono::Utc::now().timestamp_millis()) {
        return Err(failure(
            CallFailureCode::Unauthenticated,
            "claim consent has expired",
        ));
    }
    match parse_request(body)? {
        ClaimRequest::PreConsent { handle, nonce } => {
            validate_handle(&handle)?;
            let nonce = decode_nonce(&nonce)?;
            if !state.prepare(&handle, &nonce) {
                return Err(failure(
                    CallFailureCode::PermissionDenied,
                    "claim binding does not match the opened ceremony",
                ));
            }
            let consent = signed_pre_consent(state, &handle, &nonce, signer)?;
            serde_json::to_vec(&ClaimReply::PreConsented { consent }).map_err(internal_failure)
        }
        ClaimRequest::PromoteConsent {
            handle,
            credential_id,
        } => {
            validate_handle(&handle)?;
            validate_credential_id(&credential_id)?;
            if !state.claim(&handle) {
                return Err(failure(
                    CallFailureCode::FailedPrecondition,
                    "claim promotion does not match its pre-consent",
                ));
            }
            let consent = signed_promote_consent(state, &handle, &credential_id, signer)?;
            serde_json::to_vec(&ClaimReply::PromoteConsented { consent }).map_err(internal_failure)
        }
        ClaimRequest::Resolve => Err(invalid_failure("consent body has the wrong kind")),
    }
}

pub(crate) fn signed_pre_consent(
    state: &ClaimState,
    handle: &str,
    nonce: &[u8],
    signer: &Ed25519Signer,
) -> Result<AgentConsent, CallFailure> {
    refuse_stale_consent(state)?;
    let statement = pre_consent_message(state, handle, nonce)?;
    let signature = URL_SAFE_NO_PAD.encode(signer.sign(&statement).map_err(internal_failure)?);
    Ok(bound_consent(state, signature))
}

pub(crate) fn signed_promote_consent(
    state: &ClaimState,
    handle: &str,
    credential_id: &str,
    signer: &Ed25519Signer,
) -> Result<AgentConsent, CallFailure> {
    refuse_stale_consent(state)?;
    let statement = promote_consent_message(state, handle, credential_id)?;
    let signature = URL_SAFE_NO_PAD.encode(signer.sign(&statement).map_err(internal_failure)?);
    Ok(bound_consent(state, signature))
}

pub(crate) fn pre_consent_message(
    state: &ClaimState,
    handle: &str,
    nonce: &[u8],
) -> Result<Vec<u8>, CallFailure> {
    encode_pre_consent(&state.owner_id.to_string(), handle, &state.node_id, nonce)
}

pub(crate) fn promote_consent_message(
    state: &ClaimState,
    handle: &str,
    credential_id: &str,
) -> Result<Vec<u8>, CallFailure> {
    encode_promote_consent(&state.owner_id.to_string(), handle, credential_id)
}

/// Verifies a pre/promote pair from the consent payload, not local claim TTL.
///
/// Rejects when the locally bound `expiresAt` has elapsed, the pair is not the
/// same issuance, or either signature fails over weft's exact counted tuple.
/// The issuance hash/expiry remain local anti-replay metadata; weft's v1
/// signature domains intentionally do not include them.
#[cfg(test)]
pub(crate) fn verify_promotion_consents(
    agent_public_key: &[u8],
    handle: &str,
    nonce: &[u8],
    credential_id: &str,
    pre: &AgentConsent,
    promote: &AgentConsent,
    now_millis: i64,
) -> Result<(), CallFailure> {
    if pre.account_id != promote.account_id
        || pre.node_id != promote.node_id
        || pre.authorization_hash != promote.authorization_hash
        || pre.expires_at != promote.expires_at
    {
        return Err(failure(
            CallFailureCode::PermissionDenied,
            "claim consents are not a matching pair",
        ));
    }
    validate_consent_binding(&pre.authorization_hash, pre.expires_at)?;
    if now_millis >= pre.expires_at {
        return Err(failure(
            CallFailureCode::Unauthenticated,
            "claim consent has expired",
        ));
    }
    let pre_statement = encode_pre_consent(&pre.account_id, handle, &pre.node_id, nonce)?;
    let promote_statement = encode_promote_consent(&promote.account_id, handle, credential_id)?;
    verify_consent_signature(&pre_statement, agent_public_key, &pre.signature)?;
    verify_consent_signature(&promote_statement, agent_public_key, &promote.signature)?;
    Ok(())
}

fn refuse_stale_consent(state: &ClaimState) -> Result<(), CallFailure> {
    validate_consent_binding(state.authorization_hash(), state.expires_at_millis)?;
    if state.consent_unexpired(chrono::Utc::now().timestamp_millis()) {
        return Ok(());
    }
    Err(failure(
        CallFailureCode::Unauthenticated,
        "claim consent has expired",
    ))
}

fn bound_consent(state: &ClaimState, signature: String) -> AgentConsent {
    AgentConsent {
        account_id: state.owner_id.to_string(),
        node_id: state.node_id.clone(),
        signature,
        authorization_hash: state.authorization_hash().to_string(),
        expires_at: state.expires_at_millis,
    }
}

fn encode_pre_consent(
    account_id: &str,
    handle: &str,
    node_id: &str,
    nonce: &[u8],
) -> Result<Vec<u8>, CallFailure> {
    encode_counted(&[
        PRE_CONSENT_DOMAIN,
        account_id.as_bytes(),
        handle.as_bytes(),
        node_id.as_bytes(),
        nonce,
    ])
}

fn encode_promote_consent(
    account_id: &str,
    handle: &str,
    credential_id: &str,
) -> Result<Vec<u8>, CallFailure> {
    encode_counted(&[
        PROMOTE_CONSENT_DOMAIN,
        account_id.as_bytes(),
        handle.as_bytes(),
        credential_id.as_bytes(),
    ])
}

fn validate_consent_binding(
    authorization_hash: &str,
    expires_at_millis: i64,
) -> Result<(), CallFailure> {
    if authorization_hash.is_empty() || expires_at_millis <= 0 {
        return Err(failure(
            CallFailureCode::FailedPrecondition,
            "claim consent is not bound to an issuance",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn verify_consent_signature(
    statement: &[u8],
    agent_public_key: &[u8],
    signature: &str,
) -> Result<(), CallFailure> {
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| invalid_failure("invalid claim consent signature"))?;
    Ed25519Signer::verify_with_public_key(statement, agent_public_key, &signature).map_err(|_| {
        failure(
            CallFailureCode::Unauthenticated,
            "claim consent signature is invalid",
        )
    })
}

fn encode_counted(parts: &[&[u8]]) -> Result<Vec<u8>, CallFailure> {
    let mut encoded = Vec::new();
    for part in parts {
        let length = u32::try_from(part.len())
            .map_err(|_| invalid_failure("claim consent field is too large"))?;
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(part);
    }
    Ok(encoded)
}

fn claim_signer(state: &ClaimState) -> Result<Ed25519Signer, CallFailure> {
    stored_claim_signer(state).map_err(|error| {
        tracing::warn!(%error, "stored claim signer is unavailable");
        auth_failure()
    })
}

pub(crate) fn validate_stored_claim_signer(state: &ClaimState) -> Result<()> {
    stored_claim_signer(state).map(|_| ())
}

fn stored_claim_signer(state: &ClaimState) -> Result<Ed25519Signer> {
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

fn parse_request(body: &[u8]) -> Result<ClaimRequest, CallFailure> {
    serde_json::from_slice(body).map_err(|_| invalid_failure("invalid claim request body"))
}

fn decode_nonce(nonce: &str) -> Result<Vec<u8>, CallFailure> {
    let nonce = URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| invalid_failure("invalid claim nonce"))?;
    if !(16..=1024).contains(&nonce.len()) {
        return Err(invalid_failure(
            "claim nonce must contain between 16 and 1024 bytes",
        ));
    }
    Ok(nonce)
}

pub(crate) fn validate_handle(handle: &str) -> Result<(), CallFailure> {
    if !(3..=63).contains(&handle.len())
        || !handle
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || handle.starts_with('-')
        || handle.ends_with('-')
    {
        return Err(invalid_failure("invalid promotion handle"));
    }
    Ok(())
}

pub(crate) fn validate_credential_id(id: &str) -> Result<(), CallFailure> {
    if id.is_empty() || id.len() > 1024 || URL_SAFE_NO_PAD.decode(id).is_err() {
        return Err(invalid_failure("invalid WebAuthn credential id"));
    }
    Ok(())
}

fn auth_failure() -> CallFailure {
    failure(
        CallFailureCode::Unauthenticated,
        "claim authorization failed",
    )
}

fn invalid_failure(message: &str) -> CallFailure {
    failure(CallFailureCode::InvalidArgument, message)
}

fn internal_failure(error: impl std::fmt::Display) -> CallFailure {
    tracing::warn!(%error, "claim authorization failed internally");
    failure(CallFailureCode::Internal, "claim authorization failed")
}

fn failure(code: CallFailureCode, message: impl Into<String>) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: message.into(),
        error: None,
    }
}
