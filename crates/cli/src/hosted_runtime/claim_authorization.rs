//! Data-only browser claim resolution and promotion consent.

// The transport contract owns `CallFailure` by value at this seam.
#![allow(clippy::result_large_err)]

use anyhow::Result;
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
};

const CONSENT_TTL_MILLIS: i64 = 5 * 60 * 1_000;

#[derive(Clone, Debug)]
pub(crate) struct StoredClaimAuthorization;

impl ClaimSecretVerifier for StoredClaimAuthorization {
    async fn verify(
        &self,
        _method: &str,
        context: &CallContext,
        _body: &[u8],
    ) -> Result<VerifiedClaimPrincipal, CallFailure> {
        let state = identity_state::load().map_err(internal_failure)?;
        let Some(state) = state else {
            return Err(auth_failure());
        };
        if !state.accepts(
            &context.bearer_capability,
            chrono::Utc::now().timestamp_millis(),
        ) {
            return Err(auth_failure());
        }
        Ok(VerifiedClaimPrincipal {
            subject: state.account_id.clone(),
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
        let mut state = identity_state::load()
            .map_err(internal_failure)?
            .ok_or_else(auth_failure)?;
        if state.account_id != principal.subject
            || state.authorization_hash() != principal.authorization_hash
        {
            return Err(auth_failure());
        }
        match method {
            CLAIM_RESOLVE_METHOD => resolved_reply(&state, body),
            CLAIM_CONSENT_METHOD => consent_reply(&mut state, body),
            _ => Err(failure(
                CallFailureCode::Unimplemented,
                "unknown claim method",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ClaimRequest {
    Resolve,
    Consent {
        handle: String,
        credential_id: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ClaimReply {
    Resolved { agent: AgentAccountSummary },
    Consented { consent: AgentConsent },
    Refused { refusal: &'static str },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentAccountSummary {
    pet_name: String,
    created_at: String,
    last_active_at: Option<String>,
    spools: Vec<serde_json::Value>,
    repos: Vec<serde_json::Value>,
    change_count: Option<u64>,
    agent_label: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConsent {
    pub(crate) node_id: String,
    pub(crate) statement: String,
    pub(crate) signature: String,
    pub(crate) expires_at: i64,
}

pub(crate) fn resolved_reply(state: &ClaimState, body: &[u8]) -> Result<Vec<u8>, CallFailure> {
    if !matches!(parse_request(body)?, ClaimRequest::Resolve) {
        return Err(invalid_failure("resolve body has the wrong kind"));
    }
    let reply = if state.is_consented() {
        ClaimReply::Refused { refusal: "claimed" }
    } else {
        ClaimReply::Resolved {
            agent: AgentAccountSummary {
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

pub(crate) fn consent_reply(state: &mut ClaimState, body: &[u8]) -> Result<Vec<u8>, CallFailure> {
    let ClaimRequest::Consent {
        handle,
        credential_id,
    } = parse_request(body)?
    else {
        return Err(invalid_failure("consent body has the wrong kind"));
    };
    if state.is_consented() {
        return serde_json::to_vec(&ClaimReply::Refused { refusal: "claimed" })
            .map_err(internal_failure);
    }
    validate_handle(&handle)?;
    validate_credential_id(&credential_id)?;
    let now = chrono::Utc::now().timestamp_millis();
    let expires_at = (now + CONSENT_TTL_MILLIS).min(state.expires_at_millis);
    if expires_at <= now {
        return Err(auth_failure());
    }
    let signer = claim_signer(state)?;
    let consent = signed_consent(state, &handle, &credential_id, expires_at, &signer)?;
    state.mark_consented();
    identity_state::store(state).map_err(internal_failure)?;
    serde_json::to_vec(&ClaimReply::Consented { consent }).map_err(internal_failure)
}

pub(crate) fn signed_consent(
    state: &ClaimState,
    handle: &str,
    credential_id: &str,
    expires_at: i64,
    signer: &Ed25519Signer,
) -> Result<AgentConsent, CallFailure> {
    let statement = serde_json::json!({
        "accountId": state.account_id,
        "credentialId": credential_id,
        "expiresAt": expires_at,
        "handle": handle,
        "nodeId": state.node_id,
        "purpose": "heddle-agent-account-promotion-v1"
    })
    .to_string();
    let signature = URL_SAFE_NO_PAD.encode(
        signer
            .sign(statement.as_bytes())
            .map_err(internal_failure)?,
    );
    Ok(AgentConsent {
        node_id: state.node_id.clone(),
        statement,
        signature,
        expires_at,
    })
}

fn claim_signer(state: &ClaimState) -> Result<Ed25519Signer, CallFailure> {
    let store = cli_shared::credentials::load_credentials().map_err(internal_failure)?;
    let credential = store.servers.get(&state.server).ok_or_else(auth_failure)?;
    let metadata = headless_token_metadata(&credential.token).map_err(internal_failure)?;
    if !metadata.is_derived || metadata.subject != state.subject {
        return Err(auth_failure());
    }
    let pem = credential
        .private_key_pem
        .as_deref()
        .ok_or_else(auth_failure)?;
    let signer = Ed25519Signer::from_pem(pem).map_err(internal_failure)?;
    if hex::encode(signer.public_key()) != state.node_id
        || !metadata
            .proof_public_key_hex
            .eq_ignore_ascii_case(&state.node_id)
    {
        return Err(auth_failure());
    }
    Ok(signer)
}

fn parse_request(body: &[u8]) -> Result<ClaimRequest, CallFailure> {
    serde_json::from_slice(body).map_err(|_| invalid_failure("invalid claim request body"))
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
