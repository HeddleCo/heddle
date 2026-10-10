//! Proof-of-possession exchange for ordinary Git's short-lived transport credential.
//!
//! This module verifies the client's statement, never issues account authority.
//! A host must first verify the registered root, overlay its CURRENT owner, check
//! all revocations and resolve current Spool/Thread disclosure. It must consume
//! the nonce atomically in its shared authority store before creating a session.
//! Every use of that session repeats those checks on the original credential.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};

use crate::{BiscuitError, BiscuitFacts, biscuit_string, edge::EdgeAudience};

pub const GIT_SERVICE_AUDIENCE: &str = "git-gateway";
pub const GIT_REQUEST_PREDICATE: &str = "git_transport_request_v1";
pub const GIT_CHALLENGE_TTL_SECONDS: i64 = 60;
pub const GIT_SESSION_TTL_SECONDS: i64 = 300;
const DOMAIN: &[u8] = b"heddle-git-transport-exchange-v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitAction {
    Read,
    Write,
}
impl GitAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
    /// The actual operation supplied to the ordinary Biscuit verifier. A
    /// delegated operation ceiling must explicitly admit the Git operation.
    pub fn operation(self) -> &'static str {
        match self {
            Self::Read => "GitRead",
            Self::Write => "GitWrite",
        }
    }
}

/// Stable identities plus the current canonical address. The authority resolves
/// these together; a caller-selected path or label alone is never authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitScope {
    pub service_audience: String,
    pub tenant_spool_id: uuid::Uuid,
    pub spool_id: uuid::Uuid,
    pub repository_path: String,
    pub thread_id: [u8; 32],
    pub action: GitAction,
    /// Content disclosure is independent of the service audience above.
    pub disclosure_audience: String,
}
impl GitScope {
    pub fn validate(&self) -> Result<(), BiscuitError> {
        let path = &self.repository_path;
        if self.service_audience != GIT_SERVICE_AUDIENCE
            || self.tenant_spool_id.is_nil()
            || self.spool_id.is_nil()
            || self.thread_id == [0; 32]
            || path.is_empty()
            || path.len() > 1024
            || path.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.len() > 128
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
            || path.starts_with("me/")
            || self.disclosure_audience.len() > 256
            || EdgeAudience::parse(&self.disclosure_audience).is_none()
            || self.disclosure_audience.chars().any(char::is_control)
        {
            return Err(denied("invalid canonical Git scope"));
        }
        Ok(())
    }

    /// Injected by the verifier only, and reserved against token facts AND rule
    /// heads. Attenuation may add a check against this fact to narrow the scope.
    pub fn request_fact(&self) -> Result<String, BiscuitError> {
        self.validate()?;
        let fields = [
            self.service_audience.clone(),
            self.tenant_spool_id.to_string(),
            self.spool_id.to_string(),
            self.repository_path.clone(),
            hex::encode(self.thread_id),
            self.action.as_str().to_string(),
            self.disclosure_audience.clone(),
        ];
        Ok(format!(
            "{GIT_REQUEST_PREDICATE}({})",
            fields
                .iter()
                .map(|field| biscuit_string(field))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// A write transport session includes read negotiation on the same exact
    /// target. The authority still revalidates its original Write caveats and
    /// current writer role; this does not reauthorize the Biscuit as GitRead.
    pub fn covers(&self, requested: &Self) -> bool {
        let mut narrowed = self.clone();
        if self.action == GitAction::Write && requested.action == GitAction::Read {
            narrowed.action = GitAction::Read;
        }
        narrowed == *requested
    }

    pub fn require_capability(&self, facts: &BiscuitFacts) -> Result<(), BiscuitError> {
        self.validate()?;
        // Registered account roots may carry identity only. Their authority is
        // resolved by the host from live grants; explicit rights only narrow it.
        if facts.rights.is_empty() {
            return Ok(());
        }
        let thread_path = format!(
            "{}/threads/{}",
            self.repository_path,
            hex::encode(self.thread_id)
        );
        // The explicit ceiling also constrains staff: live account standing is
        // not a reason to discard a credential's declared restriction.
        let actions: &[&str] = match self.action {
            GitAction::Read => &["read", "write", "admin"],
            GitAction::Write => &["write", "admin"],
        };
        let permitted =
            std::iter::once((crate::resource::ResourceKind::Thread, thread_path.as_str()))
                .chain(crate::resource::walk_to_root(
                    crate::resource::ResourceKind::Thread,
                    &thread_path,
                ))
                .any(|(kind, path)| {
                    actions
                        .iter()
                        .any(|action| facts.has_right(kind.as_str(), path, action))
                });
        if !permitted {
            return Err(denied("Git capability does not cover scope"));
        }
        Ok(())
    }
}

/// A server-issued, single-use challenge. Storage/consumption belongs to the
/// shared authority service, not a Worker instance or client-supplied clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitChallenge {
    pub nonce: [u8; 32],
    pub issued_at_seconds: i64,
    pub expires_at_seconds: i64,
}

/// Signed exchange payload. Never log the accompanying Biscuit or transport
/// secret; this structure intentionally contains only their bound digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitExchange {
    pub scope: GitScope,
    pub credential_digest: [u8; 32],
    pub challenge: GitChallenge,
    pub session_expires_at_seconds: i64,
}
impl GitExchange {
    /// Canonical client-signing helper, also used by the authority verifier.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, BiscuitError> {
        self.scope.validate()?;
        let mut out = DOMAIN.to_vec();
        for field in [
            self.scope.service_audience.as_bytes(),
            self.scope.tenant_spool_id.as_bytes(),
            self.scope.spool_id.as_bytes(),
            self.scope.repository_path.as_bytes(),
            self.scope.thread_id.as_slice(),
            self.scope.action.as_str().as_bytes(),
            self.scope.disclosure_audience.as_bytes(),
            self.credential_digest.as_slice(),
            self.challenge.nonce.as_slice(),
        ] {
            append_field(&mut out, field);
        }
        out.extend_from_slice(&self.challenge.issued_at_seconds.to_be_bytes());
        out.extend_from_slice(&self.challenge.expires_at_seconds.to_be_bytes());
        out.extend_from_slice(&self.session_expires_at_seconds.to_be_bytes());
        Ok(out)
    }

