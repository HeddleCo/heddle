//! Data-only browser claim resolution and two-phase promotion consent.

// The transport contract owns `CallFailure` by value at this seam.
#![allow(clippy::result_large_err)]

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{
    AuthChallengeResponse, CallContext, CallFailure, CallFailureCode, RegisterPublicKeyRequest,
    SignedOwnerKeyTransition, SignedOwnerRoot,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;
use serde::{Deserialize, Serialize};

use super::{
    auth::headless_token_metadata,
    hosted::claim_protocol::{
        CLAIM_CONSENT_METHOD, CLAIM_OWNER_ROOT_METHOD, CLAIM_RESOLVE_METHOD, ClaimHandler,
        ClaimSecretVerifier, VerifiedClaimPrincipal,
    },
    identity_state::{self, ClaimState},
    owner_root::BrowserClaimDeferredHuman,
    root_mint::is_local_agent_root,
};

const PRE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-pre-consent-v1";
const PROMOTE_CONSENT_DOMAIN: &[u8] = b"heddle-agent-promote-consent-v1";

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
pub(crate) struct ClaimOwnerRootOperation {
    resolve_handle: Option<String>,
    registration: Option<RegisterPublicKeyRequest>,
    browser_claim: Option<BrowserClaimDeferredHuman>,
}

pub(crate) enum ClaimOwnerRootOperationRef<'a> {
    Resolve(&'a str),
    CoSign {
        registration: &'a RegisterPublicKeyRequest,
        browser_claim: &'a BrowserClaimDeferredHuman,
    },
}

impl ClaimOwnerRootOperation {
    fn resolve(handle: String) -> Self {
        Self {
            resolve_handle: Some(handle),
            registration: None,
            browser_claim: None,
        }
    }

    fn co_sign(
        registration: RegisterPublicKeyRequest,
        browser_claim: BrowserClaimDeferredHuman,
    ) -> Self {
        Self {
            resolve_handle: None,
            registration: Some(registration),
            browser_claim: Some(browser_claim),
        }
    }

    pub(crate) fn as_ref(&self) -> Option<ClaimOwnerRootOperationRef<'_>> {
        match (
            self.resolve_handle.as_deref(),
            self.registration.as_ref(),
            self.browser_claim.as_ref(),
        ) {
            (Some(handle), None, None) => Some(ClaimOwnerRootOperationRef::Resolve(handle)),
            (None, Some(registration), Some(browser_claim)) => {
                Some(ClaimOwnerRootOperationRef::CoSign {
                    registration,
                    browser_claim,
                })
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClaimOwnerRootResult {
    signed_owner_root: Option<SignedOwnerRoot>,
    webauthn_challenge: Option<AuthChallengeResponse>,
    signed_transition: Option<SignedOwnerKeyTransition>,
}

enum ClaimOwnerRootResultRef<'a> {
    Resolved {
        signed_owner_root: &'a SignedOwnerRoot,
        webauthn_challenge: &'a AuthChallengeResponse,
    },
    CoSigned(&'a SignedOwnerKeyTransition),
}

impl ClaimOwnerRootResult {
    pub(crate) fn resolved(
        signed_owner_root: SignedOwnerRoot,
        webauthn_challenge: AuthChallengeResponse,
    ) -> Self {
        Self {
            signed_owner_root: Some(signed_owner_root),
            webauthn_challenge: Some(webauthn_challenge),
            signed_transition: None,
        }
    }

    pub(crate) fn co_signed(signed_transition: SignedOwnerKeyTransition) -> Self {
        Self {
            signed_owner_root: None,
            webauthn_challenge: None,
            signed_transition: Some(signed_transition),
        }
    }

    fn as_ref(&self) -> Option<ClaimOwnerRootResultRef<'_>> {
        match (
            self.signed_owner_root.as_ref(),
            self.webauthn_challenge.as_ref(),
            self.signed_transition.as_ref(),
        ) {
            (Some(signed_owner_root), Some(webauthn_challenge), None) => {
                Some(ClaimOwnerRootResultRef::Resolved {
                    signed_owner_root,
                    webauthn_challenge,
                })
            }
            (None, None, Some(signed_transition)) => {
                Some(ClaimOwnerRootResultRef::CoSigned(signed_transition))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClaimOwnerRootCall {
    pub(crate) principal: VerifiedClaimPrincipal,
    pub(crate) operation: ClaimOwnerRootOperation,
    response: tokio::sync::oneshot::Sender<Result<ClaimOwnerRootResult, CallFailure>>,
}

impl ClaimOwnerRootCall {
    pub(crate) fn respond(self, response: Result<ClaimOwnerRootResult, CallFailure>) {
        let _ = self.response.send(response);
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
        if method == CLAIM_OWNER_ROOT_METHOD {
            return self.owner_root_reply(principal, body).await;
        }
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
        if method == CLAIM_OWNER_ROOT_METHOD
            && matches!(parse_request(body), Ok(ClaimRequest::ClaimOwnerRoot { .. }))
        {
            self.completion.send_replace(true);
        }
    }
}

impl StoredClaimAuthorization {
    async fn owner_root_reply(
        &self,
        principal: VerifiedClaimPrincipal,
        body: &[u8],
    ) -> Result<Vec<u8>, CallFailure> {
        let operation = {
            let _guard = identity_state::write_lock().map_err(internal_failure)?;
            let state = identity_state::load_while_locked()
                .map_err(internal_failure)?
                .ok_or_else(auth_failure)?;
            let now = chrono::Utc::now().timestamp_millis();
            if state.owner_id.to_string() != principal.subject
                || state.authorization_hash() != principal.authorization_hash
                || !state.is_active(now)
            {
                return Err(auth_failure());
            }
            owner_root_operation(body)?
        };
        let (response, receive) = tokio::sync::oneshot::channel();
        self.owner_root_calls
            .send(ClaimOwnerRootCall {
                principal,
                operation,
                response,
            })
            .await
            .map_err(|_| internal_failure("claim owner-root session is unavailable"))?;
        let result = receive
            .await
            .map_err(|_| internal_failure("claim owner-root session stopped"))??;
        let reply = match result
            .as_ref()
            .ok_or_else(|| internal_failure("claim owner-root result has an invalid shape"))?
        {
            ClaimOwnerRootResultRef::Resolved {
                signed_owner_root,
                webauthn_challenge,
            } => ClaimReply::OwnerRootResolved {
                signed_owner_root: encode_message(signed_owner_root),
                webauthn_challenge: encode_message(webauthn_challenge),
            },
            ClaimOwnerRootResultRef::CoSigned(signed_transition) => ClaimReply::OwnerRootCoSigned {
                signed_transition: encode_message(signed_transition),
            },
        };
        serde_json::to_vec(&reply).map_err(internal_failure)
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
    ResolveOwnerRoot {
        handle: String,
    },
    PreConsent {
        handle: String,
        nonce: String,
    },
    PromoteConsent {
        handle: String,
        credential_id: String,
    },
    ClaimOwnerRoot {
        registration: String,
        next_authority_key: String,
        next_authority_key_proof: String,
        next_recovery_policy: String,
        next_recovery_key_proofs: Vec<String>,
        valid_from_unix_seconds: i64,
        nonce: String,
    },
}

#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum ClaimReply {
    Resolved {
        agent: AgentAccountSummary,
    },
    PreConsented {
        consent: AgentConsent,
    },
    PromoteConsented {
        consent: AgentConsent,
    },
    OwnerRootResolved {
        signed_owner_root: String,
        webauthn_challenge: String,
    },
    OwnerRootCoSigned {
        signed_transition: String,
    },
    Refused {
        refusal: &'static str,
    },
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
        ClaimRequest::Resolve
        | ClaimRequest::ResolveOwnerRoot { .. }
        | ClaimRequest::ClaimOwnerRoot { .. } => {
            Err(invalid_failure("consent body has the wrong kind"))
        }
    }
}

fn owner_root_operation(body: &[u8]) -> Result<ClaimOwnerRootOperation, CallFailure> {
    match parse_request(body)? {
        ClaimRequest::ResolveOwnerRoot { handle } => {
            validate_handle(&handle)?;
            Ok(ClaimOwnerRootOperation::resolve(handle))
        }
        ClaimRequest::ClaimOwnerRoot {
            registration,
            next_authority_key,
            next_authority_key_proof,
            next_recovery_policy,
            next_recovery_key_proofs,
            valid_from_unix_seconds,
            nonce,
        } => Ok(ClaimOwnerRootOperation::co_sign(
            decode_message(&registration, "RegisterPublicKey request")?,
            BrowserClaimDeferredHuman {
                next_authority_key: decode_message(&next_authority_key, "next authority key")?,
                next_authority_key_proof: decode_message(
                    &next_authority_key_proof,
                    "next authority key proof",
                )?,
                next_recovery_policy: decode_message(
                    &next_recovery_policy,
                    "next recovery policy",
                )?,
                next_recovery_key_proofs: next_recovery_key_proofs
                    .iter()
                    .map(|proof| decode_message(proof, "next recovery key proof"))
                    .collect::<Result<_, _>>()?,
                valid_from_unix_seconds,
                nonce: decode_fixed(&nonce, "claim transition nonce")?,
            },
        )),
        ClaimRequest::Resolve
        | ClaimRequest::PreConsent { .. }
        | ClaimRequest::PromoteConsent { .. } => {
            Err(invalid_failure("owner-root body has the wrong kind"))
        }
    }
}

fn encode_message(message: &impl Message) -> String {
    URL_SAFE_NO_PAD.encode(message.encode_to_vec())
}

fn decode_message<M: Message + Default>(
    encoded: &str,
    field: &'static str,
) -> Result<M, CallFailure> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_failure(&format!("invalid {field}")))?;
    M::decode(bytes.as_slice()).map_err(|_| invalid_failure(&format!("invalid {field}")))
}

fn decode_fixed<const N: usize>(
    encoded: &str,
    field: &'static str,
) -> Result<[u8; N], CallFailure> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_failure(&format!("invalid {field}")))?
        .try_into()
        .map_err(|_| invalid_failure(&format!("{field} must contain {N} bytes")))
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

/// Pins the published `heddle-claim/1` JSON contract to these private serde
/// enums. The golden vectors and JSON Schema under `contracts/` are the source
/// of truth browser/TS clients consume; if a field name, `#[serde(rename)]`,
/// the `kind` tag, or the camelCase casing here ever drifts from what is
/// published, one of these assertions fails and the build goes red.
///
/// See `crates/hosted-client/contracts/README.md`.
#[cfg(test)]
mod contract {
    use super::{
        AgentAccountSummary, AgentConsent, ClaimReply, ClaimRequest, parse_request,
    };
    use serde_json::{Value, json};

    const GOLDEN: &str = include_str!("../../contracts/heddle-claim-v1.golden.json");
    const SCHEMA: &str = include_str!("../../contracts/heddle-claim-v1.schema.json");

    fn golden() -> Value {
        serde_json::from_str(GOLDEN).expect("golden vectors parse as JSON")
    }

    fn sample_consent(signature: &str) -> AgentConsent {
        AgentConsent {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".to_string(),
            node_id: "11".repeat(32),
            signature: signature.to_string(),
            authorization_hash: "authorization-hash-abc123".to_string(),
            expires_at: 1_700_000_000_000,
        }
    }

    /// Every reply variant, serialized through the real enum, must equal its
    /// published golden vector — this is what catches a rename/casing/tag drift
    /// on the reply side that a hand-mirror would silently diverge on.
    #[test]
    fn reply_variants_match_published_golden() {
        let golden = golden();
        let replies = &golden["replies"];

        let cases: Vec<(&str, ClaimReply)> = vec![
            (
                "resolved",
                ClaimReply::Resolved {
                    agent: AgentAccountSummary {
                        account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".to_string(),
                        pet_name: "steady-heron".to_string(),
                        created_at: "2026-01-01T00:00:00Z".to_string(),
                        last_active_at: None,
                        spools: Vec::new(),
                        repos: Vec::new(),
                        change_count: None,
                        agent_label: None,
                    },
                },
            ),
            (
                "preConsented",
                ClaimReply::PreConsented {
                    consent: sample_consent("cHJlLWNvbnNlbnQtc2lnbmF0dXJl"),
                },
            ),
            (
                "promoteConsented",
                ClaimReply::PromoteConsented {
                    consent: sample_consent("cHJvbW90ZS1jb25zZW50LXNpZ25hdHVyZQ"),
                },
            ),
            (
                "ownerRootResolved",
                ClaimReply::OwnerRootResolved {
                    signed_owner_root: "c2lnbmVkLW93bmVyLXJvb3Q".to_string(),
                    webauthn_challenge: "d2ViYXV0aG4tY2hhbGxlbmdl".to_string(),
                },
            ),
            (
                "ownerRootCoSigned",
                ClaimReply::OwnerRootCoSigned {
                    signed_transition: "c2lnbmVkLXRyYW5zaXRpb24".to_string(),
                },
            ),
            ("refused", ClaimReply::Refused { refusal: "claimed" }),
        ];

        for (kind, reply) in cases {
            let produced = serde_json::to_value(&reply).expect("reply serializes");
            assert_eq!(
                produced["kind"], kind,
                "{kind} reply must carry its own internal tag"
            );
            assert_eq!(
                produced, replies[kind],
                "{kind} reply diverged from the published golden vector"
            );
        }

        // Every published reply vector is covered by a case above.
        let published: std::collections::BTreeSet<String> = replies
            .as_object()
            .expect("replies is an object")
            .keys()
            .cloned()
            .collect();
        let covered: std::collections::BTreeSet<String> = [
            "resolved",
            "preConsented",
            "promoteConsented",
            "ownerRootResolved",
            "ownerRootCoSigned",
            "refused",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            published, covered,
            "published reply vectors and the pinned cases must be the same set"
        );
    }

    /// Every request variant's published golden vector must parse through the
    /// real deserialize enum with the expected fields. `deny_unknown_fields`
    /// plus the internal tag mean a rename/casing drift makes the published
    /// vector fail to parse — turning this red.
    #[test]
    fn request_variants_match_published_golden() {
        let golden = golden();
        let requests = &golden["requests"];

        let bytes = |kind: &str| serde_json::to_vec(&requests[kind]).expect("request re-serializes");

        assert!(matches!(
            parse_request(&bytes("resolve")).expect("resolve parses"),
            ClaimRequest::Resolve
        ));

        match parse_request(&bytes("resolveOwnerRoot")).expect("resolveOwnerRoot parses") {
            ClaimRequest::ResolveOwnerRoot { handle } => assert_eq!(handle, "human-handle"),
            _ => panic!("resolveOwnerRoot parsed as the wrong variant"),
        }

        match parse_request(&bytes("preConsent")).expect("preConsent parses") {
            ClaimRequest::PreConsent { handle, nonce } => {
                assert_eq!(handle, "human-handle");
                assert_eq!(nonce, "MDEyMzQ1Njc4OWFiY2RlZg");
            }
            _ => panic!("preConsent parsed as the wrong variant"),
        }

        match parse_request(&bytes("promoteConsent")).expect("promoteConsent parses") {
            ClaimRequest::PromoteConsent {
                handle,
                credential_id,
            } => {
                assert_eq!(handle, "human-handle");
                assert_eq!(credential_id, "Y3JlZGVudGlhbA");
            }
            _ => panic!("promoteConsent parsed as the wrong variant"),
        }

        match parse_request(&bytes("claimOwnerRoot")).expect("claimOwnerRoot parses") {
            ClaimRequest::ClaimOwnerRoot {
                registration,
                next_authority_key,
                next_authority_key_proof,
                next_recovery_policy,
                next_recovery_key_proofs,
                valid_from_unix_seconds,
                nonce,
            } => {
                assert_eq!(registration, "cmVnaXN0cmF0aW9u");
                assert_eq!(next_authority_key, "bmV4dC1hdXRob3JpdHkta2V5");
                assert_eq!(next_authority_key_proof, "bmV4dC1hdXRob3JpdHkta2V5LXByb29m");
                assert_eq!(next_recovery_policy, "bmV4dC1yZWNvdmVyeS1wb2xpY3k");
                assert_eq!(next_recovery_key_proofs, vec!["cmVjb3Zlcnkta2V5LXByb29m"]);
                assert_eq!(valid_from_unix_seconds, 1);
                assert_eq!(nonce, "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI");
            }
            _ => panic!("claimOwnerRoot parsed as the wrong variant"),
        }

        // An unknown field must be rejected — proves the published vectors are
        // exhaustive, not merely a permissive subset.
        let mut poisoned = requests["preConsent"].clone();
        poisoned["unexpected"] = json!(true);
        assert!(
            parse_request(&serde_json::to_vec(&poisoned).unwrap()).is_err(),
            "requests must deny unknown fields"
        );
    }

    /// The hand-maintained JSON Schema must declare exactly the `kind`
    /// discriminants the golden vectors (and therefore the enums) carry, so a
    /// new/removed variant cannot land in the golden without the schema
    /// following.
    #[test]
    fn schema_declares_the_same_variants_as_the_golden() {
        let golden = golden();
        let schema: Value = serde_json::from_str(SCHEMA).expect("schema parses as JSON");

        let mut schema_kinds = std::collections::BTreeSet::new();
        for def in schema["$defs"]
            .as_object()
            .expect("schema $defs is an object")
            .values()
        {
            if let Some(kind) = def
                .get("properties")
                .and_then(|properties| properties.get("kind"))
                .and_then(|kind| kind.get("const"))
                .and_then(Value::as_str)
            {
                schema_kinds.insert(kind.to_string());
            }
        }

        let mut golden_kinds = std::collections::BTreeSet::new();
        for group in ["requests", "replies"] {
            for kind in golden[group]
                .as_object()
                .expect("golden group is an object")
                .keys()
            {
                golden_kinds.insert(kind.clone());
            }
        }

        assert_eq!(
            schema_kinds, golden_kinds,
            "the JSON Schema and golden vectors must declare the same variants"
        );
    }
}
