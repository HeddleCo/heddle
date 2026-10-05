// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::{
    Decision, Denial, PurgeContext, TimelineAcceptanceContext, VerificationLimits,
    conformance::{run_fixture, run_keyring_fixture, run_timeline_fixture, run_transfer_fixture},
    verify_purge_authorization_bytes, verify_timeline_acceptance,
    wire::{
        PurgeOperationSigningBody, SignedSpoolOwnerGenesis, TimelineAdmissionAcceptance,
        TimelineOriginEndorsement,
    },
};

const MAX_OPERATION_BODY_BYTES: usize = 4 * 1024;
const MAX_OWNER_GENESIS_BYTES: usize = crate::creation::MAX_CREATION_PROOF_BYTES + 1024;

fn js_error(error: impl ToString) -> JsError {
    JsError::new(&error.to_string())
}

fn json<T: Serialize>(value: &T) -> Result<String, JsError> {
    serde_json::to_string(value).map_err(js_error)
}

fn object<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let encoded =
        serde_json::to_string(value).map_err(|error| serialization_error(&error.to_string()))?;
    js_sys::JSON::parse(&encoded).map_err(|_| serialization_error("invalid serialized result"))
}

fn serialization_error(message: &str) -> JsValue {
    // Constant property names and ordinary objects cannot fail Reflect::set.
    // Propagate any JS exception instead of panicking if the runtime is altered.
    let value = js_sys::Object::new();
    for (key, text) in [("code", "serialization"), ("message", message)] {
        if let Err(error) = js_sys::Reflect::set(&value, &key.into(), &text.into()) {
            return error;
        }
    }
    value.into()
}

fn verification_error(error: crate::Error) -> JsValue {
    match object(&crate::observed::VerificationError::from(error)) {
        Ok(value) | Err(value) => value,
    }
}

// TryFrom<JsValue> checks the original bigint against the converted integer,
// rejecting ABI truncation. Test typeof first so numbers get our typed error.
fn checked_integer<T: TryFrom<JsValue, Error = JsValue>>(
    value: JsValue,
    field: &str,
) -> Result<T, JsValue> {
    if !value.is_bigint() {
        return Err(verification_error(crate::Error::Invalid(format!(
            "{field} must be a bigint"
        ))));
    }
    T::try_from(value).map_err(|_| {
        verification_error(crate::Error::Invalid(format!(
            "{field} is outside the {} range",
            std::any::type_name::<T>()
        )))
    })
}

#[derive(Serialize)]
struct ImportSummary {
    certificate_digest_hex: String,
}

/// Verify a complete two-party handoff against its exact OwnerHistory witnesses.
#[wasm_bindgen(js_name = verifyOwnershipTransfer, unchecked_return_type = "TransferSummary")]
#[allow(clippy::too_many_arguments)]
pub fn verify_ownership_transfer_binding(
    transfer: &[u8],
    source_history: &[u8],
    destination_history: &[u8],
    resource_uuid: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] expected_sequence: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let expected_sequence: u64 = checked_integer(expected_sequence, "expected_sequence")?;
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    object(
        &crate::observed::verify_ownership_transfer_bytes(
            transfer,
            source_history,
            destination_history,
            resource_uuid,
            expected_sequence,
            now_unix_seconds,
            max_capability_ttl_seconds,
        )
        .map_err(verification_error)?,
    )
}

/// Verify ObserveOwnership.owner.resource_keyring and the observed OwnerState.
#[wasm_bindgen(js_name = verifyResourceKeyring, unchecked_return_type = "ResourceKeyringSummary")]
pub fn verify_resource_keyring_binding(
    keyring: &[u8],
    current_owner: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    object(
        &crate::observed::verify_resource_keyring_bytes(
            keyring,
            current_owner,
            now_unix_seconds,
            max_capability_ttl_seconds,
        )
        .map_err(verification_error)?,
    )
}

