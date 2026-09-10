//! Retain the actual device credential at hosted enrollment/renewal boundaries.
//! Browser requests and installed agent credentials never impersonate this key.
use anyhow::{Context, Result};
use crypto::{Ed25519Signer, Signer};

pub(super) fn retain(
    server: &str,
    credential: &config::credentials::ServerCredential,
) -> Result<()> {
    let home = repo::identity::heddle_home_dir();
    let Some(device) = repo::identity::load_device(&repo::identity::device_identity_path())? else {
        return Ok(());
    };
    if !super::hosted::server_keys_match(&device.server, server) {
        return Ok(());
    }
    let Some(pem) = credential.private_key_pem.as_deref() else {
        return Ok(());
    };
    let signer = Ed25519Signer::from_pem(pem)?;
    if hex::encode(signer.public_key()) != device.public_key {
        return Ok(());
    }
    // An imported credential without independently admitted account ownership
    // still captures as an explicit local key; importing does not enroll trust.
    if !home.join("state/device-rpc/authority.bin").try_exists()? {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(&home, now)?;
    let publisher: [u8; 32] = signer
        .public_key()
        .try_into()
        .context("source publisher length")?;
    let root =
        biscuit_verifier::PublicKey::from_bytes(&publisher, biscuit_auth::Algorithm::Ed25519)?;
    let token = biscuit_verifier::parse_token(&credential.token, &[root])?;
    repo::identity::source_author::publish(&home, &authority, &publisher, &publisher, &token, now)
}
