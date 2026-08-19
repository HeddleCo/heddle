//! Data-only browser claim resolution and two-phase promotion consent.

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

const PRE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-pre-consent-v1";
const PROMOTE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-promote-consent-v1";

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
        if state.owner_id.to_string() != principal.subject
            || state.authorization_hash() != principal.authorization_hash
            || !state.is_active(chrono::Utc::now().timestamp_millis())
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConsent {
    pub(crate) account_id: String,
    pub(crate) node_id: String,
    pub(crate) signature: String,
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
    Ok(AgentConsent {
        account_id: state.owner_id.to_string(),
        node_id: state.node_id.clone(),
        signature,
    })
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
    Ok(AgentConsent {
        account_id: state.owner_id.to_string(),
        node_id: state.node_id.clone(),
        signature,
    })
}

pub(crate) fn pre_consent_message(
    state: &ClaimState,
    handle: &str,
    nonce: &[u8],
) -> Result<Vec<u8>, CallFailure> {
    let owner_id = state.owner_id.to_string();
    let expires_at = consent_expiry_bytes(state)?;
    encode_counted(&[
        PRE_CONSENT_DOMAIN,
        owner_id.as_bytes(),
        handle.as_bytes(),
        state.node_id.as_bytes(),
        nonce,
        claim_state_id(state)?,
        &expires_at,
    ])
}

pub(crate) fn promote_consent_message(
    state: &ClaimState,
    handle: &str,
    credential_id: &str,
) -> Result<Vec<u8>, CallFailure> {
    let owner_id = state.owner_id.to_string();
    let expires_at = consent_expiry_bytes(state)?;
    encode_counted(&[
        PROMOTE_CONSENT_DOMAIN,
        owner_id.as_bytes(),
        handle.as_bytes(),
        credential_id.as_bytes(),
        claim_state_id(state)?,
        &expires_at,
    ])
}

fn refuse_stale_consent(state: &ClaimState) -> Result<(), CallFailure> {
    if state.consent_unexpired(chrono::Utc::now().timestamp_millis()) {
        return Ok(());
    }
    Err(failure(
        CallFailureCode::Unauthenticated,
        "claim consent has expired",
    ))
}

fn claim_state_id(state: &ClaimState) -> Result<&[u8], CallFailure> {
    let id = state.authorization_hash().as_bytes();
    if id.is_empty() {
        return Err(invalid_failure("claim state is missing an issuance id"));
    }
    Ok(id)
}

fn consent_expiry_bytes(state: &ClaimState) -> Result<[u8; 8], CallFailure> {
    if state.expires_at_millis <= 0 {
        return Err(invalid_failure("claim consent is missing an expiry"));
    }
    Ok(state.expires_at_millis.to_be_bytes())
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