/// Verify the full accepted audit chain, including both sides of every handoff.
#[wasm_bindgen(js_name = verifyOwnershipTransferChain, unchecked_return_type = "ResourceKeyringSummary")]
pub fn verify_ownership_transfer_chain_binding(
    keyring: &[u8],
    current_owner: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    verify_resource_keyring_binding(
        keyring,
        current_owner,
        now_unix_seconds,
        max_capability_ttl_seconds,
    )
}

/// Verify self-signed or delegated-creation observed immutable Spool genesis.
#[wasm_bindgen(js_name = verifySpoolOwnerGenesis, unchecked_return_type = "GenesisSummary")]
pub fn verify_spool_owner_genesis_binding(
    genesis: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    object(
        &crate::observed::verify_spool_owner_genesis_bytes(genesis, now_unix_seconds)
            .map_err(verification_error)?,
    )
}

/// Verify complete SpoolEvent.signed_policy records against observed ownership.
#[wasm_bindgen(js_name = verifySignedPolicyChain, unchecked_return_type = "PolicySummary")]
pub fn verify_signed_policy_chain_binding(
    #[wasm_bindgen(unchecked_param_type = "Uint8Array[]")] records: Vec<JsValue>,
    keyring: &[u8],
    current_owner: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    if records.len() > VerificationLimits::MAX_BUNDLE_BYTES {
        return Err(verification_error(crate::Error::TooLarge {
            limit: VerificationLimits::MAX_BUNDLE_BYTES,
        }));
    }
    let mut bytes = Vec::with_capacity(records.len());
    let mut total = 0_usize;
    for record in records {
        let record = record.dyn_into::<js_sys::Uint8Array>().map_err(|_| {
            verification_error(crate::Error::Invalid(
                "policy record must be Uint8Array".into(),
            ))
        })?;
        total = total.saturating_add(record.length() as usize);
        if total > VerificationLimits::MAX_BUNDLE_BYTES {
            return Err(verification_error(crate::Error::TooLarge {
                limit: VerificationLimits::MAX_BUNDLE_BYTES,
            }));
        }
        bytes.push(record.to_vec());
    }
    object(
        &crate::observed::verify_signed_policy_chain_bytes(
            &bytes,
            keyring,
            current_owner,
            now_unix_seconds,
            max_capability_ttl_seconds,
        )
        .map_err(verification_error)?,
    )
}

use crate::canonical::{canonical_message, fixed};

/// Verify typed import delegation against independently selected owner/Spool
/// pins and actual current time. No incoming keyring enrolls its carried root.
/// Key-role exclusions and job associations use bounded canonical JSON arrays
/// of hex keys and [key, logical-job] pairs respectively.
#[wasm_bindgen(js_name = verifyImportDelegation, unchecked_return_type = "ImportSummary")]
#[allow(clippy::too_many_arguments)]
pub fn verify_import_delegation_binding(
    certificate: &[u8],
    permission: &[u8],
    keyring: &[u8],
    accepted_owner_history: &[u8],
    selected_initial_owner_id: &[u8],
    selected_spool_genesis_digest: &[u8],
    forbidden_job_keys_json: &str,
    known_job_associations_json: &str,
    cancelled_ids_json: &str,
    revoked_key_ids_json: &str,
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    let digest = crate::import_delegation::verify_bytes(
        certificate,
        permission,
        keyring,
        accepted_owner_history,
        selected_initial_owner_id,
        selected_spool_genesis_digest,
        forbidden_job_keys_json,
        known_job_associations_json,
        cancelled_ids_json,
        revoked_key_ids_json,
        now_unix_seconds,
        max_capability_ttl_seconds,
    )
    .map_err(verification_error)?;
    object(&ImportSummary {
        certificate_digest_hex: hex::encode(digest),
    })
}

