//! Retain the actual device credential at hosted enrollment/renewal boundaries.
//! Browser requests and installed agent credentials never impersonate this key.
use anyhow::{Context, Result};
use crypto::{Ed25519Signer, Signer};

/// The Biscuit root a retained device credential must chain to.
pub(super) enum CredentialRoot {
    /// Independent-root enrollment: the device key is its own mint root.
    DeviceKey,
    /// A browser-paired credential is the approver's session attenuated to
    /// this device, so it chains to the approver's mint root, never to the
    /// freshly generated device key.
    Paired(VerifiedMintRoot),
}

/// A mint root already checked against the account's owner authority. Only
/// [`VerifiedMintRoot::verify`] constructs one, so a caller cannot hand
/// `retain` a root that no owner authority vouched for.
pub(super) struct VerifiedMintRoot([u8; 32]);

impl VerifiedMintRoot {
    pub(super) fn verify(
        authority: &repo::device_authority::DeviceAuthority,
        public_key: &[u8],
        now: i64,
    ) -> Result<Self> {
        let key: [u8; 32] = public_key.try_into().context("mint root length")?;
        authority.verify_mint_root(&key, now)?;
        Ok(Self(key))
    }
}

/// Retain `credential` as this device's original source authority. The token
/// must verify under `root`, and `source_author::publish` re-checks that root
/// against the locally admitted owner authority and requires the credential's
/// effective proof key (`cnf`) to be this device's key.
pub(super) fn retain(
    server: &str,
    credential: &config::credentials::ServerCredential,
    root: CredentialRoot,
) -> Result<()> {
    let Some(retention) = Retention::of(server, credential)? else {
        return Ok(());
    };
    retention.publish(server, credential, root)
}

/// Retain a credential an earlier login saved. Its root follows from how it
/// was issued, with no fallback between the two: a paired credential chains to
/// the approval root recorded at pairing, verified against the admitted owner
/// authority; every other credential chains to this device's own key.
pub(super) fn retain_stored(
    server: &str,
    credential: &config::credentials::ServerCredential,
) -> Result<()> {
    let Some(retention) = Retention::of(server, credential)? else {
        return Ok(());
    };
    let root = if super::auth_pairing::is_paired(credential) {
        let approved = super::auth_pairing::approved_root(server, credential)?;
        CredentialRoot::Paired(
            VerifiedMintRoot::verify(&retention.authority, &approved, retention.now)
                .context("verify recorded pairing root belongs to the account")?,
        )
    } else {
        CredentialRoot::DeviceKey
    };
    retention.publish(server, credential, root)
}

/// This device's enrolled key and admitted owner authority, present only when
/// `credential` is this device's own credential for `server`.
struct Retention {
    home: std::path::PathBuf,
    authority: repo::device_authority::DeviceAuthority,
    publisher: [u8; 32],
    now: i64,
}

impl Retention {
    fn of(
        server: &str,
        credential: &config::credentials::ServerCredential,
    ) -> Result<Option<Self>> {
        let home = repo::identity::heddle_home_dir();
        let Some(device) = repo::identity::load_device(&repo::identity::device_identity_path())?
        else {
            return Ok(None);
        };
        if !super::hosted::server_keys_match(&device.server, server) {
            return Ok(None);
        }
        let Some(pem) = credential.private_key_pem.as_deref() else {
            return Ok(None);
        };
        let signer = Ed25519Signer::from_pem(pem)?;
        if hex::encode(signer.public_key()) != device.public_key {
            return Ok(None);
        }
        // An imported credential without independently admitted account ownership
        // still captures as an explicit local key; importing does not enroll trust.
        if !home.join("state/device-rpc/authority.bin").try_exists()? {
            return Ok(None);
        }
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(&home, now)?;
        let publisher: [u8; 32] = signer
            .public_key()
            .try_into()
            .context("source publisher length")?;
        Ok(Some(Self {
            home,
            authority,
            publisher,
            now,
        }))
    }

    fn publish(
        self,
        server: &str,
        credential: &config::credentials::ServerCredential,
        root: CredentialRoot,
    ) -> Result<()> {
        let mint_root = match root {
            CredentialRoot::DeviceKey => self.publisher,
            CredentialRoot::Paired(VerifiedMintRoot(key)) => key,
        };
        let trusted =
            biscuit_verifier::PublicKey::from_bytes(&mint_root, biscuit_auth::Algorithm::Ed25519)?;
        let token = biscuit_verifier::parse_token(&credential.token, &[trusted])?;
        repo::identity::source_author::publish(
            &self.home,
            &self.authority,
            &mint_root,
            &self.publisher,
            &token,
            self.now,
        )?;
        repo::identity::retain_device_bearer(
            server,
            &self.publisher,
            &credential.token,
            &credential.subject,
        )?;
        Ok(())
    }
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
        retain("api.S", &credential, CredentialRoot::DeviceKey).expect("retain verified bearer");
        let stored = repo::identity::load_device(&repo::identity::device_identity_path())
            .expect("load device")
            .expect("enrolled device");
        assert!(stored.credential_token.as_deref() == Some(bearer.token.as_str()));
    }
}
