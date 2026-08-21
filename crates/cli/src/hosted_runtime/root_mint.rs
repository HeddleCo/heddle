//! Client-side independent-root Biscuit mint.
//!
//! The caller's Ed25519 seed is the Biscuit authority key and the
//! request-proof key. `heddle auth login` remints and invite-creates
//! through this path. Weft only registers the public key.

use anyhow::{Context, Result, bail};
use biscuit_auth::{Algorithm, KeyPair, PrivateKey};
use chrono::{DateTime, Duration, Utc};
use crypto::{Ed25519Signer, Signer as _};

/// Default lifetime for an account or device root.
pub(crate) const ACCOUNT_ROOT_TTL: Duration = Duration::days(30);

#[derive(Clone, Debug)]
pub(crate) struct IndependentRoot {
    pub(crate) token: String,
    pub(crate) subject: String,
    pub(crate) public_key: [u8; 32],
    pub(crate) private_key_pem: String,
    pub(crate) expires_at: DateTime<Utc>,
}

impl IndependentRoot {
    pub(crate) fn public_key_hex(&self) -> String {
        hex::encode(self.public_key)
    }
}

pub(crate) struct IndependentRootMint<'a> {
    pub seed: &'a [u8; 32],
    pub subject: &'a str,
    pub ttl: Duration,
}

/// Mint a root from an existing seed. Invite-create uses the persisted
/// Iroh node seed so the registered key and the authority key stay the
/// same key.
pub(crate) fn mint_independent_root(mint: IndependentRootMint<'_>) -> Result<IndependentRoot> {
    validate_root_string("subject", mint.subject)?;
    if mint.ttl <= Duration::zero() {
        bail!("independent-root TTL must be greater than zero");
    }
    let now = Utc::now();
    let expires_at = now
        .checked_add_signed(mint.ttl)
        .context("independent-root TTL overflows the calendar")?;
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
    let session = format!("key:{pop_hex}");
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
    })
}

pub(crate) fn mint_agent_root(seed: &[u8; 32]) -> Result<IndependentRoot> {
    let signer = Ed25519Signer::from_seed(seed).context("loading agent root seed")?;
    let subject = agent_root_subject(signer.public_key());
    mint_independent_root(IndependentRootMint {
        seed,
        subject: &subject,
        ttl: ACCOUNT_ROOT_TTL,
    })
}

pub(crate) fn agent_root_subject(public_key: &[u8]) -> String {
    format!("agent-key:{}", hex::encode(public_key))
}

fn authority_keypair(seed: &[u8; 32]) -> Result<KeyPair> {
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
#[cfg(test)]
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