/// Verify the frozen creator binding and original StartThread capability.
#[wasm_bindgen(js_name = verifyNativeGenesisAuthority, unchecked_return_type = "NativeGenesisSummary")]
#[allow(clippy::too_many_arguments)]
pub fn verify_native_genesis_authority_binding(
    binding: &[u8],
    original: &[u8],
    envelope: &[u8],
    keyring: &[u8],
    current_owner: &[u8],
    selected_initial_owner_id: &[u8],
    selected_spool_genesis_digest: &[u8],
    revoked_key_ids_json: &str,
    revoked_credential_ids_json: &str,
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let ttl: i64 = checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    let digest = crate::native_genesis::verify_bytes(
        binding,
        original,
        envelope,
        keyring,
        current_owner,
        selected_initial_owner_id,
        selected_spool_genesis_digest,
        revoked_key_ids_json,
        revoked_credential_ids_json,
        now,
        ttl,
    )
    .map_err(verification_error)?;
    object(&digest)
}

/// Exact crate version backing this generated package.
#[wasm_bindgen(js_name = verifierVersion)]
#[must_use]
pub fn verifier_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Verify an observed protobuf owner root and return its typed accepted authority.
#[wasm_bindgen(js_name = verifyOwnerRoot, unchecked_return_type = "OwnerSummary")]
pub fn verify_owner_root_binding(signed_owner_root: &[u8]) -> Result<JsValue, JsValue> {
    object(
        &crate::observed::verify_owner_root_bytes(signed_owner_root).map_err(verification_error)?,
    )
}

