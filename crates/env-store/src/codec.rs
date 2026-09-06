// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::error::{Result, EnvStoreError};
use crate::ids::{
    AuditRecordId, CiphertextId, LifecycleRecordId, RecipientId, EnvProfileId,
    EnvProfileVersionId,
};
use crate::types::{
    AuditRecord, FacetKindWire, LifecycleRecord, LifecycleStatus, ProviderCapability,
    ENV_STORE_SCHEMA_VERSION, RecipientDescriptor, EnvProfileRef, EnvProfileVersion,
    SlotRecord,
};

#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u16,
}

fn require_v1(bytes: &[u8], what: &str) -> Result<()> {
    let probe: VersionProbe = rmp_serde::from_slice(bytes)
        .map_err(|err| EnvStoreError::Decoding(format!("decode {what} version: {err}")))?;
    if probe.schema_version != ENV_STORE_SCHEMA_VERSION {
        return Err(EnvStoreError::UnsupportedVersion(
            probe.schema_version,
        ));
    }
    Ok(())
}

pub fn encode_named<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(value)
        .map_err(|err| EnvStoreError::Encoding(format!("encode {what}: {err}")))
}

pub fn decode_ref(bytes: &[u8]) -> Result<EnvProfileRef> {
    require_v1(bytes, "heddle-env-ref")?;
    let decoded: EnvProfileRef = rmp_serde::from_slice(bytes).map_err(|err| {
        EnvStoreError::Decoding(format!("decode heddle-env-ref: {err}"))
    })?;
    if decoded.facet != FacetKindWire::ConfidentialRuntime {
        return Err(EnvStoreError::Invalid(
            "env store facet must be confidential-runtime".to_string(),
        ));
    }
    Ok(decoded)
}

pub fn decode_state(bytes: &[u8]) -> Result<EnvProfileVersion> {
    require_v1(bytes, "heddle-env-state")?;
    let decoded: EnvProfileVersion = rmp_serde::from_slice(bytes).map_err(|err| {
        EnvStoreError::Decoding(format!("decode heddle-env-state: {err}"))
    })?;
    let expected = EnvProfileVersionId::for_bytes(&state_id_payload(&decoded)?);
    if decoded.state_id != expected {
        return Err(EnvStoreError::Invalid(
            "env-store state id does not match canonical bytes".to_string(),
        ));
    }
    Ok(decoded)
}

/// Canonical bytes hashed into `EnvProfileVersionId`.
///
/// Lifecycle is excluded: signed lifecycle records advance status on an
/// immutable version without minting a new identity.
pub fn state_id_payload(state: &EnvProfileVersion) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct StateIdentity<'a> {
        schema_version: u16,
        profile_id: EnvProfileId,
        parent: Option<EnvProfileVersionId>,
        version: u64,
        slots: &'a [SlotRecord],
        recipient_ids: &'a [RecipientId],
        policy_ref: &'a Option<String>,
        created_at_ms: i64,
        attribution: &'a heddle_object_model::object::Attribution,
    }
    encode_named(
        &StateIdentity {
            schema_version: state.schema_version,
            profile_id: state.profile_id,
            parent: state.parent,
            version: state.version,
            slots: &state.slots,
            recipient_ids: &state.recipient_ids,
            policy_ref: &state.policy_ref,
            created_at_ms: state.created_at_ms,
            attribution: &state.attribution,
        },
        "heddle-env-state-identity",
    )
}

pub fn decode_recipient(bytes: &[u8]) -> Result<RecipientDescriptor> {
    require_v1(bytes, "heddle-env-recipient")?;
    rmp_serde::from_slice(bytes).map_err(|err| {
        EnvStoreError::Decoding(format!("decode heddle-env-recipient: {err}"))
    })
}

pub fn recipient_endorsement_payload(descriptor: &RecipientDescriptor) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Body<'a> {
        schema_version: u16,
        capability: ProviderCapability,
        wrap_alg: &'a str,
        key_version: u32,
        #[serde(with = "serde_bytes")]
        public_key: &'a [u8],
    }
    let mut payload = crate::types::RECIPIENT_ENDORSE_DOMAIN.to_vec();
    payload.extend_from_slice(&encode_named(
        &Body {
            schema_version: descriptor.schema_version,
            capability: descriptor.capability,
            wrap_alg: &descriptor.wrap_alg,
            key_version: descriptor.key_version,
            public_key: &descriptor.public_key,
        },
        "recipient-endorsement",
    )?);
    Ok(payload)
}

pub fn lifecycle_signing_payload(record: &LifecycleRecord) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Body {
        schema_version: u16,
        profile_id: EnvProfileId,
        state_id: EnvProfileVersionId,
        from: Option<LifecycleStatus>,
        to: LifecycleStatus,
        occurred_at_ms: i64,
        attribution: heddle_object_model::object::Attribution,
    }
    let mut payload = crate::types::LIFECYCLE_SIGNING_DOMAIN.to_vec();
    payload.extend_from_slice(&encode_named(
        &Body {
            schema_version: record.schema_version,
            profile_id: record.profile_id,
            state_id: record.state_id,
            from: record.from,
            to: record.to,
            occurred_at_ms: record.occurred_at_ms,
            attribution: record.attribution.clone(),
        },
        "lifecycle-signing",
    )?);
    Ok(payload)
}

