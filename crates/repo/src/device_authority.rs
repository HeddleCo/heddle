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
            if heddleco_capability_verifier::creation::verify_retained_mint_root_attachment(
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
    let previous = if directory.join("authority.bin").try_exists()? {
        let previous = read(home)?;
        validate(&previous, now)?;
        let old = previous.owner.as_ref().context("pinned owner missing")?;
        if owner.owner != old.owner
            || owner.root != old.root
            || owner.binding != old.binding
            || !owner
                .accepted_transitions
                .starts_with(&old.accepted_transitions)
        {
            bail!("device account observation rolls back or forks admitted authority");
        }
        Some(previous)
    } else {
        None
    };
    for signed in mint_roots {
        // A matching durable admission is evidence of the original enrollment;
        // a backdated certificate presented for the first time is not.
        if previous
            .as_ref()
            .is_some_and(|old| old.mint_roots.contains(signed))
        {
            continue;
        }
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
    if let Some(previous) = previous {
        next.revoked_ids.extend(previous.revoked_ids);
        next.revoked_mint_roots.extend(previous.revoked_mint_roots);
        next.revoked_publishers.extend(previous.revoked_publishers);
        // Retained certificates are historical admissions. Actual use checks
        // expiry, recovery, key retirement and cumulative explicit revocations.
        for attachment in previous.mint_roots {
            if !next.mint_roots.contains(&attachment) {
                next.mint_roots.push(attachment);
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
    fn sibling_proof_uses_pinned_owner_without_enrolling_sender() {
        let (owner, key) = owner(91);
        let device = Ed25519Signer::from_seed(&[93; 32]).expect("sibling key");
        let certificate =
            crate::sign_mint_root_attachment(&key, &owner, device.public_key(), 10, 1000, [7; 32])
                .expect("certificate");
        let sender = DeviceAuthority {
            owner: owner.clone(),
            mint_roots: vec![certificate],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        };
        let mut receiver = DeviceAuthority {
            owner,
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        };
        let private = biscuit_auth::PrivateKey::from_bytes(
            &device.to_seed(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("private key");
        let keypair = biscuit_auth::KeyPair::from(&private);
        let token = biscuit_auth::Biscuit::builder()
            .build(&keypair)
            .expect("token")
            .seal()
            .expect("seal");
        let root: [u8; 32] = device.public_key().try_into().expect("root");
        let proof = crate::thread_replication::metadata::prepare_control_authority(
            &sender, &root, &token, 20,
        )
        .expect("portable proof");
        assert!(
            receiver.verify_mint_root(&root, 20).is_err(),
            "sibling was never enrolled"
        );
        assert_eq!(
            receiver
                .verify_presented_authority(&root, &token, &proof, 20)
                .expect("same pinned owner admits sibling"),
            Some(1000)
        );
        assert!(
            receiver.mint_roots.is_empty(),
            "request does not mutate enrollment"
        );
        let other = biscuit_auth::Biscuit::builder()
            .build(&keypair)
            .expect("other token")
            .seal()
            .expect("seal other");
        assert!(
            receiver
                .verify_presented_authority(&root, &other, &proof, 20)
                .is_err(),
            "proof binds exact sealed token"
        );
        assert!(
            receiver
                .verify_presented_authority(&root, &token, &proof, 1001)
                .is_err(),
            "certificate expiration remains effective"
        );
        crate::verify_account_owner_observation(&receiver.owner, 20)
            .expect("prime exact current owner cache");
        let mut changed_owner = receiver.owner.clone();
        changed_owner.version[0] ^= 1;
        assert!(
            crate::verify_account_owner_observation(&changed_owner, 20).is_err(),
            "cached owner verification must bind exact observation bytes"
        );
        receiver.revoked_mint_roots.push(root);
        assert!(
            receiver
                .verify_presented_authority(&root, &token, &proof, 20)
                .is_err(),
            "current local revocation wins"
        );
        receiver.revoked_mint_roots.clear();
        receiver.owner = super::tests::owner(94).0;
        assert!(
            receiver
                .verify_presented_authority(&root, &token, &proof, 20)
                .is_err(),
            "proof cannot replace pinned owner"
        );
    }

    #[test]
    fn owner_observation_memoization_has_bounded_cardinality() {
        let (owner, _) = owner(95);
        for count in 0..140 {
            let mut observation = owner.clone();
            observation.pending_transitions = vec![Default::default(); count];
            crate::verify_account_owner_observation(&observation, 1234)
                .expect("unchanged accepted owner with distinct pending presentation");
        }
        assert!(
            crate::owner_root::owner_observation_cache_entries().expect("cache count") <= 128,
            "owner memoization cardinality remains bounded"
        );
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
    #[test]
    fn paired_independent_device_survives_rotation_but_backdated_enrollment_does_not() {
        use api::heddle::api::v2alpha1::{
            OwnerKeyTransition, OwnerKeyTransitionKind, SignedOwnerKeyTransition,
        };
        let home = tempfile::tempdir().expect("home");
        let (original, old_key) = owner(85);
        let device = Ed25519Signer::from_seed(&[87; 32]).expect("independent device");
        let certificate = crate::sign_mint_root_attachment(
            &old_key,
            &original,
            device.public_key(),
            10,
            1000,
            [1; 32],
        )
        .expect("original enrollment");
        publish(
            home.path(),
            &original,
            std::slice::from_ref(&certificate),
            &[],
            20,
        )
        .expect("enroll device before rotation");
        let previous = crate::verify_account_owner_observation(&original, 100).expect("owner");
        let next_key = Ed25519Signer::from_seed(&[88; 32]).expect("next owner");
        let transition = OwnerKeyTransition {
            format_version: 1,
            owner_id: previous.owner_id().to_vec(),
            previous_state_hash: previous.state_hash().to_vec(),
            sequence: 1,
            kind: OwnerKeyTransitionKind::Rotate as i32,
            next_authority_key: Some(
                crate::ed25519_verification_key(next_key.public_key()).expect("next key"),
            ),
            next_recovery_policy: Some(previous.recovery_policy().clone()),
            valid_from_unix_seconds: 99,
            previous_key_valid_until_unix_seconds: 100,
            nonce: vec![2; 32],
        };
        let body = crate::owner_key_transition_body(&transition).expect("transition bytes");
        let signed = SignedOwnerKeyTransition {
            transition: Some(transition),
            authorizations: vec![
                crate::sign_canonical(&old_key, crate::OWNER_TRANSITION_DOMAIN, &body)
                    .expect("previous owner"),
            ],
            next_authority_key_proof: Some(
                crate::sign_canonical(&next_key, crate::OWNER_TRANSITION_DOMAIN, &body)
                    .expect("new owner"),
            ),
            next_recovery_key_proofs: vec![],
        };
        let current = heddleco_capability_verifier::apply_accepted_transition(
            &previous,
            &signed,
            101,
            heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
                .expect("limits"),
        )
        .expect("verified rotation");
        let mut rotated = original.clone();
        rotated.accepted_transitions.push(signed);
        rotated.version = current.state_hash().to_vec();
        crate::verify_account_owner_observation(&rotated, 101).expect("current verified rotation");
        assert!(
            crate::verify_account_owner_observation(&rotated, 90).is_err(),
            "cached owner verification must bind exact verification clock"
        );
        publish(home.path(), &rotated, &[], &[], 101)
            .expect("publish current owner without recertifying devices");
        let stored = load(home.path(), 101).expect("persisted current owner");
        assert_eq!(
            stored.mint_roots,
            vec![certificate.clone()],
            "prior admission survives observation rotation"
        );
        stored
            .verify_mint_root(device.public_key(), 101)
            .expect("paired device still works offline");
        let private = biscuit_auth::PrivateKey::from_bytes(
            &device.to_seed(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("local device signing authority");
        let keypair = biscuit_auth::KeyPair::from(&private);
        let token = biscuit_auth::Biscuit::builder()
            .build(&keypair)
            .expect("locally minted credential");
        let proof = crate::thread_replication::metadata::prepare_control_authority(
            &stored,
            &device.public_key().try_into().expect("mint key"),
            &token,
            101,
        )
        .expect(
            "retained device can prepare a new local proof after rotation without recertification",
        );
        let envelope = api::heddle::api::v2alpha1::ThreadControlAuthority::decode(proof.as_slice())
            .expect("portable proof");
        assert_eq!(envelope.mint_root_attachment, Some(certificate.clone()));
        assert_eq!(
            envelope.owner.expect("owner history").state_hash,
            current.state_hash()
        );
        assert!(
            stored.verify_mint_root(old_key.public_key(), 101).is_err(),
            "retired owner direct mint is not a retained device"
        );
        let backdated = crate::sign_mint_root_attachment(
            &old_key,
            &original,
            device.public_key(),
            10,
            1000,
            [3; 32],
        )
        .expect("valid signature with false historical claim");
        assert!(
            publish(home.path(), &rotated, &[backdated], &[], 101).is_err(),
            "new record cannot borrow another certificate's retained admission"
        );
        let fresh = tempfile::tempdir().expect("unpaired device");
        assert!(
            publish(fresh.path(), &rotated, &[certificate], &[], 101).is_err(),
            "historical signature alone is not earlier admission"
        );
        let stored = load(home.path(), 101).expect("failed publication kept prior pin");
        assert_eq!(stored.owner, rotated);
        stored
            .verify_mint_root(device.public_key(), 101)
            .expect("failed new admission cannot remove existing device");
    }
}

impl DeviceAuthority {
    /// Admit a sibling device's public attachment against the independently
    /// enrolled owner. This proof never installs an account or a new trust root.
    /// Returns the attachment deadline, if a separate mint key is used.
    pub fn verify_presented_authority(
        &self,
        root_key: &[u8],
        token: &biscuit_auth::Biscuit,
        proof: &[u8],
        now: i64,
    ) -> Result<Option<i64>> {
        let root_key: &[u8; 32] = root_key
            .try_into()
            .context("device mint root must be Ed25519")?;
        let current = crate::verify_account_owner_observation(&self.owner, now)?;
        if proof.is_empty() {
            self.verify_mint_root(root_key, now)?;
            if current.authority_key().public_key == root_key.as_slice() {
                return Ok(None);
            }
            return self
                .mint_roots
                .iter()
                .filter_map(|record| record.attachment.as_ref())
                .filter(|record| {
                    record
                        .mint_root_key
                        .as_ref()
                        .is_some_and(|key| key.public_key == root_key.as_slice())
                })
                .map(|record| record.expires_at_unix_seconds)
                .filter(|expiry| *expiry > now)
                .min()
                .map(Some)
                .context("mint root deadline unavailable");
        }
        if proof.len() > 64 * 1024 {
            bail!("device authority proof exceeds 64KiB");
        }
        let envelope = api::heddle::api::v2alpha1::ThreadControlAuthority::decode(proof)?;
        if envelope.format != 1
            || envelope.encode_to_vec() != proof
            || envelope.mint_root_public_key != root_key.as_slice()
            || envelope.sealed_biscuit != token.to_vec()?
            || !matches!(token.seal(), Err(biscuit_auth::error::Token::AlreadySealed))
        {
            bail!("device authority proof differs from exact sealed request credential");
        }
        let history = envelope
            .owner
            .as_ref()
            .context("device owner history required")?;
        if history.root != self.owner.root
            || !self
                .owner
                .accepted_transitions
                .starts_with(&history.accepted_transitions)
        {
            bail!("device authority proof is not a prefix of locally enrolled owner history");
        }
        let mut observed = self.owner.clone();
        observed.accepted_transitions = history.accepted_transitions.clone();
        observed.version = history.state_hash.clone();
        let original = crate::verify_account_owner_observation(&observed, now)?;
        let account = current
            .signed_root()
            .root
            .as_ref()
            .context("locally enrolled account missing")?;
        let claimed = original
            .signed_root()
            .root
            .as_ref()
            .context("proof account missing")?;
        if current.owner_id() != original.owner_id() || account.account_uuid != claimed.account_uuid
        {
            bail!("device authority proof is not attached to locally enrolled current owner");
        }
        if self.revoked_mint_roots.contains(root_key) {
            bail!("device mint root revoked");
        }
        if current.authority_key().public_key == root_key.as_slice() {
            if envelope.mint_root_attachment.is_some() {
                bail!("direct owner proof has an unrelated attachment");
            }
            return Ok(None);
        }
        let attachment = envelope
            .mint_root_attachment
            .as_ref()
            .context("sibling mint root requires owner certificate")?;
        if self.mint_roots.contains(attachment) {
            heddleco_capability_verifier::creation::verify_retained_mint_root_attachment(
                attachment,
                &current,
                &account.account_uuid,
                root_key,
                now,
            )?;
        } else {
            heddleco_capability_verifier::creation::verify_mint_root_attachment(
                attachment,
                &current,
                &account.account_uuid,
                root_key,
                now,
            )?;
        }
        Ok(Some(
            attachment
                .attachment
                .as_ref()
                .context("mint certificate missing")?
                .expires_at_unix_seconds,
        ))
    }
}
