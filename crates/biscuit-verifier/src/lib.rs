//! Transport-free Heddle capability verification.
//!
//! This crate is the single authorization implementation shared by native
//! Weft and the Cloudflare provider Worker. It deliberately owns no storage,
//! networking, runtime, environment, or root-minting concerns.
//! Existing capabilities may be narrowed offline using [`delegation`].

use std::time::Duration;

pub use biscuit_auth::PublicKey;
use biscuit_auth::{
    Biscuit, UnverifiedBiscuit,
    builder::{Algorithm, AuthorizerBuilder, BlockBuilder, Term},
    datalog::RunLimits,
};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use thiserror::Error;

pub mod delegation;
pub mod edge;
pub mod envelope;
pub mod facts;
pub mod resource;

#[cfg(test)]
mod grant_envelope_conformance_tests;

pub use facts::{ActorClaims, BiscuitFacts, Right};

/// Versioned domain for offline PoP-key delegation.
pub const POP_DELEGATION_DOMAIN: &[u8] = b"heddle-pop-delegation-v1\0";

pub const PRESENCE_TOKEN_TTL_SECS: i64 = 5 * 60;

/// Authority-block action required to submit a signed CI verdict.
pub const CI_VERDICT_WRITE_ACTION: &str = "ci-verdict:write";

/// Request-time operation fact used by the CI-verdict authorization gate.
pub const CI_VERDICT_WRITE_OPERATION: &str = "CiVerdictWrite";

/// Marker predicate carried by the exact server-signed presence block shape.
pub(crate) const PRESENCE_ATTENUATION_FACT: &str = "weft_presence_attenuation_v1";

/// Caller-bound observation exempted only from delegated work ceilings.
/// Hosts must apply `limits_identity_disclosure` before composing account data.
pub const SELF_OBSERVATION_OPERATION: &str = "ObserveIdentity";
const HEDDLE_RULES: &str = include_str!("rules.biscuit");

#[derive(Debug, Error)]
pub enum BiscuitError {
    #[error("biscuit signature/parse failed: {0}")]
    Invalid(String),
    #[error("biscuit authorization failed: {0}")]
    Authorization(String),
    #[error("biscuit internal error: {0}")]
    Internal(String),
    #[error("biscuit session is revoked")]
    Revoked,
    #[error("grant envelope invalid: {0}")]
    EnvelopeInvalid(String),
}

