//! Client-side independent-root Biscuit mint.
//!
//! One ceremony: the caller's Ed25519 seed is the Biscuit authority key and
//! the request-proof key. Device login, rotation, and `CreateAgentAccount`
//! call this. Weft only registers the public key.
//!
//! `session()` is the RevokeSession id. It is never a caller-chosen random
//! UUID: a server-issued session id wins, otherwise `cred:{credential_id}`
//! or `key:{public_key}`. Reminting the same registered key cannot mint a
//! new live session.

use anyhow::{Context, Result, bail};
use biscuit_auth::{Algorithm, KeyPair, PrivateKey};
use chrono::{DateTime, Duration, Utc};
use crypto::{Ed25519Signer, Signer as _};

/// Default lifetime for an account, claim, passkey, or device root.
pub(crate) const ACCOUNT_ROOT_TTL: Duration = Duration::days(30);

#[derive(Clone, Debug)]
pub(crate) struct IndependentRoot {
    pub(crate) token: String,
    pub(crate) subject: String,
    pub(crate) public_key: [u8; 32],
    pub(crate) private_key_pem: String,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) credential_id: Option<String>,
}

impl IndependentRoot {
    pub(crate) fn public_key_hex(&self) -> String {
        hex::encode(self.public_key)
    }
}

/// Inputs for one independent-root mint. `expires_at` wins when set
/// (device login honors the server's returned expiry); otherwise `ttl`
/// is added to now.
pub(crate) struct IndependentRootMint<'a> {
    pub seed: &'a [u8; 32],
    pub subject: &'a str,
    pub ttl: Duration,
    pub credential_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Mint a root from an existing seed. `CreateAgentAccount` uses the persisted
/// Iroh node seed so the claim signer and the authority key stay the same key.
pub(crate) fn mint_independent_root(mint: IndependentRootMint<'_>) -> Result<IndependentRoot> {
    validate_root_string("subject", mint.subject)?;
    if let Some(credential_id) = mint.credential_id {
        validate_root_string("credential_id", credential_id)?;
    }
    let now = Utc::now();
    let expires_at = match mint.expires_at {
        Some(expires_at) => expires_at,
        None => {
            if mint.ttl <= Duration::zero() {
                bail!("independent-root TTL must be greater than zero");
            }
            now.checked_add_signed(mint.ttl)
                .context("independent-root TTL overflows the calendar")?
        }
    };
    if expires_at <= now {
        bail!("independent-root expiry must be in the future");
    }
    let signer = Ed25519Signer::from_seed(mint.seed).context("loading independent-root seed")?;
    let public_key: [u8; 32] = signer
        .public_key()
        .try_into()
        .map_err(|_| anyhow::anyhow!("independent-root public key must be 32 bytes"))?;
    let private_key_pem = signer
        .to_pem()
        .context("exporting independent-root proof key")?;
    let authority = authority_keypair(mint.seed)?;
    let pop_hex = hex::encode(public_key);
    let session = resolve_revocation_session(&pop_hex, mint.credential_id, mint.session_id)?;
    let expiry = expires_at.to_rfc3339();
    let mut builder = biscuit_auth::Biscuit::builder();
    builder = builder
        .fact(format!("user({})", quote(mint.subject)).as_str())
        .context("independent-root user fact")?;
    builder = builder
        .fact(format!("session({})", quote(&session)).as_str())
        .context("independent-root session fact")?;
    builder = builder
        .fact(format!("device_pop_key({})", quote(&pop_hex)).as_str())
        .context("independent-root device_pop_key fact")?;
    if let Some(credential_id) = mint.credential_id {
        builder = builder
            .fact(format!("credential_id({})", quote(credential_id)).as_str())
            .context("independent-root credential_id fact")?;
    }
    builder = builder
        .fact(format!("expires_at({expiry})").as_str())
        .context("independent-root expires_at fact")?;
    builder = builder
        .check(format!("check if time($now), $now < {expiry}").as_str())
        .context("independent-root expiry check")?;
    let token = builder
        .build(&authority)
        .context("signing the independent-root authority block")?
        .to_base64()
        .context("encoding the independent-root token")?;
    Ok(IndependentRoot {
        token,
        subject: mint.subject.to_string(),
        public_key,
        private_key_pem,
        expires_at,
        credential_id: mint.credential_id.map(ToOwned::to_owned),
    })
}

