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
    encrypt_padded, unwrap_dek, wrap_dek,
};
use heddle_fs_prims::fs_atomic::{
    create_private_dir_all, write_file_atomic, write_file_atomic_secret,
};
use heddle_object_model::object::{Attribution, FacetKind};

use crate::codec::{
    StoredCiphertext, assign_audit_id, assign_lifecycle_id, assign_state_id, audit_signing_payload,
    decode_audit, decode_ciphertext, decode_lifecycle, decode_recipient, decode_ref, decode_state,
    encode_recipient, encode_ref, encode_state, lifecycle_signing_payload,
    recipient_endorsement_payload,
};
use crate::error::{Result, RuntimeProfileError};
use crate::ids::{
    AuditRecordId, LifecycleRecordId, RecipientId, RuntimeProfileId, RuntimeProfileStateId,
};
use crate::types::{
    AuditEventKind, AuditRecord, FacetKindWire, LifecycleRecord, LifecycleStatus, ProfileMetadata,
    ProviderCapability, RUNTIME_PROFILE_SCHEMA_VERSION, RecipientDescriptor, RuntimeProfileRef,
    RuntimeProfileState, SignatureBlock, SlotMetadata, SlotRecord, WrappedDekRecord, slot_aad,
    validate_profile_name, validate_slot_name,
};

const STORE_DIR: &str = "runtime-profiles";
const PROFILES_DIR: &str = "profiles";
const VERSIONS_DIR: &str = "versions";
const LIFECYCLE_DIR: &str = "lifecycle";
const CIPHERTEXT_DIR: &str = "ciphertext";
const RECIPIENTS_DIR: &str = "recipients";
const KEYS_DIR: &str = "keys";
const AUDIT_DIR: &str = "audit";
const WRAP_ALG: &str = "x25519-hkdf-sha256-aes-256-gcm-v1";

pub struct RuntimeProfileStore {
    root: PathBuf,
}

pub struct SlotWrite {
    pub name: String,
    pub value: Vec<u8>,
}

impl RuntimeProfileStore {
    /// Open (or create) the store under `{heddle_dir}/runtime-profiles`.
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

