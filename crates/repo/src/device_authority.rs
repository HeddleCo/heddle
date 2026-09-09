//! Locally admitted account authority for offline device RPCs.
//!
//! Only enrollment and authenticated account observation may publish this pin.
//! Incoming device requests cannot bootstrap trust from their own attachments.
use std::{io::Read, path::Path};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::{OwnerState, SignedMintRootAttachment};
use objects::{fs_atomic, lock::RepoLock};
use prost::Message;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 256;
const MAX_REVOCATIONS: usize = 16_384;

pub struct DeviceAuthority {
    pub owner: OwnerState,
    pub mint_roots: Vec<SignedMintRootAttachment>,
    pub revoked_ids: Vec<String>,
    pub revoked_mint_roots: Vec<[u8; 32]>,
    pub revoked_publishers: Vec<[u8; 32]>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredAuthority {
    #[prost(uint32, tag = "1")]
    format: u32,
    #[prost(message, optional, tag = "2")]
    owner: Option<OwnerState>,
    #[prost(message, repeated, tag = "3")]
    mint_roots: Vec<SignedMintRootAttachment>,
    #[prost(string, repeated, tag = "4")]
    revoked_ids: Vec<String>,
    #[prost(bytes = "vec", repeated, tag = "5")]
    revoked_mint_roots: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "6")]
    revoked_publishers: Vec<Vec<u8>>,
}

fn read(home: &Path) -> Result<StoredAuthority> {
    let file = std::fs::File::open(home.join("state/device-rpc/authority.bin"))
        .context("open locally admitted device authority")?;
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        bail!("device authority exceeds size bound");
    }
    StoredAuthority::decode(bytes.as_slice()).context("decode device authority")
}

fn validate(stored: &StoredAuthority, now: i64) -> Result<()> {
    if stored.format != 1
        || stored.encoded_len() > MAX_BYTES
        || stored.mint_roots.len() > MAX_ATTACHMENTS
        || stored.revoked_ids.len() > MAX_REVOCATIONS
        || stored.revoked_mint_roots.len() > MAX_REVOCATIONS
        || stored.revoked_publishers.len() > MAX_REVOCATIONS
        || stored
            .revoked_mint_roots
            .iter()
            .chain(&stored.revoked_publishers)
            .any(|key| key.len() != 32)
        || stored
            .revoked_ids
            .iter()
            .any(|id| id.is_empty() || id.len() > 512)
    {
        bail!("invalid device authority format or bounds");
    }
    let owner = stored
        .owner
        .as_ref()
        .context("device account authority missing")?;
    crate::verify_account_owner_observation(owner, now)?;
    Ok(())
}

/// Load the independently admitted pin. Expired mint attachments remain stored
/// as historical evidence; admission of a presented mint root checks its time.
pub fn load(home: &Path, now: i64) -> Result<DeviceAuthority> {
    let stored = read(home)?;
    validate(&stored, now)?;
    Ok(DeviceAuthority {
        owner: stored.owner.context("device account authority missing")?,
        mint_roots: stored.mint_roots,
        revoked_ids: stored.revoked_ids,
        revoked_mint_roots: keys(stored.revoked_mint_roots)?,
        revoked_publishers: keys(stored.revoked_publishers)?,
    })
}

fn keys(values: Vec<Vec<u8>>) -> Result<Vec<[u8; 32]>> {
    values
        .into_iter()
        .map(|key| {
            key.try_into()
                .map_err(|_| anyhow::anyhow!("revoked key must be 32 bytes"))
        })
        .collect()
}

