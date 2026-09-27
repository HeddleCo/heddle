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
    repo::identity::source_author::publish(&home, &authority, &publisher, &publisher, &token, now)?;
    repo::identity::retain_device_bearer(
        server,
        &publisher,
        &credential.token,
        &credential.subject,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_retains_the_exact_device_bearer_for_later_agent_uploads() {
        let signer = Ed25519Signer::from_seed(&[89; 32]).expect("device signer");
        let recovery = Ed25519Signer::from_seed(&[90; 32]).expect("recovery signer");
        let owner_id = uuid::Uuid::from_bytes([9; 16]);
        let root = repo::sign_custodial_owner_root(&signer, &recovery, [9; 16], [5; 32])
            .expect("owner root");
        let binding =
            repo::sign_custodial_owner_binding(&signer, &root, [6; 32]).expect("owner binding");
        let verified = heddleco_capability_verifier::verify_owner_root(&root).expect("owner");
        let authority = repo::device_authority::DeviceAuthority {
            owner: api::heddle::api::v1alpha2::OwnerState {
                owner: Some(api::heddle::api::v1alpha2::PrincipalRef {
                    id: owner_id.to_string(),
                }),
                root: Some(root),
                binding: Some(binding),
                version: verified.state_hash().to_vec(),
                ..Default::default()
            },
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        };
        let home = repo::identity::heddle_home_dir();
        repo::device_authority::publish(&home, &authority, chrono::Utc::now().timestamp())
            .expect("publish owner authority");
        repo::identity::link_device_key(
            signer.public_key(),
            &signer.to_pem().expect("device PEM"),
            "api.S",
        )
        .expect("enroll device");
        let bearer = crate::hosted_runtime::root_mint::mint_independent_root(
            crate::hosted_runtime::root_mint::IndependentRootMint {
                seed: &signer.to_seed(),
                subject: "owner@example.test",
                ttl: crate::hosted_runtime::root_mint::ACCOUNT_ROOT_TTL,
                credential_id: Some("device-credential"),
                session_id: None,
                expires_at: None,
            },
        )
        .expect("device bearer");
        let credential = config::credentials::ServerCredential {
            mint_root_attachment: None,
            token: bearer.token.clone(),
            subject: bearer.subject,
            device_id: None,
            credential_id: Some("device-credential".into()),
            private_key_pem: Some(bearer.private_key_pem),
            expires_at: Some(bearer.expires_at.to_rfc3339()),
        };
        retain("api.S", &credential).expect("retain verified bearer");
        let stored = repo::identity::load_device(&repo::identity::device_identity_path())
            .expect("load device")
            .expect("enrolled device");
        assert!(stored.credential_token.as_deref() == Some(bearer.token.as_str()));
    }
}