pub(crate) trait BiscuitResultExt<T> {
    fn internal_ctx(self, ctx: &'static str) -> Result<T, BiscuitError>;
    fn authz_ctx(self, ctx: &'static str) -> Result<T, BiscuitError>;
}

impl<T, E: std::fmt::Display> BiscuitResultExt<T> for Result<T, E> {
    fn internal_ctx(self, ctx: &'static str) -> Result<T, BiscuitError> {
        self.map_err(|error| BiscuitError::Internal(format!("{ctx}: {error}")))
    }

    fn authz_ctx(self, ctx: &'static str) -> Result<T, BiscuitError> {
        self.map_err(|error| BiscuitError::Authorization(format!("{ctx}: {error}")))
    }
}

pub(crate) fn pop_delegation_payload(
    parent_revocation_id: &[u8],
    child_public_key: &[u8],
) -> Vec<u8> {
    [
        POP_DELEGATION_DOMAIN,
        parent_revocation_id,
        child_public_key,
    ]
    .concat()
}

/// Construct the one exact trusted presence-block shape accepted by the
/// verifier. Native Weft also uses this when minting that block.
pub fn presence_attenuation_block(
    subject: &str,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<BlockBuilder, BiscuitError> {
    let issued_at = DateTime::from_timestamp(issued_at.timestamp(), 0)
        .ok_or_else(|| BiscuitError::Internal("presence issue time is invalid".to_string()))?;
    let expires_at = DateTime::from_timestamp(expires_at.timestamp(), 0)
        .ok_or_else(|| BiscuitError::Internal("presence expiry is invalid".to_string()))?;
    let ttl = expires_at - issued_at;
    if ttl <= chrono::Duration::zero() || ttl > chrono::Duration::seconds(PRESENCE_TOKEN_TTL_SECS) {
        return Err(BiscuitError::Internal(format!(
            "presence attenuation lifetime must be between 1 and {PRESENCE_TOKEN_TTL_SECS} seconds"
        )));
    }
    let marker = format!(
        "{PRESENCE_ATTENUATION_FACT}({}, {}, {})",
        biscuit_string(subject),
        issued_at.to_rfc3339(),
        expires_at.to_rfc3339(),
    );
    BlockBuilder::new()
        .fact(marker.as_str())
        .internal_ctx("add presence attenuation marker")?
        .fact(format!("agent({})", biscuit_string(&format!("presence:{subject}"))).as_str())
        .internal_ctx("add presence audit agent")?
        .check(format!("check if time($now), $now < {}", expires_at.to_rfc3339()).as_str())
        .internal_ctx("add presence expiry check")?
        .check(
            format!(r#"check if operation("{SELF_OBSERVATION_OPERATION}") or operation($op), $op == "presence""#)
                .as_str(),
        )
        .internal_ctx("add presence operation check")
}

/// Verify a token signature against a bounded root-key rotation list.
pub fn parse_token(token_b64: &str, trust_list: &[PublicKey]) -> Result<Biscuit, BiscuitError> {
    parse_token_with_root(token_b64, trust_list).map(|(biscuit, _)| biscuit)
}

/// Read the sole `device_pop_key($hex)` selector from unverified authority
/// block 0. This value chooses a candidate key only; [`parse_token_with_root`]
/// must still verify the Biscuit signature with that key before any fact is
/// trusted.
pub fn unverified_authority_device_pop_key(
    token_b64: &str,
) -> Result<Option<PublicKey>, BiscuitError> {
    let token = UnverifiedBiscuit::from_base64(token_b64.as_bytes())
        .map_err(|error| BiscuitError::Invalid(error.to_string()))?;
    let source = token
        .print_block_source(0)
        .map_err(|error| BiscuitError::Invalid(error.to_string()))?;
    let block = BlockBuilder::new()
        .code(&source)
        .map_err(|error| BiscuitError::Invalid(error.to_string()))?;
    let selectors = block
        .facts
        .iter()
        .filter_map(|fact| {
            match (
                fact.predicate.name.as_str(),
                fact.predicate.terms.as_slice(),
            ) {
                ("device_pop_key", [Term::Str(value)]) => Some(value.as_str()),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    let selector = match selectors.as_slice() {
        [] => return Ok(None),
        [selector] => *selector,
        _ => {
            return Err(BiscuitError::Invalid(
                "authority block must contain at most one device_pop_key fact".to_string(),
            ));
        }
    };
    let raw = hex::decode(selector)
        .map_err(|_| BiscuitError::Invalid("device_pop_key is not valid hex".to_string()))?;
    if raw.len() != 32 {
        return Err(BiscuitError::Invalid(
            "device_pop_key must encode exactly 32 bytes".to_string(),
        ));
    }
    PublicKey::from_bytes(&raw, Algorithm::Ed25519)
        .map(Some)
        .map_err(|error| BiscuitError::Invalid(error.to_string()))
}

/// Verify a token signature and return the trust-list key that matched.
///
/// Callers overlay staff (and other DB grants) from the matching root
/// rather than trusting `staff(true)` asserted in a client-minted token.
pub fn parse_token_with_root(
    token_b64: &str,
    trust_list: &[PublicKey],
) -> Result<(Biscuit, PublicKey), BiscuitError> {
    if trust_list.is_empty() {
        return Err(BiscuitError::Internal(
            "biscuit trust list is empty; verifier is unconfigured".to_string(),
        ));
    }
    let mut last_error = None;
    for public_key in trust_list {
        let public_key = *public_key;
        match Biscuit::from_base64(token_b64, move |_| Ok(public_key)) {
            Ok(biscuit) => return Ok((biscuit, public_key)),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(BiscuitError::Invalid(
        last_error.unwrap_or_else(|| "no trusted key matched".to_string()),
    ))
}

/// Authorize a parsed token at an explicitly injected wall-clock sample.
///
/// Requiring the caller's clock at the shared boundary keeps Worker execution
/// deterministic and avoids consulting a frozen Cloudflare clock from WASM.
pub fn authorize_at(
    biscuit: &Biscuit,
    operation: &str,
    now: DateTime<Utc>,
    initial_pop_key_hex: Option<&str>,
    trusted_presence_signers: &[PublicKey],
    resource: Option<(&str, &str)>,
) -> Result<BiscuitFacts, BiscuitError> {
    authorize_at_with_extra_facts(
        biscuit,
        operation,
        now,
        initial_pop_key_hex,
        trusted_presence_signers,
        resource,
        &[],
    )
}

/// [`authorize_at`] plus caller-supplied authorizer facts.
///
/// `extra_facts` are Datalog fact literals the *verifier* injects to describe
/// the in-flight request — they are never read off the token. Attenuation
/// checks appended by weft (e.g. the edge serving scope in [`edge`]) match
/// against them, which is what lets a check decide on request parameters that
/// did not exist when the token was minted.
///
/// Injected facts widen nothing on their own: an attenuation check that finds
/// no matching fact fails, so a caller that forgets to inject one fails closed.
#[allow(clippy::too_many_arguments)]
pub fn authorize_at_with_extra_facts(
    biscuit: &Biscuit,
    operation: &str,
    now: DateTime<Utc>,
    initial_pop_key_hex: Option<&str>,
    trusted_presence_signers: &[PublicKey],
    resource: Option<(&str, &str)>,
    extra_facts: &[String],
) -> Result<BiscuitFacts, BiscuitError> {
    let mut builder = AuthorizerBuilder::new()
        .set_limits(authorizer_limits())
        .code(HEDDLE_RULES)
        .internal_ctx("add rule pack")?
        .fact(format!("time({})", now.to_rfc3339()).as_str())
        .internal_ctx("add time fact")?;
    if !operation.is_empty() {
        builder = builder
            .fact(format!("operation({})", biscuit_string(operation)).as_str())
            .internal_ctx("add operation fact")?;
    }
    if let Some((kind, path)) = resource {
        builder = builder
            .fact(
                format!(
                    "resource({}, {})",
                    biscuit_string(kind),
                    biscuit_string(path)
                )
                .as_str(),
            )
            .internal_ctx("add resource fact")?;
    }
    if operation == CI_VERDICT_WRITE_OPERATION {
        // SECURITY: both the operation and resolved resource are injected by
        // the verifier. Trust the capability right only from the authority
        // block so an offline-appended fact cannot self-grant verdict power.
        // The action is intentionally outside admin → write → read, keeping a
        // normal spool writer unable to sign CI verdicts.
        builder = builder
            .check(
                format!(
                    "check if resource(\"spool\", $path), right(\"spool\", $path, \"{CI_VERDICT_WRITE_ACTION}\") trusting authority"
                )
                .as_str(),
            )
            .internal_ctx("add CI-verdict capability check")?;
    }
    if !extra_facts.is_empty() {
        // Injected request facts are only trustworthy if the token cannot
        // assert them itself. Scanned here rather than in `parse_token` so
        // ordinary RPCs do not pay to walk every block.
        facts::reject_reserved_request_facts(biscuit)?;
        for fact in extra_facts {
            builder = builder
                .fact(fact.as_str())
                .internal_ctx("add caller-supplied request fact")?;
        }
    }
    builder = builder
        .policy("allow if true")
        .internal_ctx("add allow policy")?;

    let mut authorizer = builder.build(biscuit).authz_ctx("authorizer build")?;
    authorizer
        .authorize()
        .map_err(|error| BiscuitError::Authorization(error.to_string()))?;
    BiscuitFacts::extract(
        &mut authorizer,
        biscuit,
        initial_pop_key_hex,
        trusted_presence_signers,
    )
}

fn authorizer_limits() -> RunLimits {
    RunLimits {
        max_facts: 1000,
        max_iterations: 100,
        // Fact and iteration limits are the deterministic DoS guard. Wall
        // time is only a runaway backstop and must tolerate host descheduling.
        max_time: Duration::from_secs(3600),
    }
}

pub fn verify_at_with_resource(
    token_b64: &str,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    verify_at_with_extra_facts(
        token_b64,
        trust_list,
        trusted_presence_signers,
        operation,
        resource,
        &[],
        now,
    )
}

/// [`verify_at_with_resource`] with verifier-injected request facts. See
/// [`authorize_at_with_extra_facts`].
#[allow(clippy::too_many_arguments)]
pub fn verify_at_with_extra_facts(
    token_b64: &str,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    extra_facts: &[String],
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    let biscuit = parse_token(token_b64, trust_list)?;
    authorize_at_with_extra_facts(
        &biscuit,
        operation,
        now,
        None,
        trusted_presence_signers,
        resource,
        extra_facts,
    )
}

pub fn verify_client_minted_at_with_resource(
    token_b64: &str,
    envelope_b64: &str,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    verify_client_minted_at_with_extra_facts(
        token_b64,
        envelope_b64,
        trust_list,
        trusted_presence_signers,
        operation,
        resource,
        &[],
        now,
    )
}

/// [`verify_client_minted_at_with_resource`] with verifier-injected request
/// facts. See [`authorize_at_with_extra_facts`].
#[allow(clippy::too_many_arguments)]
pub fn verify_client_minted_at_with_extra_facts(
    token_b64: &str,
    envelope_b64: &str,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    extra_facts: &[String],
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    use envelope::{SignedGrantEnvelope, biscuit_pubkey_from_raw};

    let signed = SignedGrantEnvelope::from_base64(envelope_b64)?;
    signed.verify_signature(trust_list)?;
    if now >= signed.envelope.expires_at {
        return Err(BiscuitError::EnvelopeInvalid(format!(
            "envelope expired at {} (now {})",
            signed.envelope.expires_at, now
        )));
    }

    let device_public_key = biscuit_pubkey_from_raw(&signed.envelope.device_pubkey)?;
    let biscuit = parse_token(token_b64, &[device_public_key])?;
    let envelope_pop_key_hex = hex::encode(signed.envelope.device_pubkey);
    let mut facts = authorize_at_with_extra_facts(
        &biscuit,
        operation,
        now,
        Some(&envelope_pop_key_hex),
        trusted_presence_signers,
        resource,
        extra_facts,
    )?;

    if let Some(reason) = client_minted_identity_violation(&facts) {
        return Err(BiscuitError::EnvelopeInvalid(format!(
            "client-minted token carries forbidden identity fact: {reason}"
        )));
    }
    if facts.sub != signed.envelope.subject {
        return Err(BiscuitError::EnvelopeInvalid(format!(
            "token subject {:?} ≠ envelope subject {:?}",
            facts.sub, signed.envelope.subject
        )));
    }
    for token_right in &facts.rights {
        if !envelope_covers_right(&signed.envelope.rights, token_right) {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "token asserts right {:?} not covered by envelope rights {:?}",
                token_right, signed.envelope.rights
            )));
        }
    }
    facts.envelope_device_pubkey_hex = Some(envelope_pop_key_hex);
    Ok(facts)
}

/// The production dual-path entry point shared by native and WASM callers.
pub fn verify_any_at_with_resource(
    token_b64: &str,
    envelope_b64: Option<&str>,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    verify_any_at_with_extra_facts(
        token_b64,
        envelope_b64,
        trust_list,
        trusted_presence_signers,
        operation,
        resource,
        &[],
        now,
    )
}

/// [`verify_any_at_with_resource`] with verifier-injected request facts. The
/// edge extent path in [`edge`] is the first consumer.
#[allow(clippy::too_many_arguments)]
pub fn verify_any_at_with_extra_facts(
    token_b64: &str,
    envelope_b64: Option<&str>,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    extra_facts: &[String],
    now: DateTime<Utc>,
) -> Result<BiscuitFacts, BiscuitError> {
    match envelope_b64 {
        Some(envelope) => verify_client_minted_at_with_extra_facts(
            token_b64,
            envelope,
            trust_list,
            trusted_presence_signers,
            operation,
            resource,
            extra_facts,
            now,
        ),
        None => verify_at_with_extra_facts(
            token_b64,
            trust_list,
            trusted_presence_signers,
            operation,
            resource,
            extra_facts,
            now,
        ),
    }
}

pub fn verify_any_at_millis_with_resource(
    token_b64: &str,
    envelope_b64: Option<&str>,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    operation: &str,
    resource: Option<(&str, &str)>,
    now_ms: u64,
) -> Result<BiscuitFacts, BiscuitError> {
    let now_ms = i64::try_from(now_ms)
        .map_err(|_| BiscuitError::Internal("verifier time exceeds i64".to_string()))?;
    let now = DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(|| BiscuitError::Internal("verifier time is invalid".to_string()))?;
    verify_any_at_with_resource(
        token_b64,
        envelope_b64,
        trust_list,
        trusted_presence_signers,
        operation,
        resource,
        now,
    )
}

pub fn parse_ed25519_public_keys_hex(
    encoded: &str,
    max_keys: usize,
) -> Result<Vec<PublicKey>, BiscuitError> {
    let mut keys = Vec::new();
    for fragment in encoded
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if keys.len() >= max_keys {
            return Err(BiscuitError::Internal(format!(
                "biscuit trust list exceeds {max_keys} keys"
            )));
        }
        let raw = hex::decode(fragment).map_err(|_| {
            BiscuitError::Internal("biscuit trust key is not hexadecimal".to_string())
        })?;
        let key = PublicKey::from_bytes(&raw, biscuit_auth::Algorithm::Ed25519)
            .map_err(|_| BiscuitError::Internal("biscuit trust key is not Ed25519".to_string()))?;
        keys.push(key);
    }
    if keys.is_empty() {
        return Err(BiscuitError::Internal(
            "biscuit trust list is empty; verifier is unconfigured".to_string(),
        ));
    }
    Ok(keys)
}

/// Exact fields covered by Heddle's provider-plan possession proof.
pub struct ProviderPlanSignatureInput<'a> {
    pub signing_identity: &'a str,
    pub stream_id: &'a str,
    pub repository: &'a str,
    pub client_endpoint_id: &'a str,
    pub plan_nonce: &'a [u8],
    pub grant_batch_digest: &'a [u8],
}

/// Verify Heddle's exact provider-plan possession proof.
pub fn verify_provider_plan_signature(
    input: &ProviderPlanSignatureInput<'_>,
    cnf_public_key: &[u8; 32],
    signature: &[u8; 64],
) -> bool {
    let Ok(public_key) = VerifyingKey::from_bytes(cnf_public_key) else {
        return false;
    };
    let signature = Signature::from_bytes(signature);
    public_key
        .verify(
            &provider_plan_bytes(
                input.signing_identity,
                input.stream_id,
                input.repository,
                input.client_endpoint_id,
                input.plan_nonce,
                input.grant_batch_digest,
            ),
            &signature,
        )
        .is_ok()
}

fn provider_plan_bytes(
    signing_identity: &str,
    stream_id: &str,
    repository: &str,
    client_endpoint_id: &str,
    plan_nonce: &[u8],
    grant_batch_digest: &[u8],
) -> Vec<u8> {
    let fields = [
        ("identity", signing_identity.as_bytes().to_vec()),
        ("stream_id", stream_id.as_bytes().to_vec()),
        ("repository", repository.as_bytes().to_vec()),
        ("client_endpoint_id", client_endpoint_id.as_bytes().to_vec()),
        ("plan_nonce", hex::encode(plan_nonce).into_bytes()),
        (
            "grant_batch_digest",
            hex::encode(grant_batch_digest).into_bytes(),
        ),
    ];
    let mut output = b"heddle-provider-plan-v1\nkind=11:exact-batch".to_vec();
    for (name, value) in fields {
        output.extend_from_slice(format!("\n{name}={}:", value.len()).as_bytes());
        output.extend_from_slice(&value);
    }
    output
}

#[doc(hidden)]
pub fn client_minted_identity_violation(facts: &BiscuitFacts) -> Option<&'static str> {
    if facts.staff_marker_present {
        return Some("staff(true) operator marker");
    }
    if facts.service_account_id.is_some() {
        return Some("service_account($id) binding");
    }
    if facts.credential_id.is_some() {
        return Some("credential_id($id) reference");
    }
    if facts.device_id.is_some() {
        return Some("device($id) binding");
    }
    if facts.authority_device_pop_key_present {
        return Some("device_pop_key($hex) additional PoP key");
    }
    if facts.agent_provider.is_some() {
        return Some("agent_provider($name) metadata");
    }
    if facts.agent_model.is_some() {
        return Some("agent_model($name) metadata");
    }
    if facts.act.is_some() {
        return Some("delegated_from($sub) actor delegation");
    }
    if facts.subject_user_uuid_str.is_some() {
        return Some("subject_user_uuid($uuid) user-substrate fact");
    }
    if facts.signup_bootstrap_email.is_some() {
        return Some("signup_bootstrap_email($email) signup-bootstrap fact");
    }
    if facts.bootstrap_session {
        return Some("bootstrap_session(true) recovery-bootstrap marker");
    }
    if facts.request_signed_session {
        return Some("request_signed_session(true) web-session marker");
    }
    if facts.root_established {
        return Some("root_established(true) independent-root ceremony marker");
    }
    None
}

#[doc(hidden)]
pub fn envelope_covers_right(envelope: &[Right], token_right: &Right) -> bool {
    envelope.iter().any(|right| {
        right.kind == token_right.kind
            && right.path == token_right.path
            && envelope_action_implies(&right.action, &token_right.action)
    })
}

#[doc(hidden)]
pub fn envelope_action_implies(envelope: &str, requested: &str) -> bool {
    envelope == requested
        || matches!(
            (envelope, requested),
            ("admin", "write") | ("admin", "read") | ("write", "read")
        )
}

pub fn revocation_ids(biscuit: &Biscuit) -> Vec<String> {
    biscuit
        .revocation_identifiers()
        .into_iter()
        .map(hex::encode)
        .collect()
}

pub(crate) fn biscuit_string(value: &str) -> String {
    format!("{value:?}")
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    #[test]
    fn agent_delegation_inherits_identity_authority_until_a_caveat_narrows_it() {
        let root = biscuit_auth::KeyPair::new();
        let signing = SigningKey::from_bytes(
            &root
                .private()
                .to_bytes()
                .as_slice()
                .try_into()
                .expect("Ed25519 root seed"),
        );
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(10);
        let mut token = Biscuit::builder()
            .code(
                format!(
                    r#"
                user("identity-owner"); session("identity-session");
                device_pop_key("{}"); root_established(true);
                expires_at({}); check if time($now), expires_at($end), $now < $end;
                right("spool", "org/allowed", "admin");
            "#,
                    hex::encode(signing.verifying_key().as_bytes()),
                    expires.to_rfc3339()
                )
                .as_str(),
            )
            .expect("human authority")
            .build(&root)
            .expect("signed authority");
        let mut parent_signer = signing;
        for seed in [41, 42] {
            let child = SigningKey::from_bytes(&[seed; 32]);
            let parent_id = token
                .revocation_identifiers()
                .last()
                .expect("parent block")
                .to_vec();
            let child_key = child.verifying_key().to_bytes();
            let signature = parent_signer.sign(&pop_delegation_payload(&parent_id, &child_key));
            token = token
                .append(
                    delegation::AgentAttenuation::time_bounded(format!("agent-{seed}"), expires)
                        .block()
                        .expect("inherited scope")
                        .fact(
                            format!(
                                "pop_delegation(\"{}\", \"{}\", \"{}\")",
                                hex::encode(parent_id),
                                hex::encode(child_key),
                                hex::encode(signature.to_bytes())
                            )
                            .as_str(),
                        )
                        .expect("delegated proof key"),
                )
                .expect("derive agent offline");
            let encoded = token.to_base64().expect("credential");
            let facts = verify_at_with_resource(
                &encoded,
                &[root.public()],
                &[],
                "ObserveIdentity",
                None,
                now,
            )
            .expect("delegated identity access");
            assert!(
                facts.root_established,
                "the human's authority remains the root"
            );
            assert!(
                !facts.limits_identity_disclosure,
                "delegation alone must not remove account authority"
            );
            assert_eq!(
                facts.delegation_agent_id.as_deref(),
                Some(format!("agent-{seed}").as_str())
            );
            assert!(facts.rights.contains(&Right::spool_admin("org/allowed")));
            parent_signer = child;
        }
        let parent_id = token
            .revocation_identifiers()
            .last()
            .expect("parent block")
            .to_vec();
        let child_key = parent_signer.verifying_key().to_bytes();
        let signature = parent_signer.sign(&pop_delegation_payload(&parent_id, &child_key));
        let identity_only = token
            .append(
                delegation::AgentAttenuation {
                    agent_id: "identity-administrator".into(),
                    expires_at: expires,
                    allowed_operations: Some(vec!["ObserveIdentity".into()]),
                    allowed_resources: None,
                }
                .block()
                .expect("explicit identity permission")
                .fact(
                    format!(
                        "pop_delegation(\"{}\", \"{}\", \"{}\")",
                        hex::encode(&parent_id),
                        hex::encode(child_key),
                        hex::encode(signature.to_bytes())
                    )
                    .as_str(),
                )
                .expect("delegated proof key"),
            )
            .expect("delegate account observation explicitly")
            .to_base64()
            .expect("credential");
        let identity_facts = verify_at_with_resource(
            &identity_only,
            &[root.public()],
            &[],
            "ObserveIdentity",
            None,
            now,
        )
        .expect("explicit account observation");
        assert!(
            !identity_facts.limits_identity_disclosure,
            "explicit identity permission grants the inherited view, not just the self-introspection exception"
        );
        assert!(
            verify_at_with_resource(
                &identity_only,
                &[root.public()],
                &[],
                "RevokeSession",
                None,
                now
            )
            .is_err(),
            "explicit observation does not grant session mutation"
        );
        let narrowed = token
            .append(
                delegation::AgentAttenuation {
                    agent_id: "scoped-agent".into(),
                    expires_at: expires,
                    allowed_operations: Some(vec!["ReadContent".into()]),
                    allowed_resources: Some(vec![("spool".into(), "org/allowed".into())]),
                }
                .block()
                .expect("explicit work ceilings")
                .fact(
                    format!(
                        "pop_delegation(\"{}\", \"{}\", \"{}\")",
                        hex::encode(parent_id),
                        hex::encode(child_key),
                        hex::encode(signature.to_bytes())
                    )
                    .as_str(),
                )
                .expect("delegated proof key"),
            )
            .expect("narrow the existing chain")
            .to_base64()
            .expect("credential");
        let facts = verify_at_with_resource(
            &narrowed,
            &[root.public()],
            &[],
            "ObserveIdentity",
            None,
            now,
        )
        .expect("self-introspection survives explicit ceilings");
        assert!(
            facts.limits_identity_disclosure,
            "explicit ceilings retain the limited self view"
        );
        let narrowed_parent =
            Biscuit::from_base64(&narrowed, |_| Ok(root.public())).expect("narrowed parent");
        let parent_id = narrowed_parent
            .revocation_identifiers()
            .last()
            .expect("parent block")
            .to_vec();
        let signature = parent_signer.sign(&pop_delegation_payload(&parent_id, &child_key));
        let descendant = narrowed_parent
            .append(
                delegation::AgentAttenuation {
                    agent_id: "identity-descendant".into(),
                    expires_at: expires,
                    allowed_operations: Some(vec!["ObserveIdentity".into()]),
                    allowed_resources: None,
                }
                .block()
                .expect("requested identity permission")
                .fact(
                    format!(
                        "pop_delegation(\"{}\", \"{}\", \"{}\")",
                        hex::encode(parent_id),
                        hex::encode(child_key),
                        hex::encode(signature.to_bytes())
                    )
                    .as_str(),
                )
                .expect("proof key"),
            )
            .expect("append to constrained parent")
            .to_base64()
            .expect("credential");
        let descendant_facts = verify_at_with_resource(
            &descendant,
            &[root.public()],
            &[],
            "ObserveIdentity",
            None,
            now,
        )
        .expect("inherited self view");
        assert!(
            descendant_facts.limits_identity_disclosure,
            "a child cannot turn its parent's self-introspection exception into account authority"
        );
        for (operation, resource, at) in [
            ("PublishContent", Some(("spool", "org/allowed")), now),
            ("ReadContent", Some(("spool", "org/other")), now),
            (
                "ObserveIdentity",
                None,
                expires + chrono::Duration::seconds(1),
            ),
        ] {
            assert!(
                verify_at_with_resource(&narrowed, &[root.public()], &[], operation, resource, at)
                    .is_err(),
                "delegation never removes a parent's operation, resource or lifetime ceiling"
            );
        }
    }

    #[test]
    fn identity_observation_disclosure_respects_operation_and_resource_ceilings() {
        let root = biscuit_auth::KeyPair::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(10);
        let token = Biscuit::builder()
            .code(
                format!(
                    r#"
                user("identity-owner");
                session("identity-session");
                expires_at({});
                check if time($now), expires_at($end), $now < $end;
                right("spool", "org/allowed", "admin");
                check if operation("ObserveIdentity") or operation("ReadContent");
                check if operation("ObserveIdentity") or resource($kind, $path), $kind == "spool", $path == "org/allowed/sub";
            "#,
                    expires.to_rfc3339()
                )
                .as_str(),
            )
            .expect("scoped authority statement")
            .build(&root)
            .expect("signed authority")
            .to_base64()
            .expect("credential encoding");
        let trust = [root.public()];
        let facts = verify_at_with_resource(&token, &trust, &[], "ObserveIdentity", None, now)
            .expect("self-observation does not select a work resource");
        assert!(
            facts.limits_identity_disclosure,
            "an identity exception cannot disclose the authority block's broader scope"
        );
        assert_eq!(facts.bounded_identity_scope, "spool:org/allowed/sub read");
        verify_at_with_resource(
            &token,
            &trust,
            &[],
            "ReadContent",
            Some(("spool", "org/allowed/sub")),
            now,
        )
        .expect("work inside both ceilings remains allowed");
        for (operation, resource, time) in [
            ("ReadContent", Some(("spool", "org/other")), now),
            ("PublishContent", Some(("spool", "org/allowed/sub")), now),
            ("WhoAmI", None, now),
            (
                "ObserveIdentity",
                None,
                expires + chrono::Duration::seconds(1),
            ),
        ] {
            assert!(
                verify_at_with_resource(&token, &trust, &[], operation, resource, time).is_err(),
                "self-observation must not widen other operations, resources, expiry or the retired route"
            );
        }
    }

    #[test]
    fn provider_plan_signature_binds_every_handoff_field() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        let nonce = [4; 16];
        let digest = [5; 32];
        let endpoint = "11".repeat(32);
        let canonical = provider_plan_bytes(
            "principal:alice",
            "pull:one",
            "acme/widgets",
            &endpoint,
            &nonce,
            &digest,
        );
        assert_eq!(
            canonical,
            heddle_api::signing::provider_plan_bytes(
                "principal:alice",
                "pull:one",
                "acme/widgets",
                &endpoint,
                &nonce,
                &digest,
            ),
            "Worker verifier preimage must remain byte-identical to the public API helper"
        );
        let signature = signing_key.sign(&canonical).to_bytes();
        assert!(verify_provider_plan_signature(
            &ProviderPlanSignatureInput {
                signing_identity: "principal:alice",
                stream_id: "pull:one",
                repository: "acme/widgets",
                client_endpoint_id: &endpoint,
                plan_nonce: &nonce,
                grant_batch_digest: &digest,
            },
            &public_key,
            &signature,
        ));
        assert!(!verify_provider_plan_signature(
            &ProviderPlanSignatureInput {
                signing_identity: "principal:alice",
                stream_id: "pull:one",
                repository: "acme/private",
                client_endpoint_id: &endpoint,
                plan_nonce: &nonce,
                grant_batch_digest: &digest,
            },
            &public_key,
            &signature,
        ));
    }

    #[test]
    fn trust_key_parser_is_bounded_and_rejects_partial_configuration() {
        let key = "11".repeat(32);
        assert_eq!(parse_ed25519_public_keys_hex(&key, 8).unwrap().len(), 1);
        assert!(parse_ed25519_public_keys_hex("", 8).is_err());
        assert!(parse_ed25519_public_keys_hex("not-hex", 8).is_err());
        assert!(parse_ed25519_public_keys_hex(&format!("{key},{key}"), 1).is_err());
    }

    #[test]
    fn device_pop_selector_reads_only_authority_block_and_still_needs_signature_verify() {
        let root = biscuit_auth::KeyPair::new();
        let forged = biscuit_auth::KeyPair::new();
        let root_hex = hex::encode(root.public().to_bytes());
        let forged_hex = hex::encode(forged.public().to_bytes());
        let token = Biscuit::builder()
            .fact(format!("device_pop_key({root_hex:?})").as_str())
            .expect("add authority selector")
            .build(&root)
            .expect("build token")
            .append(
                BlockBuilder::new()
                    .fact(format!("device_pop_key({forged_hex:?})").as_str())
                    .expect("add appended spoof"),
            )
            .expect("append spoof block")
            .to_base64()
            .expect("encode token");

        assert_eq!(
            unverified_authority_device_pop_key(&token)
                .expect("parse selector")
                .expect("authority selector present"),
            root.public(),
            "an attenuation block cannot redirect the key lookup"
        );
        assert!(
            parse_token(&token, &[forged.public()]).is_err(),
            "reading a selector cannot replace Biscuit signature verification"
        );
        parse_token(&token, &[root.public()]).expect("authority-selected signer verifies");
    }
}
