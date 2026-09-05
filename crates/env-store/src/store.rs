// SPDX-License-Identifier: Apache-2.0
//! Local confidential-runtime store.
//!
//! Decrypt APIs that accept a [`SoftwareRecipientSecret`] are the provider
//! unwrap boundary. The policy broker calls them after authorizing a request
//! and returns values, never key material. Holding the software secret in
//! the broker process is the explicit weaker-custody fallback — not agent
//! isolation.

use std::fs;
use std::path::{Path, PathBuf};

use crypto::{
    AEAD_AES256_GCM_V1, AeadCiphertext, Dek, Signer, SoftwareRecipientSecret, decrypt_padded,
    encrypt_padded, unwrap_dek, verify_payload_signature, wrap_dek,
};
use heddle_fs_prims::fs_atomic::{
    create_private_dir_all, write_file_atomic, write_file_atomic_secret,
};
use heddle_object_model::object::{Attribution, FacetKind};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::codec::{
    StoredCiphertext, assign_audit_id, assign_lifecycle_id, assign_state_id, audit_signing_payload,
    decode_audit, decode_ciphertext, decode_lifecycle, decode_recipient, decode_ref, decode_state,
    encode_named, encode_recipient, encode_ref, encode_state, lifecycle_signing_payload,
    recipient_endorsement_payload,
};
use crate::error::{Result, EnvStoreError};
use crate::ids::{
    AuditRecordId, LifecycleRecordId, RecipientId, EnvProfileId, EnvProfileVersionId,
};
use crate::types::{
    AuditEventKind, AuditRecord, FacetKindWire, LifecycleRecord, LifecycleStatus, ProfileMetadata,
    ProviderCapability, ENV_STORE_SCHEMA_VERSION, RecipientDescriptor, EnvProfileRef,
    EnvProfileVersion, SignatureBlock, SlotMetadata, SlotRecord, WrappedDekRecord, slot_aad,
    validate_profile_name, validate_slot_name, wrap_aad,
};

const STORE_DIR: &str = "env";
const PROFILES_DIR: &str = "profiles";
const VERSIONS_DIR: &str = "versions";
const LIFECYCLE_DIR: &str = "lifecycle";
const CIPHERTEXT_DIR: &str = "ciphertext";
const RECIPIENTS_DIR: &str = "recipients";
const KEYS_DIR: &str = "keys";
const AUDIT_DIR: &str = "audit";
const IDENTITY_FILE: &str = "identity.msgpack";
const WRAP_ALG: &str = "x25519-hkdf-sha256-aes-256-gcm-v1";

pub struct EnvStore {
    root: PathBuf,
}

pub struct SlotWrite {
    pub name: String,
    pub value: Vec<u8>,
}

impl Drop for SlotWrite {
    fn drop(&mut self) {
        // Plaintext secret material — wipe it, don't leave it in a freed Vec.
        self.value.zeroize();
    }
}

/// The store's pinned signing identity (trust anchor). Every signed record
/// (lifecycle, recipient endorsement, audit) must verify against this key;
/// it is written on first signed use and is immutable thereafter.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PinnedIdentity {
    schema_version: u16,
    algorithm: String,
    #[serde(with = "serde_bytes")]
    public_key: Vec<u8>,
}