pub fn decode_lifecycle(bytes: &[u8]) -> Result<LifecycleRecord> {
    require_v1(bytes, "heddle-env-lifecycle")?;
    let decoded: LifecycleRecord = rmp_serde::from_slice(bytes).map_err(|err| {
        EnvStoreError::Decoding(format!("decode heddle-env-lifecycle: {err}"))
    })?;
    Ok(decoded)
}

pub fn decode_ciphertext(bytes: &[u8]) -> Result<StoredCiphertext> {
    require_v1(bytes, "heddle-env-ciphertext")?;
    let decoded: StoredCiphertext = rmp_serde::from_slice(bytes).map_err(|err| {
        EnvStoreError::Decoding(format!("decode heddle-env-ciphertext: {err}"))
    })?;
    let expected = CiphertextId::for_bytes(&ciphertext_id_payload(&decoded)?);
    if decoded.ciphertext_id != expected {
        return Err(EnvStoreError::Invalid(
            "ciphertext id does not match canonical bytes".to_string(),
        ));
    }
    Ok(decoded)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCiphertext {
    pub schema_version: u16,
    pub ciphertext_id: CiphertextId,
    pub alg: String,
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub ciphertext: Vec<u8>,
    pub pad_bucket: u32,
}

impl StoredCiphertext {
    pub fn from_aead(sealed: &crypto::AeadCiphertext) -> Result<(Self, CiphertextId, Vec<u8>)> {
        let mut provisional = Self {
            schema_version: ENV_STORE_SCHEMA_VERSION,
            ciphertext_id: CiphertextId::from_bytes([0; 32]),
            alg: sealed.alg.to_string(),
            nonce: sealed.nonce.to_vec(),
            ciphertext: sealed.ciphertext.clone(),
            pad_bucket: sealed.pad_bucket,
        };
        let identity = ciphertext_id_payload(&provisional)?;
        let id = CiphertextId::for_bytes(&identity);
        provisional.ciphertext_id = id;
        let bytes = encode_named(&provisional, "heddle-env-ciphertext")?;
        Ok((provisional, id, bytes))
    }
}

fn ciphertext_id_payload(value: &StoredCiphertext) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Body<'a> {
        schema_version: u16,
        alg: &'a str,
        #[serde(with = "serde_bytes")]
        nonce: &'a [u8],
        #[serde(with = "serde_bytes")]
        ciphertext: &'a [u8],
        pad_bucket: u32,
    }
    encode_named(
        &Body {
            schema_version: value.schema_version,
            alg: &value.alg,
            nonce: &value.nonce,
            ciphertext: &value.ciphertext,
            pad_bucket: value.pad_bucket,
        },
        "ciphertext-identity",
    )
}

pub fn encode_ref(value: &EnvProfileRef) -> Result<Vec<u8>> {
    encode_named(value, "heddle-env-ref")
}

pub fn encode_state(value: &EnvProfileVersion) -> Result<Vec<u8>> {
    encode_named(value, "heddle-env-state")
}

pub fn encode_recipient(value: &RecipientDescriptor) -> Result<Vec<u8>> {
    encode_named(value, "heddle-env-recipient")
}

pub fn encode_lifecycle(value: &LifecycleRecord) -> Result<Vec<u8>> {
    encode_named(value, "heddle-env-lifecycle")
}

pub fn audit_signing_payload(record: &AuditRecord) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Body<'a> {
        schema_version: u16,
        profile_id: Option<EnvProfileId>,
        profile_name: &'a str,
        state_id: Option<EnvProfileVersionId>,
        slots: &'a [String],
        purpose: &'a str,
        caller: &'a str,
        event: crate::types::AuditEventKind,
        reason: &'a Option<String>,
        occurred_at_ms: i64,
        attribution: heddle_object_model::object::Attribution,
    }
    let mut payload = crate::types::AUDIT_SIGNING_DOMAIN.to_vec();
    payload.extend_from_slice(&encode_named(
        &Body {
            schema_version: record.schema_version,
            profile_id: record.profile_id,
            profile_name: &record.profile_name,
            state_id: record.state_id,
            slots: &record.slots,
            purpose: &record.purpose,
            caller: &record.caller,
            event: record.event,
            reason: &record.reason,
            occurred_at_ms: record.occurred_at_ms,
            attribution: record.attribution.clone(),
        },
        "audit-signing",
    )?);
    Ok(payload)
}

pub fn decode_audit(bytes: &[u8]) -> Result<AuditRecord> {
    require_v1(bytes, "heddle-env-audit")?;
    rmp_serde::from_slice(bytes)
        .map_err(|err| EnvStoreError::Decoding(format!("decode heddle-env-audit: {err}")))
}

pub fn encode_audit(value: &AuditRecord) -> Result<Vec<u8>> {
    encode_named(value, "heddle-env-audit")
}

pub fn assign_audit_id(record: &mut AuditRecord) -> Result<Vec<u8>> {
    let payload = audit_signing_payload(record)?;
    record.record_id = AuditRecordId::for_bytes(&payload);
    encode_audit(record)
}

pub fn assign_lifecycle_id(record: &mut LifecycleRecord) -> Result<Vec<u8>> {
    let payload = lifecycle_signing_payload(record)?;
    record.record_id = LifecycleRecordId::for_bytes(&payload);
    encode_lifecycle(record)
}

pub fn assign_state_id(state: &mut EnvProfileVersion) -> Result<Vec<u8>> {
    let payload = state_id_payload(state)?;
    state.state_id = EnvProfileVersionId::for_bytes(&payload);
    encode_state(state)
}