    /// Create a software-exportable recipient and persist its public descriptor
    /// plus the 0600 secret (weaker-custody fallback).
    pub fn create_software_recipient(
        &self,
        signer: &impl Signer,
        key_version: u32,
    ) -> Result<(RecipientDescriptor, SoftwareRecipientSecret)> {
        let secret = SoftwareRecipientSecret::generate()?;
        let mut descriptor = RecipientDescriptor {
            schema_version: RUNTIME_PROFILE_SCHEMA_VERSION,
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
                RuntimeProfileError::RecipientNotFound(id.to_hex())
            } else {
                RuntimeProfileError::Io(err)
            }
        })?;
        decode_recipient(&bytes)
    }

    /// Load the on-disk software secret. Weaker-custody fallback only.
    pub fn load_software_secret(&self, id: RecipientId) -> Result<SoftwareRecipientSecret> {
        crypto::reject_group_or_world_readable_key(&self.key_path(id))?;
        let bytes = fs::read(self.key_path(id)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RuntimeProfileError::RecipientNotFound(id.to_hex())
            } else {
                RuntimeProfileError::Io(err)
            }
        })?;
        let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            RuntimeProfileError::Invalid("software recipient secret must be 32 bytes".to_string())
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
    ) -> Result<RuntimeProfileRef> {
        validate_profile_name(name).map_err(RuntimeProfileError::Invalid)?;
        let recipient = self.load_recipient(recipient_id)?;
        let now = now_ms()?;
        let profile_id = RuntimeProfileId::generate();
        let (state, _) = self.write_version(
            profile_id,
            None,
            1,
            slots,
            &[recipient],
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
        let profile = RuntimeProfileRef {
            schema_version: RUNTIME_PROFILE_SCHEMA_VERSION,
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
        profile_id: RuntimeProfileId,
        slots: Vec<SlotWrite>,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<RuntimeProfileRef> {
        let mut profile = self.load_profile(profile_id)?;
        let previous = self.load_state(profile.head)?;
        if previous.lifecycle != LifecycleStatus::Active {
            return Err(RuntimeProfileError::IllegalLifecycle {
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
            previous.state_id,
            Some(LifecycleStatus::Active),
            LifecycleStatus::Superseded,
            now,
            attribution.clone(),
            signer,
        )?;
        let mut superseded = previous;
        superseded.lifecycle = LifecycleStatus::Superseded;
        write_file_atomic(
            &self.version_path(superseded.state_id),
            &encode_state(&superseded)?,
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
        profile.attribution = attribution;
        write_file_atomic(&self.profile_path(profile_id), &encode_ref(&profile)?)?;
        Ok(profile)
    }

    pub fn find_profile_by_name(&self, name: &str) -> Result<RuntimeProfileRef> {
        for meta in self.list_profiles()? {
            if meta.name == name {
                return self.load_profile(meta.profile_id);
            }
        }
        Err(RuntimeProfileError::ProfileNotFound(name.to_string()))
    }

    pub fn load_profile(&self, profile_id: RuntimeProfileId) -> Result<RuntimeProfileRef> {
        let bytes = fs::read(self.profile_path(profile_id)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RuntimeProfileError::ProfileNotFound(profile_id.to_string())
            } else {
                RuntimeProfileError::Io(err)
            }
        })?;
        decode_ref(&bytes)
    }

    pub fn load_state(&self, state_id: RuntimeProfileStateId) -> Result<RuntimeProfileState> {
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
    pub fn list_slots(&self, profile_id: RuntimeProfileId) -> Result<Vec<SlotMetadata>> {
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
        profile_id: Option<RuntimeProfileId>,
        profile_name: &str,
        state_id: Option<RuntimeProfileStateId>,
        slots: &[String],
        purpose: &str,
        event: AuditEventKind,
        reason: Option<String>,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<AuditRecordId> {
        let now = now_ms()?;
        let mut record = AuditRecord {
            schema_version: RUNTIME_PROFILE_SCHEMA_VERSION,
            record_id: AuditRecordId::from_bytes([0; 32]),
            profile_id,
            profile_name: profile_name.to_string(),
            state_id,
            slots: slots.to_vec(),
            purpose: purpose.to_string(),
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
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(AUDIT_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            out.push(decode_audit(&fs::read(entry.path())?)?);
        }
        out.sort_by_key(|record| record.occurred_at_ms);
        Ok(out)
    }

    pub fn list_lifecycle(&self, profile_id: RuntimeProfileId) -> Result<Vec<LifecycleRecord>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(LIFECYCLE_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let record = decode_lifecycle(&fs::read(entry.path())?)?;
            if record.profile_id == profile_id {
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
        profile_id: RuntimeProfileId,
        slot_name: &str,
        recipient: &SoftwareRecipientSecret,
    ) -> Result<Vec<u8>> {
        let profile = self.load_profile(profile_id)?;
        self.decrypt_slot_in_state(profile.head, slot_name, recipient)
    }

    pub fn decrypt_slot_in_state(
        &self,
        state_id: RuntimeProfileStateId,
        slot_name: &str,
        recipient: &SoftwareRecipientSecret,
    ) -> Result<Vec<u8>> {
        let state = self.load_state(state_id)?;
        if !state.lifecycle.decrypt_allowed() {
            return Err(RuntimeProfileError::DecryptForbidden(
                state.lifecycle.to_string(),
            ));
        }
        let slot = state
            .slots
            .iter()
            .find(|slot| slot.name == slot_name)
            .ok_or_else(|| RuntimeProfileError::SlotNotFound(slot_name.to_string()))?;
        let wrap = slot.dek_wraps.first().ok_or_else(|| {
            RuntimeProfileError::Invalid(format!("slot {slot_name} has no recipient wrap"))
        })?;
        let wrapped = crypto::WrappedDek {
            ephemeral_public: vec_to_array(&wrap.ephemeral_public)?,
            nonce: vec_to_array(&wrap.nonce)?,
            ciphertext: wrap.ciphertext.clone(),
        };
        let dek = unwrap_dek(&wrapped, recipient)?;
        let stored = decode_ciphertext(&fs::read(self.ciphertext_path(slot.ciphertext_id))?)?;
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

    pub fn revoke(
        &self,
        profile_id: RuntimeProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<RuntimeProfileRef> {
        self.advance_head_lifecycle(profile_id, LifecycleStatus::Revoked, attribution, signer)
    }

    pub fn mark_purge_eligible(
        &self,
        profile_id: RuntimeProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<RuntimeProfileRef> {
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
        profile_id: RuntimeProfileId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<RuntimeProfileRef> {
        let profile = self.load_profile(profile_id)?;
        let state = self.load_state(profile.head)?;
        if state.lifecycle != LifecycleStatus::PurgeEligible {
            return Err(RuntimeProfileError::IllegalLifecycle {
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
        profile_id: RuntimeProfileId,
        to: LifecycleStatus,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<RuntimeProfileRef> {
        let mut profile = self.load_profile(profile_id)?;
        let mut state = self.load_state(profile.head)?;
        if !state.lifecycle.can_transition_to(to) {
            return Err(RuntimeProfileError::IllegalLifecycle {
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
        profile_id: RuntimeProfileId,
        parent: Option<RuntimeProfileStateId>,
        version: u64,
        slots: Vec<SlotWrite>,
        recipients: &[RecipientDescriptor],
        attribution: Attribution,
        created_at_ms: i64,
    ) -> Result<(RuntimeProfileState, Vec<u8>)> {
        if recipients.is_empty() {
            return Err(RuntimeProfileError::Invalid(
                "a runtime profile requires at least one recipient".to_string(),
            ));
        }
        let mut slot_records = Vec::with_capacity(slots.len());
        for slot in slots {
            validate_slot_name(&slot.name).map_err(RuntimeProfileError::Invalid)?;
            let dek = Dek::generate()?;
            let sealed = encrypt_padded(&dek, &slot.value, &slot_aad(profile_id, &slot.name))?;
            let (_stored, ciphertext_id, cipher_bytes) = StoredCiphertext::from_aead(&sealed)?;
            write_file_atomic(&self.ciphertext_path(ciphertext_id), &cipher_bytes)?;
            let mut wraps = Vec::new();
            for recipient in recipients {
                let public: [u8; 32] = vec_to_array(&recipient.public_key)?;
                let wrapped = wrap_dek(&dek, &public)?;
                wraps.push(WrappedDekRecord {
                    recipient_id: recipient.recipient_id,
                    ephemeral_public: wrapped.ephemeral_public.to_vec(),
                    nonce: wrapped.nonce.to_vec(),
                    ciphertext: wrapped.ciphertext,
                });
            }
            slot_records.push(SlotRecord {
                name: slot.name,
                aead_alg: AEAD_AES256_GCM_V1.to_string(),
                pad_bucket: sealed.pad_bucket,
                ciphertext_id,
                dek_wraps: wraps,
            });
        }
        slot_records.sort_by(|a, b| a.name.cmp(&b.name));
        let mut state = RuntimeProfileState {
            schema_version: RUNTIME_PROFILE_SCHEMA_VERSION,
            state_id: RuntimeProfileStateId::from_bytes([0; 32]),
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
        profile_id: RuntimeProfileId,
        state_id: RuntimeProfileStateId,
        from: Option<LifecycleStatus>,
        to: LifecycleStatus,
        occurred_at_ms: i64,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<LifecycleRecordId> {
        if let Some(from) = from
            && !from.can_transition_to(to)
        {
            return Err(RuntimeProfileError::IllegalLifecycle {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        let mut record = LifecycleRecord {
            schema_version: RUNTIME_PROFILE_SCHEMA_VERSION,
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

    fn profile_path(&self, id: RuntimeProfileId) -> PathBuf {
        self.root.join(PROFILES_DIR).join(format!("{id}.msgpack"))
    }

    fn version_path(&self, id: RuntimeProfileStateId) -> PathBuf {
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
/// verbs. `RuntimeProfileStateId` is a distinct type from `StateId`.
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
        RuntimeProfileError::Invalid(format!("expected {N} bytes, found {}", bytes.len()))
    })
}

pub(crate) fn now_ms() -> Result<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| RuntimeProfileError::Invalid(format!("system clock before epoch: {err}")))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| RuntimeProfileError::Invalid("timestamp overflow".to_string()))
}