impl EnvStore {
    /// Open (or create) the store under `{heddle_dir}/env`.
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        let root = heddle_dir.as_ref().join(STORE_DIR);
        create_private_dir_all(&root)?;
        for child in [
            PROFILES_DIR,
            VERSIONS_DIR,
            LIFECYCLE_DIR,
            CIPHERTEXT_DIR,
            RECIPIENTS_DIR,
            KEYS_DIR,
            AUDIT_DIR,
        ] {
            create_private_dir_all(&root.join(child))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn identity_path(&self) -> PathBuf {
        self.root.join(IDENTITY_FILE)
    }

    /// Pin the store's signing identity on first signed use, or require that a
    /// later signer matches the pinned one. This is the trust anchor: reads
    /// verify every signed record against this key.
    fn pin_or_check_identity(&self, signer: &impl Signer) -> Result<()> {
        match self.load_pinned_identity()? {
            Some(pinned) => {
                if pinned.algorithm != signer.algorithm()
                    || pinned.public_key != signer.public_key()
                {
                    return Err(EnvStoreError::Invalid(
                        "signer does not match the store's pinned identity".to_string(),
                    ));
                }
                Ok(())
            }
            None => {
                let identity = PinnedIdentity {
                    schema_version: ENV_STORE_SCHEMA_VERSION,
                    algorithm: signer.algorithm().to_string(),
                    public_key: signer.public_key().to_vec(),
                };
                let bytes = encode_named(&identity, "heddle-env-identity")?;
                write_file_atomic(&self.identity_path(), &bytes)?;
                Ok(())
            }
        }
    }

    fn load_pinned_identity(&self) -> Result<Option<PinnedIdentity>> {
        match fs::read(self.identity_path()) {
            Ok(bytes) => {
                let identity: PinnedIdentity = rmp_serde::from_slice(&bytes).map_err(|err| {
                    EnvStoreError::Decoding(format!("decode store identity: {err}"))
                })?;
                Ok(Some(identity))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(EnvStoreError::Io(err)),
        }
    }

    /// The pinned identity, or a fail-closed error. A store that holds signed
    /// records but no pinned identity is malformed and must not be trusted.
    fn require_identity(&self) -> Result<PinnedIdentity> {
        self.load_pinned_identity()?.ok_or_else(|| {
            EnvStoreError::Invalid("store has no pinned signing identity".to_string())
        })
    }

    /// Verify a signature block over `payload` against the pinned identity:
    /// integrity (the signature is valid over the bytes) AND authenticity (the
    /// signer is the store's pinned key, not an arbitrary attacker key).
    fn verify_block(
        &self,
        identity: &PinnedIdentity,
        payload: &[u8],
        block: &SignatureBlock,
    ) -> Result<()> {
        if block.algorithm != identity.algorithm || block.public_key != identity.public_key {
            return Err(EnvStoreError::Invalid(
                "signed record is not from the store's pinned identity".to_string(),
            ));
        }
        verify_payload_signature(payload, &block.algorithm, &block.public_key, &block.signature)
            .map_err(EnvStoreError::Signature)
    }

    /// The authoritative lifecycle status of a version, derived from its signed
    /// lifecycle records — NOT the unsigned `lifecycle` field baked into the
    /// version file (which an attacker with store-write access could edit).
    /// Every record is verified against the pinned identity; the latest wins.
    pub(crate) fn effective_lifecycle(
        &self,
        profile_id: EnvProfileId,
        state_id: EnvProfileVersionId,
    ) -> Result<LifecycleStatus> {
        let identity = self.require_identity()?;
        let mut current: Option<LifecycleStatus> = None;
        for entry in fs::read_dir(self.root.join(LIFECYCLE_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let record = decode_lifecycle(&fs::read(entry.path())?)?;
            if record.profile_id != profile_id || record.state_id != state_id {
                continue;
            }
            self.verify_block(
                &identity,
                &lifecycle_signing_payload(&record)?,
                &record.signature,
            )?;
            // Transitions are monotonic, so the most-advanced `to` is current.
            if current.is_none_or(|status| record.to.rank() > status.rank()) {
                current = Some(record.to);
            }
        }
        current.ok_or_else(|| {
            EnvStoreError::Invalid(
                "version has no signed lifecycle record; refusing to trust it".to_string(),
            )
        })
    }

    /// Create a software-exportable recipient and persist its public descriptor
    /// plus the 0600 secret (weaker-custody fallback).
    pub fn create_software_recipient(
        &self,
        signer: &impl Signer,
        key_version: u32,
    ) -> Result<(RecipientDescriptor, SoftwareRecipientSecret)> {
        self.pin_or_check_identity(signer)?;
        let secret = SoftwareRecipientSecret::generate()?;
        let mut descriptor = RecipientDescriptor {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            recipient_id: RecipientId::from_bytes([0; 32]),
            capability: ProviderCapability::SoftwareExportable,
            wrap_alg: WRAP_ALG.to_string(),
            key_version,
            public_key: secret.public_key().to_vec(),
            endorsement: SignatureBlock {
                algorithm: signer.algorithm().to_string(),
                public_key: signer.public_key().to_vec(),
                signature: Vec::new(),
            },
        };
        let payload = recipient_endorsement_payload(&descriptor)?;
        descriptor.endorsement.signature = signer.sign(&payload)?;
        let mut id_bytes = payload;
        id_bytes.extend_from_slice(&descriptor.endorsement.signature);
        descriptor.recipient_id = RecipientId::for_bytes(&id_bytes);
        let bytes = encode_recipient(&descriptor)?;
        write_file_atomic(&self.recipient_path(descriptor.recipient_id), &bytes)?;
        write_file_atomic_secret(&self.key_path(descriptor.recipient_id), &secret.to_bytes())?;
        Ok((descriptor, secret))
    }

    pub fn load_recipient(&self, id: RecipientId) -> Result<RecipientDescriptor> {
        let bytes = fs::read(self.recipient_path(id)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                EnvStoreError::RecipientNotFound(id.to_hex())
            } else {
                EnvStoreError::Io(err)
            }
        })?;
        let descriptor = decode_recipient(&bytes)?;
        // Verify the endorsement against the pinned identity, and that the id
        // is the content hash of the endorsed payload — a forged/edited
        // recipient (e.g. an attacker-chosen wrap public key) is rejected here.
        let identity = self.require_identity()?;
        self.verify_block(
            &identity,
            &recipient_endorsement_payload(&descriptor)?,
            &descriptor.endorsement,
        )?;
        let mut id_bytes = recipient_endorsement_payload(&descriptor)?;
        id_bytes.extend_from_slice(&descriptor.endorsement.signature);
        if descriptor.recipient_id != RecipientId::for_bytes(&id_bytes) {
            return Err(EnvStoreError::Invalid(
                "recipient id does not match its endorsed bytes".to_string(),
            ));
        }
        if descriptor.recipient_id != id {
            return Err(EnvStoreError::Invalid(
                "recipient descriptor id does not match its path".to_string(),
            ));
        }
        Ok(descriptor)
    }

    /// Load the on-disk software secret. Weaker-custody fallback only.
    pub fn load_software_secret(&self, id: RecipientId) -> Result<SoftwareRecipientSecret> {
        crypto::reject_group_or_world_readable_key(&self.key_path(id))?;
        let bytes = fs::read(self.key_path(id)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                EnvStoreError::RecipientNotFound(id.to_hex())
            } else {
                EnvStoreError::Io(err)
            }
        })?;
        let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            EnvStoreError::Invalid("software recipient secret must be 32 bytes".to_string())
        })?;
        Ok(SoftwareRecipientSecret::from_bytes(seed))
    }

    pub fn create_profile(
        &self,
        name: &str,
        slots: Vec<SlotWrite>,
        recipient_id: RecipientId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        self.create_profile_with_recipients(name, slots, &[recipient_id], attribution, signer)
    }

    /// Create a profile wrapped to more than one recipient (e.g. a device key
    /// plus a recovery key). Each slot's DEK is wrapped to every recipient.
    pub fn create_profile_with_recipients(
        &self,
        name: &str,
        slots: Vec<SlotWrite>,
        recipient_ids: &[RecipientId],
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        validate_profile_name(name).map_err(EnvStoreError::Invalid)?;
        if recipient_ids.is_empty() {
            return Err(EnvStoreError::Invalid(
                "a env store requires at least one recipient".to_string(),
            ));
        }
        let recipients = recipient_ids
            .iter()
            .map(|id| self.load_recipient(*id))
            .collect::<Result<Vec<_>>>()?;
        let now = now_ms()?;
        let profile_id = EnvProfileId::generate();
        let (state, _) = self.write_version(
            profile_id,
            None,
            1,
            slots,
            &recipients,
            attribution.clone(),
            now,
        )?;
        self.record_lifecycle(
            profile_id,
            state.state_id,
            None,
            LifecycleStatus::Staged,
            now,
            attribution.clone(),
            signer,
        )?;
        self.record_lifecycle(
            profile_id,
            state.state_id,
            Some(LifecycleStatus::Staged),
            LifecycleStatus::Active,
            now,
            attribution.clone(),
            signer,
        )?;
        let mut active = state;
        active.lifecycle = LifecycleStatus::Active;
        write_file_atomic(&self.version_path(active.state_id), &encode_state(&active)?)?;
        let profile = EnvProfileRef {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            profile_id,
            name: name.to_string(),
            facet: FacetKindWire::ConfidentialRuntime,
            head: active.state_id,
            created_at_ms: now,
            updated_at_ms: now,
            attribution,
        };
        write_file_atomic(&self.profile_path(profile_id), &encode_ref(&profile)?)?;
        Ok(profile)
    }

    pub fn update_slots(
        &self,
        profile_id: EnvProfileId,
        slots: Vec<SlotWrite>,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        let mut profile = self.load_profile(profile_id)?;
        let previous = self.load_state(profile.head)?;
        if previous.lifecycle != LifecycleStatus::Active {
            return Err(EnvStoreError::IllegalLifecycle {
                from: previous.lifecycle.to_string(),
                to: LifecycleStatus::Superseded.to_string(),
            });
        }
        let recipients = previous
            .recipient_ids
            .iter()
            .map(|id| self.load_recipient(*id))
            .collect::<Result<Vec<_>>>()?;
        let now = now_ms()?;
        let (staged, _) = self.write_version(
            profile_id,
            Some(previous.state_id),
            previous.version + 1,
            slots,
            &recipients,
            attribution.clone(),
            now,
        )?;
        // Crash-safe ordering: bring the new version fully Active and flip head
        // to it FIRST, THEN supersede the old one. A crash between leaves head
        // pointing at a valid Active version (the old one merely stays Active a
        // little longer), never at a Superseded version with no repair path.
        self.record_lifecycle(
            profile_id,
            staged.state_id,
            None,
            LifecycleStatus::Staged,
            now,
            attribution.clone(),
            signer,
        )?;
        self.record_lifecycle(
            profile_id,
            staged.state_id,
            Some(LifecycleStatus::Staged),
            LifecycleStatus::Active,
            now,
            attribution.clone(),
            signer,
        )?;
        let mut active = staged;
        active.lifecycle = LifecycleStatus::Active;
        write_file_atomic(&self.version_path(active.state_id), &encode_state(&active)?)?;
        profile.head = active.state_id;
        profile.updated_at_ms = now;
        profile.attribution = attribution.clone();
        write_file_atomic(&self.profile_path(profile_id), &encode_ref(&profile)?)?;
        self.record_lifecycle(
            profile_id,
            previous.state_id,
            Some(LifecycleStatus::Active),
            LifecycleStatus::Superseded,
            now,
            attribution,
            signer,
        )?;
        let mut superseded = previous;
        superseded.lifecycle = LifecycleStatus::Superseded;
        write_file_atomic(
            &self.version_path(superseded.state_id),
            &encode_state(&superseded)?,
        )?;
        Ok(profile)
    }

    pub fn find_profile_by_name(&self, name: &str) -> Result<EnvProfileRef> {
        for meta in self.list_profiles()? {
            if meta.name == name {
                return self.load_profile(meta.profile_id);
            }
        }
        Err(EnvStoreError::ProfileNotFound(name.to_string()))
    }

    pub fn load_profile(&self, profile_id: EnvProfileId) -> Result<EnvProfileRef> {
        let bytes = fs::read(self.profile_path(profile_id)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                EnvStoreError::ProfileNotFound(profile_id.to_string())
            } else {
                EnvStoreError::Io(err)
            }
        })?;
        decode_ref(&bytes)
    }

    pub fn load_state(&self, state_id: EnvProfileVersionId) -> Result<EnvProfileVersion> {
        let bytes = fs::read(self.version_path(state_id))?;
        decode_state(&bytes)
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileMetadata>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(PROFILES_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let profile = decode_ref(&fs::read(entry.path())?)?;
            let state = self.load_state(profile.head)?;
            out.push(ProfileMetadata {
                profile_id: profile.profile_id,
                name: profile.name,
                facet: profile.facet.facet_kind(),
                head: profile.head,
                version: state.version,
                lifecycle: state.lifecycle,
                slot_names: state.slots.iter().map(|slot| slot.name.clone()).collect(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// List slot metadata for the current head. Does not decrypt.
    pub fn list_slots(&self, profile_id: EnvProfileId) -> Result<Vec<SlotMetadata>> {
        let profile = self.load_profile(profile_id)?;
        let state = self.load_state(profile.head)?;
        Ok(state
            .slots
            .into_iter()
            .map(|slot| SlotMetadata {
                name: slot.name,
                aead_alg: slot.aead_alg,
                pad_bucket: slot.pad_bucket,
                ciphertext_id: slot.ciphertext_id,
                recipient_ids: slot
                    .dek_wraps
                    .into_iter()
                    .map(|wrap| wrap.recipient_id)
                    .collect(),
            })
            .collect())
    }

    pub fn list_recipients(&self) -> Result<Vec<RecipientDescriptor>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(RECIPIENTS_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            out.push(decode_recipient(&fs::read(entry.path())?)?);
        }
        Ok(out)
    }

    /// First software recipient, or create one. Used by CLI setup.
    pub fn default_or_create_software_recipient(
        &self,
        signer: &impl Signer,
    ) -> Result<RecipientDescriptor> {
        if let Some(existing) = self.list_recipients()?.into_iter().next() {
            return Ok(existing);
        }
        let (descriptor, _) = self.create_software_recipient(signer, 1)?;
        Ok(descriptor)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_audit(
        &self,
        profile_id: Option<EnvProfileId>,
        profile_name: &str,
        state_id: Option<EnvProfileVersionId>,
        slots: &[String],
        purpose: &str,
        caller: &str,
        event: AuditEventKind,
        reason: Option<String>,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<AuditRecordId> {
        self.pin_or_check_identity(signer)?;
        let now = now_ms()?;
        let mut record = AuditRecord {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            record_id: AuditRecordId::from_bytes([0; 32]),
            profile_id,
            profile_name: profile_name.to_string(),
            state_id,
            slots: slots.to_vec(),
            purpose: purpose.to_string(),
            caller: caller.to_string(),
            event,
            reason,
            occurred_at_ms: now,
            attribution,
            signature: SignatureBlock {
                algorithm: signer.algorithm().to_string(),
                public_key: signer.public_key().to_vec(),
                signature: Vec::new(),
            },
        };
        let payload = audit_signing_payload(&record)?;
        record.signature.signature = signer.sign(&payload)?;
        let bytes = assign_audit_id(&mut record)?;
        write_file_atomic(&self.audit_path(record.record_id), &bytes)?;
        Ok(record.record_id)
    }

    pub fn list_audit(&self) -> Result<Vec<AuditRecord>> {
        let identity = self.require_identity()?;
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(AUDIT_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let record = decode_audit(&fs::read(entry.path())?)?;
            self.verify_block(&identity, &audit_signing_payload(&record)?, &record.signature)?;
            out.push(record);
        }
        out.sort_by_key(|record| record.occurred_at_ms);
        Ok(out)
    }

    pub fn list_lifecycle(&self, profile_id: EnvProfileId) -> Result<Vec<LifecycleRecord>> {
        let identity = self.require_identity()?;
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(LIFECYCLE_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let record = decode_lifecycle(&fs::read(entry.path())?)?;
            if record.profile_id == profile_id {
                self.verify_block(
                    &identity,
                    &lifecycle_signing_payload(&record)?,
                    &record.signature,
                )?;
                out.push(record);
            }
        }
        out.sort_by_key(|record| record.occurred_at_ms);
        Ok(out)
    }

    /// Provider unwrap boundary. Broker comes next: callers that hold
    /// [`SoftwareRecipientSecret`] are the weaker-custody fallback.
    pub fn decrypt_slot(
        &self,
        profile_id: EnvProfileId,
        slot_name: &str,
        recipient_id: RecipientId,
        recipient: &SoftwareRecipientSecret,
    ) -> Result<Vec<u8>> {
        let profile = self.load_profile(profile_id)?;
        self.decrypt_slot_in_state(profile.head, slot_name, recipient_id, recipient)
    }

    pub fn decrypt_slot_in_state(
        &self,
        state_id: EnvProfileVersionId,
        slot_name: &str,
        recipient_id: RecipientId,
        recipient: &SoftwareRecipientSecret,
    ) -> Result<Vec<u8>> {
        let state = self.load_state(state_id)?;
        // Gate on the authoritative signed lifecycle, NOT the unsigned field
        // baked into the version file.
        let lifecycle = self.effective_lifecycle(state.profile_id, state_id)?;
        if !lifecycle.decrypt_allowed() {
            return Err(EnvStoreError::DecryptForbidden(lifecycle.to_string()));
        }
        let slot = state
            .slots
            .iter()
            .find(|slot| slot.name == slot_name)
            .ok_or_else(|| EnvStoreError::SlotNotFound(slot_name.to_string()))?;
        // Select the wrap for THIS recipient — never blindly the first wrap,
        // or a multi-recipient profile only ever decrypts via recipient[0].
        let wrap = slot
            .dek_wraps
            .iter()
            .find(|wrap| wrap.recipient_id == recipient_id)
            .ok_or_else(|| {
                EnvStoreError::Invalid(format!(
                    "slot {slot_name} has no wrap for the presented recipient"
                ))
            })?;
        let wrapped = crypto::WrappedDek {
            ephemeral_public: vec_to_array(&wrap.ephemeral_public)?,
            nonce: vec_to_array(&wrap.nonce)?,
            ciphertext: wrap.ciphertext.clone(),
        };
        let dek = unwrap_dek(
            &wrapped,
            recipient,
            &wrap_aad(recipient_id, state.profile_id, slot_name, state.version),
        )?;
        let stored = decode_ciphertext(&fs::read(self.ciphertext_path(slot.ciphertext_id))?)?;
        if stored.ciphertext_id != slot.ciphertext_id {
            return Err(EnvStoreError::Invalid(
                "stored ciphertext id does not match the slot record".to_string(),
            ));
        }
        let nonce: [u8; 12] = vec_to_array(&stored.nonce)?;
        let sealed = AeadCiphertext {
            alg: AEAD_AES256_GCM_V1,
            nonce,
            ciphertext: stored.ciphertext,
            pad_bucket: stored.pad_bucket,
        };
        Ok(decrypt_padded(
            &dek,
            &sealed,
            &slot_aad(state.profile_id, slot_name),
        )?)
    }

    /// Revoke the whole credential, not just its head. Every Active or
    /// Superseded version of the profile is transitioned to `Revoked` (each
    /// with a signed lifecycle record), so a revoked secret's earlier versions
    /// stop decrypting too — not just the current head.
    pub fn revoke(
        &self,
        profile_id: EnvProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        let mut profile = self.load_profile(profile_id)?;
        let now = now_ms()?;
        let mut revoked_any = false;
        for state_id in self.version_ids_for_profile(profile_id)? {
            let current = self.effective_lifecycle(profile_id, state_id)?;
            if !matches!(current, LifecycleStatus::Active | LifecycleStatus::Superseded) {
                continue;
            }
            self.record_lifecycle(
                profile_id,
                state_id,
                Some(current),
                LifecycleStatus::Revoked,
                now,
                attribution.clone(),
                signer,
            )?;
            let mut state = self.load_state(state_id)?;
            state.lifecycle = LifecycleStatus::Revoked;
            write_file_atomic(&self.version_path(state_id), &encode_state(&state)?)?;
            revoked_any = true;
        }
        if !revoked_any {
            let head = self.effective_lifecycle(profile_id, profile.head)?;
            return Err(EnvStoreError::IllegalLifecycle {
                from: head.to_string(),
                to: LifecycleStatus::Revoked.to_string(),
            });
        }
        profile.updated_at_ms = now;
        profile.attribution = attribution;
        write_file_atomic(&self.profile_path(profile_id), &encode_ref(&profile)?)?;
        Ok(profile)
    }

    /// All version ids belonging to `profile_id`, from the versions directory.
    fn version_ids_for_profile(
        &self,
        profile_id: EnvProfileId,
    ) -> Result<Vec<EnvProfileVersionId>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(VERSIONS_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let state = decode_state(&fs::read(entry.path())?)?;
            if state.profile_id == profile_id {
                out.push(state.state_id);
            }
        }
        Ok(out)
    }

    pub fn mark_purge_eligible(
        &self,
        profile_id: EnvProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        self.advance_head_lifecycle(
            profile_id,
            LifecycleStatus::PurgeEligible,
            attribution,
            signer,
        )
    }

    /// Delete ciphertext bytes for the current head and mark it purged.
    pub fn purge(
        &self,
        profile_id: EnvProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        let profile = self.load_profile(profile_id)?;
        let state = self.load_state(profile.head)?;
        if state.lifecycle != LifecycleStatus::PurgeEligible {
            return Err(EnvStoreError::IllegalLifecycle {
                from: state.lifecycle.to_string(),
                to: LifecycleStatus::Purged.to_string(),
            });
        }
        for slot in &state.slots {
            let path = self.ciphertext_path(slot.ciphertext_id);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        self.advance_head_lifecycle(profile_id, LifecycleStatus::Purged, attribution, signer)
    }

    fn advance_head_lifecycle(
        &self,
        profile_id: EnvProfileId,
        to: LifecycleStatus,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<EnvProfileRef> {
        let mut profile = self.load_profile(profile_id)?;
        let mut state = self.load_state(profile.head)?;
        if !state.lifecycle.can_transition_to(to) {
            return Err(EnvStoreError::IllegalLifecycle {
                from: state.lifecycle.to_string(),
                to: to.to_string(),
            });
        }
        let now = now_ms()?;
        self.record_lifecycle(
            profile_id,
            state.state_id,
            Some(state.lifecycle),
            to,
            now,
            attribution.clone(),
            signer,
        )?;
        state.lifecycle = to;
        write_file_atomic(&self.version_path(state.state_id), &encode_state(&state)?)?;
        profile.head = state.state_id;
        profile.updated_at_ms = now;
        profile.attribution = attribution;
        write_file_atomic(&self.profile_path(profile_id), &encode_ref(&profile)?)?;
        Ok(profile)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_version(
        &self,
        profile_id: EnvProfileId,
        parent: Option<EnvProfileVersionId>,
        version: u64,
        slots: Vec<SlotWrite>,
        recipients: &[RecipientDescriptor],
        attribution: Attribution,
        created_at_ms: i64,
    ) -> Result<(EnvProfileVersion, Vec<u8>)> {
        if recipients.is_empty() {
            return Err(EnvStoreError::Invalid(
                "a env store requires at least one recipient".to_string(),
            ));
        }
        let mut slot_records = Vec::with_capacity(slots.len());
        for slot in slots {
            validate_slot_name(&slot.name).map_err(EnvStoreError::Invalid)?;
            let dek = Dek::generate()?;
            let sealed = encrypt_padded(&dek, &slot.value, &slot_aad(profile_id, &slot.name))?;
            let (_stored, ciphertext_id, cipher_bytes) = StoredCiphertext::from_aead(&sealed)?;
            write_file_atomic(&self.ciphertext_path(ciphertext_id), &cipher_bytes)?;
            let mut wraps = Vec::new();
            for recipient in recipients {
                let public: [u8; 32] = vec_to_array(&recipient.public_key)?;
                let wrapped = wrap_dek(
                    &dek,
                    &public,
                    &wrap_aad(recipient.recipient_id, profile_id, &slot.name, version),
                )?;
                wraps.push(WrappedDekRecord {
                    recipient_id: recipient.recipient_id,
                    ephemeral_public: wrapped.ephemeral_public.to_vec(),
                    nonce: wrapped.nonce.to_vec(),
                    ciphertext: wrapped.ciphertext,
                });
            }
            slot_records.push(SlotRecord {
                name: slot.name.clone(),
                aead_alg: AEAD_AES256_GCM_V1.to_string(),
                pad_bucket: sealed.pad_bucket,
                ciphertext_id,
                dek_wraps: wraps,
            });
        }
        slot_records.sort_by(|a, b| a.name.cmp(&b.name));
        let mut state = EnvProfileVersion {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            state_id: EnvProfileVersionId::from_bytes([0; 32]),
            profile_id,
            parent,
            version,
            lifecycle: LifecycleStatus::Staged,
            slots: slot_records,
            recipient_ids: recipients.iter().map(|r| r.recipient_id).collect(),
            policy_ref: None,
            created_at_ms,
            attribution,
        };
        let bytes = assign_state_id(&mut state)?;
        write_file_atomic(&self.version_path(state.state_id), &bytes)?;
        Ok((state, bytes))
    }

    #[allow(clippy::too_many_arguments)]
    fn record_lifecycle(
        &self,
        profile_id: EnvProfileId,
        state_id: EnvProfileVersionId,
        from: Option<LifecycleStatus>,
        to: LifecycleStatus,
        occurred_at_ms: i64,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<LifecycleRecordId> {
        if let Some(from) = from
            && !from.can_transition_to(to)
        {
            return Err(EnvStoreError::IllegalLifecycle {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        self.pin_or_check_identity(signer)?;
        let mut record = LifecycleRecord {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            record_id: LifecycleRecordId::from_bytes([0; 32]),
            profile_id,
            state_id,
            from,
            to,
            occurred_at_ms,
            attribution,
            signature: SignatureBlock {
                algorithm: signer.algorithm().to_string(),
                public_key: signer.public_key().to_vec(),
                signature: Vec::new(),
            },
        };
        let payload = lifecycle_signing_payload(&record)?;
        record.signature.signature = signer.sign(&payload)?;
        let bytes = assign_lifecycle_id(&mut record)?;
        write_file_atomic(&self.lifecycle_path(record.record_id), &bytes)?;
        Ok(record.record_id)
    }

    fn profile_path(&self, id: EnvProfileId) -> PathBuf {
        self.root.join(PROFILES_DIR).join(format!("{id}.msgpack"))
    }

    fn version_path(&self, id: EnvProfileVersionId) -> PathBuf {
        self.root
            .join(VERSIONS_DIR)
            .join(format!("{}.msgpack", id.to_hex()))
    }

    fn lifecycle_path(&self, id: LifecycleRecordId) -> PathBuf {
        self.root
            .join(LIFECYCLE_DIR)
            .join(format!("{}.msgpack", id.to_hex()))
    }

    fn ciphertext_path(&self, id: crate::ids::CiphertextId) -> PathBuf {
        self.root
            .join(CIPHERTEXT_DIR)
            .join(format!("{}.msgpack", id.to_hex()))
    }

    fn recipient_path(&self, id: RecipientId) -> PathBuf {
        self.root
            .join(RECIPIENTS_DIR)
            .join(format!("{}.msgpack", id.to_hex()))
    }

    fn key_path(&self, id: RecipientId) -> PathBuf {
        self.root.join(KEYS_DIR).join(id.to_hex())
    }

    fn audit_path(&self, id: AuditRecordId) -> PathBuf {
        self.root
            .join(AUDIT_DIR)
            .join(format!("{}.msgpack", id.to_hex()))
    }
}

/// Compile-time proof that this facet cannot be selected by Source History
/// verbs. `EnvProfileVersionId` is a distinct type from `StateId`.
pub const fn confidential_runtime_source_history_laws()
-> Option<heddle_object_model::object::SourceHistoryLaws> {
    FacetKind::ConfidentialRuntime.source_history_laws()
}

const _: () = assert!(confidential_runtime_source_history_laws().is_none());
const _: () = assert!(!FacetKind::ConfidentialRuntime.git_projection_visits());
const _: () = assert!(!FacetKind::ConfidentialRuntime.may_checkout());
const _: () = assert!(!FacetKind::ConfidentialRuntime.may_land());

fn vec_to_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| {
        EnvStoreError::Invalid(format!("expected {N} bytes, found {}", bytes.len()))
    })
}

pub(crate) fn now_ms() -> Result<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| EnvStoreError::Invalid(format!("system clock before epoch: {err}")))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| EnvStoreError::Invalid("timestamp overflow".to_string()))
}