impl DeviceAuthority {
    /// Current local revocation gate for original operation publishers.
    pub fn verify_publisher(&self, public_key: &[u8]) -> Result<()> {
        if public_key.len() != 32
            || self
                .revoked_publishers
                .iter()
                .any(|key| key.as_slice() == public_key)
        {
            bail!("operation publisher is invalid or revoked");
        }
        Ok(())
    }
    /// Typed revocation callback for the portable original-author verifier.
    pub fn is_revoked(
        &self,
        kind: heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool {
        use heddleco_capability_verifier::thread_control_authority::Revocation;
        match kind {
            Revocation::Credential(id) => self.revoked_ids.iter().any(|value| value == id),
            Revocation::MintRoot(key) => self
                .revoked_mint_roots
                .iter()
                .any(|value| value.as_slice() == key),
            Revocation::Publisher(key) => self
                .revoked_publishers
                .iter()
                .any(|value| value.as_slice() == key),
        }
    }
    /// Resolve only a locally admitted, currently valid association. A request
    /// may not introduce another mint root by supplying a self-signed statement.
    pub fn verify_mint_root(&self, public_key: &[u8], now: i64) -> Result<()> {
        if self
            .revoked_mint_roots
            .iter()
            .any(|key| key.as_slice() == public_key)
        {
            bail!("mint root is revoked");
        }
        let current = crate::verify_account_owner_observation(&self.owner, now)?;
        let account = current
            .signed_root()
            .root
            .as_ref()
            .context("owner root missing")?;
        if current.authority_key().public_key == public_key {
            return Ok(());
        }
        for signed in &self.mint_roots {
            if heddleco_capability_verifier::creation::verify_mint_root_attachment(
                signed,
                &current,
                &account.account_uuid,
                public_key,
                now,
            )
            .is_ok()
            {
                return Ok(());
            }
        }
        bail!("mint root is not attached to locally admitted current account authority")
    }
}

/// Publish an independently authenticated observation. The first observation is
/// admitted by the calling enrollment ceremony; later ones must extend its exact
/// history. Revocations are cumulative, including across repeated observations.
pub fn publish(home: &Path, authority: &DeviceAuthority, now: i64) -> Result<()> {
    let owner = &authority.owner;
    let mint_roots = &authority.mint_roots;
    let revoked_ids = &authority.revoked_ids;
    let directory = home.join("state/device-rpc");
    fs_atomic::create_private_dir_all(&directory)?;
    let _guard = RepoLock::at(directory.join("authority.lock")).write()?;
    let mut next = StoredAuthority {
        format: 1,
        owner: Some(owner.clone()),
        mint_roots: mint_roots.to_vec(),
        revoked_ids: revoked_ids.to_vec(),
        revoked_mint_roots: authority
            .revoked_mint_roots
            .iter()
            .map(|key| key.to_vec())
            .collect(),
        revoked_publishers: authority
            .revoked_publishers
            .iter()
            .map(|key| key.to_vec())
            .collect(),
    };
    validate(&next, now)?;
    let current = crate::verify_account_owner_observation(owner, now)?;
    let account = current
        .signed_root()
        .root
        .as_ref()
        .context("owner root missing")?;
    for signed in mint_roots {
        let key = signed
            .attachment
            .as_ref()
            .and_then(|a| a.mint_root_key.as_ref())
            .context("mint-root attachment key missing")?;
        heddleco_capability_verifier::creation::verify_mint_root_attachment(
            signed,
            &current,
            &account.account_uuid,
            &key.public_key,
            now,
        )?;
    }
    if directory.join("authority.bin").try_exists()? {
        let previous = read(home)?;
        validate(&previous, now)?;
        let old = previous.owner.context("pinned owner missing")?;
        if owner.owner != old.owner
            || owner.root != old.root
            || owner.binding != old.binding
            || !owner
                .accepted_transitions
                .starts_with(&old.accepted_transitions)
        {
            bail!("device account observation rolls back or forks admitted authority");
        }
        next.revoked_ids.extend(previous.revoked_ids);
        next.revoked_mint_roots.extend(previous.revoked_mint_roots);
        next.revoked_publishers.extend(previous.revoked_publishers);
        if owner.version == old.version {
            for attachment in previous.mint_roots {
                if !next.mint_roots.contains(&attachment) {
                    next.mint_roots.push(attachment);
                }
            }
        }
    }
    next.revoked_ids.sort();
    next.revoked_ids.dedup();
    next.revoked_mint_roots.sort();
    next.revoked_mint_roots.dedup();
    next.revoked_publishers.sort();
    next.revoked_publishers.dedup();
    validate(&next, now)?;
    fs_atomic::write_file_atomic_secret(&directory.join("authority.bin"), &next.encode_to_vec())
        .context("persist admitted device account authority")
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer};

    use super::*;