    pub fn validate_at(&self, now: DateTime<Utc>) -> Result<(), BiscuitError> {
        self.scope.validate()?;
        let issued = self.challenge.issued_at_seconds;
        let expires = self.challenge.expires_at_seconds;
        let now = now.timestamp();
        if self.challenge.nonce == [0; 32]
            || issued > now
            || now >= expires
            || expires
                .checked_sub(issued)
                .is_none_or(|ttl| ttl <= 0 || ttl > GIT_CHALLENGE_TTL_SECONDS)
            || self.session_expires_at_seconds <= now
            || self
                .session_expires_at_seconds
                .checked_sub(issued)
                .is_none_or(|ttl| ttl <= 0 || ttl > GIT_SESSION_TTL_SECONDS)
        {
            return Err(denied("expired or invalid Git exchange"));
        }
        Ok(())
    }
}

/// Digest binds the exact credential and envelope encoding, including absence.
/// Current registered-root hosts reject envelopes instead of silently ignoring
/// one. Bound sizes also keep unauthenticated exchange work finite.
pub fn credential_digest(biscuit: &str, envelope: Option<&str>) -> Result<[u8; 32], BiscuitError> {
    if biscuit.is_empty()
        || biscuit.len() > 96 * 1024
        || envelope.is_some_and(|value| value.is_empty() || value.len() > 96 * 1024)
    {
        return Err(denied("invalid Git exchange credential size"));
    }
    let mut bytes = b"heddle-git-credential-v1\0".to_vec();
    append_field(&mut bytes, biscuit.as_bytes());
    bytes.push(u8::from(envelope.is_some()));
    append_field(&mut bytes, envelope.unwrap_or_default().as_bytes());
    Ok(*blake3::hash(&bytes).as_bytes())
}

/// Recognize only the separate Git transport secret, never a Biscuit bearer.
pub fn validate_transport_token(token: &str) -> Result<(), BiscuitError> {
    let secret = token
        .strip_prefix("ggit1_")
        .ok_or_else(|| denied("invalid Git transport credential"))?;
    if secret.len() != 64
        || !secret
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(denied("invalid Git transport credential"));
    }
    Ok(())
}

/// Ordinary Git credential helpers send HTTP Basic; explicitly configured Git
/// clients may send Bearer. Both carry the exchanged token, never the Biscuit.
/// The username is fixed to `git` and supplies no identity or authorization.
pub fn transport_token_from_authorization(header: &str) -> Result<String, BiscuitError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    if header.len() > 256 {
        return Err(denied("invalid Git authorization header"));
    }
    let token = if let Some(token) = header.strip_prefix("Bearer ") {
        token.to_owned()
    } else if let Some(encoded) = header.strip_prefix("Basic ") {
        let decoded = STANDARD
            .decode(encoded)
            .map_err(|_| denied("invalid Git authorization header"))?;
        if STANDARD.encode(&decoded) != encoded {
            return Err(denied("invalid Git authorization header"));
        }
        String::from_utf8(decoded)
            .map_err(|_| denied("invalid Git authorization header"))?
            .strip_prefix("git:")
            .ok_or_else(|| denied("invalid Git authorization username"))?
            .to_owned()
    } else {
        return Err(denied("Git transport credential required"));
    };
    validate_transport_token(&token)?;
    Ok(token)
}