/// Verify one owner-anchored purge using only caller-supplied public evidence.
///
/// Protobuf inputs must be their exact canonical encoded bytes. The returned
/// object is the typed `Decision`; malformed untrusted evidence fails
/// closed as `deny: malformed`. Invalid caller configuration is a JS error.
#[wasm_bindgen(js_name = verifyPurgeAuthorization, unchecked_return_type = "PurgeDecision")]
#[allow(clippy::too_many_arguments)]
pub fn verify_purge_authorization_binding(
    authorization: &[u8],
    operation_body: &[u8],
    payload: &[u8],
    owner_genesis: &[u8],
    current_owner_state_hash: &[u8],
    spool_uuid: &[u8],
    spool_path_segments: Vec<String>,
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<JsValue, JsValue> {
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    let limits = VerificationLimits::new(max_capability_ttl_seconds).map_err(verification_error)?;
    let Ok(body) =
        canonical_message::<PurgeOperationSigningBody>(operation_body, MAX_OPERATION_BODY_BYTES)
    else {
        return object(&Decision::Deny(Denial::Malformed));
    };
    let Ok(genesis) =
        canonical_message::<SignedSpoolOwnerGenesis>(owner_genesis, MAX_OWNER_GENESIS_BYTES)
    else {
        return object(&Decision::Deny(Denial::Malformed));
    };
    let Ok(current_state_hash) = fixed(current_owner_state_hash, "current owner state hash") else {
        return object(&Decision::Deny(Denial::Malformed));
    };
    let Ok(spool_uuid) = fixed(spool_uuid, "spool UUID") else {
        return object(&Decision::Deny(Denial::Malformed));
    };
    object(&verify_purge_authorization_bytes(
        authorization,
        &body,
        payload,
        &PurgeContext {
            owner_genesis: &genesis,
            current_owner_state_hash: &current_state_hash,
            spool_uuid: &spool_uuid,
            spool_path_segments: &spool_path_segments,
            now_unix_seconds,
            limits,
        },
    ))
}

/// Run the published purge fixture adapter and serialize its outcomes as JSON.
#[wasm_bindgen(js_name = runPurgeFixture)]
pub fn run_purge_fixture_binding(fixture_json: &str) -> Result<String, JsError> {
    json(&run_fixture(fixture_json).map_err(js_error)?)
}

/// Run the published ownership-transfer fixture adapter as JSON.
#[wasm_bindgen(js_name = runTransferFixture)]
pub fn run_transfer_fixture_binding(fixture_json: &str) -> Result<String, JsError> {
    json(&run_transfer_fixture(fixture_json).map_err(js_error)?)
}

/// Run the published clone-keyring fixture adapter as JSON.
#[wasm_bindgen(js_name = runKeyringFixture)]
pub fn run_keyring_fixture_binding(fixture_json: &str) -> Result<String, JsError> {
    json(&run_keyring_fixture(fixture_json).map_err(js_error)?)
}

/// Run the format-3 timeline owner-acceptance fixture adapter as JSON.
#[wasm_bindgen(js_name = runTimelineFixture)]
pub fn run_timeline_fixture_binding(fixture_json: &str) -> Result<String, JsError> {
    json(&run_timeline_fixture(fixture_json).map_err(js_error)?)
}

/// Verify one owner-derived format-3 timeline acceptance against current state.
/// All protobuf inputs must have canonical encodings. `false` denies malformed
/// or invalid evidence; an invalid verifier TTL is a caller configuration error.
#[wasm_bindgen(js_name = verifyTimelineAcceptance)]
#[allow(clippy::too_many_arguments)]
pub fn verify_timeline_acceptance_binding(
    origin_bytes: &[u8],
    acceptance_bytes: &[u8],
    accepted_state_hash: &[u8],
    spool_path_segments: Vec<String>,
    request_sha256: &[u8],
    #[wasm_bindgen(unchecked_param_type = "bigint")] first_position: JsValue,
    event_count: u32,
    revoked_capability_ids_hex: Vec<String>,
    revoked_subject_ids_hex: Vec<String>,
    #[wasm_bindgen(unchecked_param_type = "bigint")] now_unix_seconds: JsValue,
    #[wasm_bindgen(unchecked_param_type = "bigint")] max_capability_ttl_seconds: JsValue,
) -> Result<bool, JsValue> {
    let first_position: u64 = checked_integer(first_position, "first_position")?;
    let now_unix_seconds: i64 = checked_integer(now_unix_seconds, "now_unix_seconds")?;
    let max_capability_ttl_seconds: i64 =
        checked_integer(max_capability_ttl_seconds, "max_capability_ttl_seconds")?;
    let limits = VerificationLimits::new(max_capability_ttl_seconds).map_err(verification_error)?;
    let verified = (|| -> crate::Result<()> {
        let origin: TimelineOriginEndorsement = canonical_message(origin_bytes, 4096)?;
        let acceptance: TimelineAdmissionAcceptance = canonical_message(
            acceptance_bytes,
            heddle_api::timeline_upload::MAX_TIMELINE_REQUEST_BYTES,
        )?;
        let state_hash = fixed::<32>(accepted_state_hash, "accepted owner state hash")?;
        let request_sha256 = fixed::<32>(request_sha256, "request digest")?;
        let revoked_capability_ids = revoked_capability_ids_hex
            .iter()
            .map(|id| hex::decode(id).map_err(|error| crate::Error::Invalid(error.to_string())))
            .collect::<crate::Result<Vec<_>>>()?;
        let revoked_subject_ids = revoked_subject_ids_hex
            .iter()
            .map(|id| hex::decode(id).map_err(|error| crate::Error::Invalid(error.to_string())))
            .collect::<crate::Result<Vec<_>>>()?;
        verify_timeline_acceptance(
            &origin,
            &acceptance,
            &TimelineAcceptanceContext {
                accepted_state_hash: &state_hash,
                spool_path_segments: &spool_path_segments,
                request_sha256: &request_sha256,
                first_position,
                event_count,
                revoked_capability_ids: &revoked_capability_ids,
                revoked_subject_ids: &revoked_subject_ids,
                now_unix_seconds,
                limits,
            },
        )?;
        Ok(())
    })()
    .is_ok();
    Ok(verified)
}

// Shapes match the native serializable summaries; sequence strings are lossless.
#[wasm_bindgen(typescript_custom_section)]
const PRODUCTION_TYPES: &str = r#"
export interface OwnerSummary {
  owner_uuid_hex: string;
  owner_id_hex: string;
  state_hash_hex: string;
  sequence: string;
}
export interface TransferSummary {
  resource_uuid_hex: string;
  transfer_sequence: string;
  source_owner_uuid_hex: string;
  source_state_hash_hex: string;
  destination_owner_uuid_hex: string;
  destination_state_hash_hex: string;
}
export interface ResourceKeyringSummary {
  spool_uuid_hex: string;
  spool_genesis_digest_hex: string;
  current_owner: OwnerSummary;
  accepted_transfer_sequence: string;
  ownership_transfers: TransferSummary[];
  audit_record_hashes_hex: string[];
}
export interface GenesisSummary {
  spool_uuid_hex: string;
  spool_genesis_digest_hex: string;
  owner_key_id_hex: string;
  delegated_creation: boolean;
}
export interface PolicySummary {
  spool_uuid_hex: string;
  sequence: string;
  policy_state_hash_hex: string;
  owner_id_hex: string;
  owner_state_hash_hex: string;
  ownership_transfer_sequence: string;
  revoked_key_ids_hex: string[];
  max_audience: 1 | 2 | 3 | null;
}
export interface ImportSummary {
  certificate_digest_hex: string;
}
/** Account verifies StartThread authority. LocalKey verifies only its creator
 * binding and MUST acquire a separately verified hosting ownership claim. */
export type NativeGenesisSummary =
  | { certificate_digest_hex: string; owner_kind: "account"; requires_hosting_claim: false }
  | { certificate_digest_hex: string; owner_kind: "local_key"; requires_hosting_claim: true };
export type PurgeDecision = "purge" | { deny: "over-limit" | "malformed" | "invalid-proof" | "genesis-binding" | "stale-owner" | "capability" | "direct-only" | "operation-binding" | "time" };
export type VerificationErrorCode =
  "invalid" |
  "invalid_signature" |
  "broken_chain" |
  "recovery_threshold" |
  "capability_denied" |
  "expired" |
  "not_yet_valid" |
  "non_canonical_protobuf" |
  "too_large" |
  "decode" |
  "biscuit" |
  "policy_invalid" |
  "policy_broken_chain" |
  "policy_rollback" |
  "policy_not_owner_signed" |
  "policy_grow_only_dropped" |
  "policy_ancestor_ceiling" |
  "policy_backdated_transfer" |
  "policy_self_revocation" |
  "serialization" |
  "hybrid_version" |
  "hybrid_canonical" |
  "hybrid_bounds" |
  "hybrid_signature" |
  "hybrid_root" |
  "hybrid_semantic" |
  "hybrid_high_water" |
  "hybrid_transition" |
  "hybrid_job_as_witness" |
  "hybrid_key_role" |
  "hybrid_expired" |
  "hybrid_scope" |
  "hybrid_prepared_fields" |
  "hybrid_preparation_refused" |
  "hybrid_ref_disclosure" |
  "hybrid_ref_pinning" |
  "hybrid_source_selection" |
  "hybrid_import_source_requires_commit" |
  "hybrid_operation_id_reused" |
  "hybrid_pending_operation" |

  "hybrid_validity_bounds" |
  "hybrid_genesis_binding" |
  "hybrid_import_permission" |
  "hybrid_renewal_fork" |
  "hybrid_stale_manifest" |
  "hybrid_committed_slot" |
  "hybrid_stale_context" |
  "hybrid_proof" |
  "hybrid_revoked" |
  "hybrid_slot_conflict" |
  "hybrid_boundary_acceptance" |
  "hybrid_protocol";
export type PreparationRefusalReason = "unspecified" | "invalid_scope" | "unsupported_converter" | "unsupported_options" | "budget_exceeded" | "destination_conflict" | "policy_denied";
export interface VerificationError { code: VerificationErrorCode; message: string; preparation_refusal_reason?: PreparationRefusalReason; }
"#;