pub(crate) fn mint_agent_root(seed: &[u8; 32]) -> Result<IndependentRoot> {
    let signer = Ed25519Signer::from_seed(seed).context("loading agent root seed")?;
    let subject = agent_root_subject(signer.public_key());
    mint_independent_root(IndependentRootMint {
        seed,
        subject: &subject,
        ttl: ACCOUNT_ROOT_TTL,
        credential_id: None,
        session_id: None,
        expires_at: None,
    })
}

pub(crate) fn remint_stored_root(
    private_key_pem: &str,
    subject: &str,
    credential_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<IndependentRoot> {
    let signer =
        Ed25519Signer::from_pem(private_key_pem).context("loading stored independent-root key")?;
    mint_independent_root(IndependentRootMint {
        seed: &signer.to_seed(),
        subject,
        ttl: ACCOUNT_ROOT_TTL,
        credential_id,
        session_id,
        expires_at: None,
    })
}

pub(crate) fn agent_root_subject(public_key: &[u8]) -> String {
    format!("agent-key:{}", hex::encode(public_key))
}

pub(crate) fn authority_keypair(seed: &[u8; 32]) -> Result<KeyPair> {
    let private = PrivateKey::from_bytes(seed, Algorithm::Ed25519)
        .context("deriving the Biscuit authority key from the independent-root seed")?;
    Ok(KeyPair::from(&private))
}

pub(crate) fn is_local_agent_root(subject: &str, proof_public_key_hex: &str) -> bool {
    subject.eq_ignore_ascii_case(&agent_root_subject(
        &match hex::decode(proof_public_key_hex) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        },
    ))
}

/// True when a stored local agent root is missing expiry or is already stale.
pub(crate) fn local_agent_credential_needs_refresh(
    expires_at: Option<&str>,
    now: DateTime<Utc>,
) -> bool {
    match expires_at {
        None => true,
        Some(value) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc) <= now)
            .unwrap_or(true),
    }
}

pub(crate) fn authority_session_fact(token: &str) -> Result<String> {
    use biscuit_auth::builder::{BlockBuilder, Term};

    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token.as_bytes())
        .context("parsing independent-root session")?;
    let source = biscuit
        .print_block_source(0)
        .context("reading independent-root authority block")?;
    let authority = BlockBuilder::new()
        .code(&source)
        .context("parsing independent-root authority facts")?;
    let mut sessions = authority.facts.iter().filter_map(|fact| {
        if fact.predicate.name != "session" || fact.predicate.terms.len() != 1 {
            return None;
        }
        match &fact.predicate.terms[0] {
            Term::Str(value) => Some(value.clone()),
            _ => None,
        }
    });
    let session = sessions
        .next()
        .ok_or_else(|| anyhow::anyhow!("independent-root authority is missing session"))?;
    if sessions.next().is_some() {
        bail!("independent-root authority contains multiple session facts");
    }
    Ok(session)
}

fn resolve_revocation_session(
    public_key_hex: &str,
    credential_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<String> {
    if let Some(session) = session_id.filter(|value| !value.is_empty()) {
        validate_root_string("session", session)?;
        return Ok(session.to_string());
    }
    if let Some(credential_id) = credential_id.filter(|value| !value.is_empty()) {
        validate_root_string("credential_id", credential_id)?;
        return Ok(format!("cred:{credential_id}"));
    }
    Ok(format!("key:{public_key_hex}"))
}

fn validate_root_string(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    for ch in value.chars() {
        if !matches!(
            ch,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '/' | '@' | ':' | '+' | '-'
        ) {
            bail!("{field} contains forbidden character {ch:?}; allowed: [A-Za-z0-9._/@:+-]");
        }
    }
    Ok(())
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}