/// Cryptographic proof only. Account/grant/revocation admission remains the
/// host's job. The effective leaf key from verified facts is mandatory, so
/// possessing the Biscuit's serialized bytes alone cannot exchange it.
pub fn verify_exchange_proof(
    exchange: &GitExchange,
    biscuit: &str,
    envelope: Option<&str>,
    signature: &[u8; 64],
    verified_facts: &BiscuitFacts,
    now: DateTime<Utc>,
) -> Result<(), BiscuitError> {
    exchange.validate_at(now)?;
    if exchange.credential_digest != credential_digest(biscuit, envelope)? {
        return Err(denied("Git exchange credential binding mismatch"));
    }
    exchange.scope.require_capability(verified_facts)?;
    let key: [u8; 32] = verified_facts
        .cnf
        .as_ref()
        .and_then(|value| hex::decode(value).ok())
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| denied("Git exchange requires a verified proof key"))?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| denied("invalid Git proof key"))?;
    key.verify_strict(
        &exchange.signing_bytes()?,
        &Signature::from_bytes(signature),
    )
    .map_err(|_| denied("Git proof of possession failed"))
}

fn append_field(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_be_bytes());
    out.extend_from_slice(value);
}
fn denied(reason: &str) -> BiscuitError {
    BiscuitError::Authorization(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature_v1::BiscuitBuilderV1Ext as _;
    use biscuit_auth::{Algorithm, Biscuit, KeyPair, PrivateKey};
    use ed25519_dalek::{Signer as _, SigningKey};

    fn fixture() -> (String, SigningKey, BiscuitFacts, GitExchange, DateTime<Utc>) {
        let signing = SigningKey::from_bytes(&[17; 32]);
        let root =
            KeyPair::from(&PrivateKey::from_bytes(&[17; 32], Algorithm::Ed25519).expect("key"));
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("time");
        let token = Biscuit::builder().code(format!(
            "user(\"account\"); session(\"git-test\"); device_pop_key(\"{}\"); right(\"spool\", \"org/project\", \"write\"); check if time($now), $now < {};",
            hex::encode(signing.verifying_key().as_bytes()), (now + chrono::Duration::hours(1)).to_rfc3339()
        ).as_str()).expect("facts").build_v1(&root).expect("root").to_base64().expect("encode");
        let scope = GitScope {
            service_audience: GIT_SERVICE_AUDIENCE.into(),
            tenant_spool_id: uuid::Uuid::from_bytes([1; 16]),
            spool_id: uuid::Uuid::from_bytes([2; 16]),
            repository_path: "org/project".into(),
            thread_id: [3; 32],
            action: GitAction::Write,
            disclosure_audience: "internal".into(),
        };
        let facts = crate::verify_at_with_extra_facts(
            &token,
            &[root.public()],
            &[],
            scope.action.operation(),
            Some(("spool", &scope.repository_path)),
            &[scope.request_fact().expect("request fact")],
            now,
        )
        .expect("verify");
        let exchange = GitExchange {
            scope,
            credential_digest: credential_digest(&token, None).expect("digest"),
            challenge: GitChallenge {
                nonce: [5; 32],
                issued_at_seconds: now.timestamp(),
                expires_at_seconds: now.timestamp() + 60,
            },
            session_expires_at_seconds: now.timestamp() + 300,
        };
        (token, signing, facts, exchange, now)
    }
    #[test]
    fn ordinary_git_basic_and_bearer_share_only_the_exchanged_credential() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let token = format!("ggit1_{}", hex::encode([7; 32]));
        assert_eq!(
            transport_token_from_authorization(&format!("Bearer {token}")).expect("bearer"),
            token
        );
        assert_eq!(
            transport_token_from_authorization(&format!(
                "Basic {}",
                STANDARD.encode(format!("git:{token}"))
            ))
            .expect("basic"),
            token
        );
        assert!(
            transport_token_from_authorization(&format!(
                "Basic {}",
                STANDARD.encode(format!("other:{token}"))
            ))
            .is_err()
        );
        assert!(transport_token_from_authorization("Bearer serialized-biscuit").is_err());
        assert!(
            transport_token_from_authorization(&format!("Bearer {token}, Bearer {token}")).is_err()
        );
    }

    #[test]
    fn exchange_requires_actual_effective_key_and_binds_all_fields() {
        let (token, signing, facts, exchange, now) = fixture();
        let signature = signing
            .sign(&exchange.signing_bytes().expect("bytes"))
            .to_bytes();
        verify_exchange_proof(&exchange, &token, None, &signature, &facts, now).expect("proof");
        let wrong = SigningKey::from_bytes(&[18; 32])
            .sign(&exchange.signing_bytes().expect("bytes"))
            .to_bytes();
        assert!(verify_exchange_proof(&exchange, &token, None, &wrong, &facts, now).is_err());
        let mut no_pop = facts.clone();
        no_pop.cnf = None;
        assert!(verify_exchange_proof(&exchange, &token, None, &signature, &no_pop, now).is_err());
        let mut variants = vec![exchange.clone(); 9];
        variants[0].scope.tenant_spool_id = uuid::Uuid::from_bytes([8; 16]);
        variants[1].scope.spool_id = uuid::Uuid::from_bytes([8; 16]);
        variants[2].scope.repository_path = "org/other".into();
        variants[3].scope.thread_id = [8; 32];
        variants[4].scope.action = GitAction::Read;
        variants[5].scope.disclosure_audience = "public".into();
        variants[6].challenge.nonce = [8; 32];
        variants[7].challenge.expires_at_seconds -= 1;
        variants[8].session_expires_at_seconds -= 1;
        for changed in variants {
            assert!(
                verify_exchange_proof(&changed, &token, None, &signature, &facts, now).is_err()
            );
        }
        assert!(
            verify_exchange_proof(
                &exchange,
                &format!("{token}x"),
                None,
                &signature,
                &facts,
                now
            )
            .is_err()
        );
        assert!(
            verify_exchange_proof(&exchange, &token, Some("other"), &signature, &facts, now)
                .is_err()
        );
    }
    #[test]
    fn explicit_read_ceiling_still_restricts_a_current_staff_account() {
        let (_, _, mut facts, exchange, _) = fixture();
        facts.rights.retain(|right| right.action == "read");
        facts.apply_staff_grant();
        assert!(exchange.scope.require_capability(&facts).is_err());
    }

    #[test]
    fn exchange_expiry_is_bounded_and_audience_is_not_disclosure() {
        let (_, _, _, mut exchange, now) = fixture();
        assert!(
            exchange
                .validate_at(now + chrono::Duration::seconds(60))
                .is_err()
        );
        exchange.session_expires_at_seconds += 1;
        assert!(exchange.validate_at(now).is_err());
        exchange.session_expires_at_seconds -= 1;
        exchange.scope.service_audience = "public".into();
        assert!(exchange.validate_at(now).is_err());
    }
    #[test]
    fn reserved_git_request_facts_and_rule_heads_are_refused() {
        let root = KeyPair::new();
        for code in [
            format!("{GIT_REQUEST_PREDICATE}(\"forged\");"),
            format!("{GIT_REQUEST_PREDICATE}($v) <- user($v);"),
        ] {
            let token = Biscuit::builder()
                .fact("user(\"account\")")
                .expect("user")
                .code(&code)
                .expect("code")
                .build_v1(&root)
                .expect("token");
            assert!(crate::facts::reject_reserved_request_facts(&token).is_err());
        }
    }
    #[test]
    fn delegated_child_key_must_sign_and_ancestor_checks_still_apply() {
        let (parent, signing, _, mut exchange, now) = fixture();
        let child = SigningKey::from_bytes(&[19; 32]);
        let statement =
            crate::key_delegation::statement(&parent, &child.verifying_key().to_bytes())
                .expect("statement");
        let restrictions = crate::delegation::AgentAttenuation {
            agent_id: "git-worker".into(),
            expires_at: now + chrono::Duration::seconds(40),
            allowed_operations: Some(vec!["GitRead".into()]),
            allowed_resources: Some(vec![("spool".into(), "org/project".into())]),
        }
        .block()
        .expect("restrictions");
        let token = crate::key_delegation::append(
            &parent,
            &child.verifying_key().to_bytes(),
            &signing.sign(&statement).to_bytes(),
            restrictions,
        )
        .expect("child token");
        exchange.scope.action = GitAction::Read;
        exchange.credential_digest = credential_digest(&token, None).expect("digest");
        let root =
            KeyPair::from(&PrivateKey::from_bytes(&[17; 32], Algorithm::Ed25519).expect("key"));
        let verify = |scope: &GitScope, time| {
            crate::verify_at_with_extra_facts(
                &token,
                &[root.public()],
                &[],
                scope.action.operation(),
                Some(("spool", &scope.repository_path)),
                &[scope.request_fact().expect("request fact")],
                time,
            )
        };
        let facts = verify(&exchange.scope, now).expect("delegated authorization");
        let bytes = exchange.signing_bytes().expect("exchange bytes");
        verify_exchange_proof(
            &exchange,
            &token,
            None,
            &child.sign(&bytes).to_bytes(),
            &facts,
            now,
        )
        .expect("leaf proof");
        assert!(
            verify_exchange_proof(
                &exchange,
                &token,
                None,
                &signing.sign(&bytes).to_bytes(),
                &facts,
                now
            )
            .is_err()
        );
        assert!(verify(&exchange.scope, now + chrono::Duration::seconds(40)).is_err());
        exchange.scope.action = GitAction::Write;
        assert!(verify(&exchange.scope, now).is_err());
    }
}
