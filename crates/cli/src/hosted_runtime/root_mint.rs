//! Client-side independent-root Biscuit mint.
//!
//! One ceremony: the caller's Ed25519 seed is the Biscuit authority key and
//! the request-proof key. Signup, claim, passkey finish, anon, and
//! `CreateAgentAccount` all call this. Weft only registers the public key.

use anyhow::{Context, Result, bail};
use biscuit_auth::{Algorithm, KeyPair, PrivateKey};
use chrono::{DateTime, Duration, Utc};
use crypto::{Ed25519Signer, Signer as _};

/// Default lifetime for an account, claim, passkey, or device root.
pub(crate) const ACCOUNT_ROOT_TTL: Duration = Duration::days(30);
/// Anon roots stay short-lived; continuity is a local remint of the same seed.
pub(crate) const ANON_ROOT_TTL: Duration = Duration::hours(24);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndependentRootKind {
    Account,
    Anon,
}

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

/// Mint a root from an existing seed. `CreateAgentAccount` uses the persisted
/// Iroh node seed so the claim signer and the authority key stay the same key.
pub(crate) fn mint_independent_root(
    seed: &[u8; 32],
    subject: &str,
    kind: IndependentRootKind,
    ttl: Duration,
    credential_id: Option<&str>,
) -> Result<IndependentRoot> {
    validate_root_string("subject", subject)?;
    if let Some(credential_id) = credential_id {
        validate_root_string("credential_id", credential_id)?;
    }
    if ttl <= Duration::zero() {
        bail!("independent-root TTL must be greater than zero");
    }
    let expires_at = Utc::now()
        .checked_add_signed(ttl)
        .context("independent-root TTL overflows the calendar")?;
    let signer = Ed25519Signer::from_seed(seed).context("loading independent-root seed")?;
    let public_key: [u8; 32] = signer
        .public_key()
        .try_into()
        .map_err(|_| anyhow::anyhow!("independent-root public key must be 32 bytes"))?;
    let private_key_pem = signer
        .to_pem()
        .context("exporting independent-root proof key")?;
    let authority = authority_keypair(seed)?;
    let pop_hex = hex::encode(public_key);
    let session = format!("sess-{}", uuid::Uuid::new_v4());
    let expiry = expires_at.to_rfc3339();
    let mut builder = biscuit_auth::Biscuit::builder();
    builder = builder
        .fact(format!("user({})", quote(subject)).as_str())
        .context("independent-root user fact")?;
    builder = builder
        .fact(format!("session({})", quote(&session)).as_str())
        .context("independent-root session fact")?;
    builder = builder
        .fact(format!("device_pop_key({})", quote(&pop_hex)).as_str())
        .context("independent-root device_pop_key fact")?;
    if let Some(credential_id) = credential_id {
        builder = builder
            .fact(format!("credential_id({})", quote(credential_id)).as_str())
            .context("independent-root credential_id fact")?;
    }
    match kind {
        IndependentRootKind::Account => {}
        IndependentRootKind::Anon => {
            builder = builder
                .fact(r#"subject_kind("anon")"#)
                .context("independent-root anon subject_kind fact")?;
        }
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
        subject: subject.to_string(),
        public_key,
        private_key_pem,
        expires_at,
        credential_id: credential_id.map(ToOwned::to_owned),
    })
}

/// Fresh seed plus mint. Anon and device-login roots start here.
pub(crate) fn mint_new_independent_root(
    subject: &str,
    kind: IndependentRootKind,
    ttl: Duration,
    credential_id: Option<&str>,
) -> Result<IndependentRoot> {
    let mut seed = [0_u8; 32];
    getrandom::fill(&mut seed).context("generating independent-root seed")?;
    mint_independent_root(&seed, subject, kind, ttl, credential_id)
}

pub(crate) fn mint_agent_root(seed: &[u8; 32]) -> Result<IndependentRoot> {
    let signer = Ed25519Signer::from_seed(seed).context("loading agent root seed")?;
    let subject = format!("agent-key:{}", hex::encode(signer.public_key()));
    mint_independent_root(
        seed,
        &subject,
        IndependentRootKind::Account,
        ACCOUNT_ROOT_TTL,
        None,
    )
}

pub(crate) fn mint_anon_root() -> Result<IndependentRoot> {
    let subject = format!("anon:{}", uuid::Uuid::new_v4());
    mint_new_independent_root(&subject, IndependentRootKind::Anon, ANON_ROOT_TTL, None)
}

pub(crate) fn remint_stored_root(
    private_key_pem: &str,
    subject: &str,
    credential_id: Option<&str>,
) -> Result<IndependentRoot> {
    let signer =
        Ed25519Signer::from_pem(private_key_pem).context("loading stored independent-root key")?;
    mint_independent_root(
        &signer.to_seed(),
        subject,
        IndependentRootKind::Account,
        ACCOUNT_ROOT_TTL,
        credential_id,
    )
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