    fn publish(
        home: &Path,
        owner: &OwnerState,
        mint_roots: &[SignedMintRootAttachment],
        revoked_ids: &[String],
        now: i64,
    ) -> Result<()> {
        super::publish(
            home,
            &DeviceAuthority {
                owner: owner.clone(),
                mint_roots: mint_roots.to_vec(),
                revoked_ids: revoked_ids.to_vec(),
                revoked_mint_roots: vec![],
                revoked_publishers: vec![],
            },
            now,
        )
    }
    fn owner(seed: u8) -> (OwnerState, Ed25519Signer) {
        let key = Ed25519Signer::from_seed(&[seed; 32]).expect("authority");
        let recovery = Ed25519Signer::from_seed(&[seed + 1; 32]).expect("recovery");
        let root =
            crate::sign_custodial_owner_root(&key, &recovery, [9; 16], [5; 32]).expect("root");
        let binding = crate::sign_custodial_owner_binding(&key, &root, [6; 32]).expect("binding");
        let state = heddleco_capability_verifier::verify_owner_root(&root).expect("verify");
        (
            OwnerState {
                owner: Some(api::heddle::api::v2alpha1::PrincipalRef {
                    id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                }),
                root: Some(root),
                binding: Some(binding),
                version: state.state_hash().to_vec(),
                ..Default::default()
            },
            key,
        )
    }

    #[test]
    fn account_pin_rejects_replacement_and_preserves_revocations() {
        let home = tempfile::tempdir().expect("home");
        let (original, key) = owner(71);
        publish(home.path(), &original, &[], &["revoked-leaf".into()], 100).expect("admit");
        let (replacement, _) = owner(73);
        let error = publish(home.path(), &replacement, &[], &[], 100).expect_err("root pinned");
        assert!(error.to_string().contains("rolls back or forks"));
        publish(home.path(), &original, &[], &[], 100).expect("same observation");
        let stored = load(home.path(), 100).expect("load");
        assert_eq!(stored.owner, original);
        assert_eq!(stored.revoked_ids, ["revoked-leaf"]);
        stored
            .verify_mint_root(key.public_key(), 100)
            .expect("current root");
        assert!(stored.verify_mint_root(&[88; 32], 100).is_err());
    }

    #[test]
    fn unverified_observation_never_creates_authority_pin() {
        let home = tempfile::tempdir().expect("home");
        let (mut candidate, _) = owner(75);
        candidate.version[0] ^= 1;
        assert!(publish(home.path(), &candidate, &[], &[], 100).is_err());
        assert!(!home.path().join("state/device-rpc/authority.bin").exists());
    }
    #[test]
    fn typed_key_revocations_survive_repeated_owner_publication() {
        use heddleco_capability_verifier::thread_control_authority::Revocation;
        let home = tempfile::tempdir().expect("home");
        let (state, key) = owner(81);
        let mint: [u8; 32] = key.public_key().try_into().expect("Ed25519 root");
        let publisher = [42; 32];
        let authority = DeviceAuthority {
            owner: state.clone(),
            mint_roots: vec![],
            revoked_ids: vec!["session-a".into()],
            revoked_mint_roots: vec![mint],
            revoked_publishers: vec![publisher],
        };
        super::publish(home.path(), &authority, 100).expect("record typed revocations");
        publish(home.path(), &state, &[], &[], 100)
            .expect("repeated observation cannot clear revocations");
        let stored = load(home.path(), 100).expect("persisted authority");
        assert_eq!(stored.revoked_mint_roots, vec![mint]);
        assert_eq!(stored.revoked_publishers, vec![publisher]);
        assert!(
            stored
                .verify_mint_root(&mint, 100)
                .expect_err("revoked current mint")
                .to_string()
                .contains("revoked")
        );
        assert!(
            stored
                .verify_publisher(&publisher)
                .expect_err("revoked publisher")
                .to_string()
                .contains("revoked")
        );
        stored
            .verify_publisher(&[43; 32])
            .expect("independent publisher remains usable");
        assert!(stored.is_revoked(Revocation::Credential("session-a")));
        assert!(stored.is_revoked(Revocation::MintRoot(&mint)));
        assert!(stored.is_revoked(Revocation::Publisher(&publisher)));
        assert!(!stored.is_revoked(Revocation::Credential(&hex::encode(mint))));
        assert!(!stored.is_revoked(Revocation::Publisher(&mint)));
    }
}
