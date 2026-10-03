// SPDX-License-Identifier: MIT OR Apache-2.0

use prost::Message;
use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::{
    Decision, Denial, PurgeContext, TimelineAcceptanceContext, VerificationLimits,
    conformance::{run_fixture, run_keyring_fixture, run_timeline_fixture, run_transfer_fixture},
    verify_owner_root, verify_purge_authorization_bytes, verify_timeline_acceptance,
    wire::{
        PurgeOperationSigningBody, SignedOwnerRoot, SignedSpoolOwnerGenesis,
        TimelineAdmissionAcceptance, TimelineOriginEndorsement,
    },
};

const MAX_OWNER_ROOT_BYTES: usize = 64 * 1024;
const MAX_OPERATION_BODY_BYTES: usize = 4 * 1024;
const MAX_OWNER_GENESIS_BYTES: usize = crate::creation::MAX_CREATION_PROOF_BYTES + 1024;

#[derive(Serialize)]
struct OwnerStateSummary {
    owner_id_hex: String,
    state_hash_hex: String,
    sequence: u64,
}

fn js_error(error: impl ToString) -> JsError {
    JsError::new(&error.to_string())
}

fn json<T: Serialize>(value: &T) -> Result<String, JsError> {
    serde_json::to_string(value).map_err(js_error)
}

fn canonical_message<T>(bytes: &[u8], maximum_bytes: usize) -> crate::Result<T>
where
    T: Message + Default,
{
    if bytes.len() > maximum_bytes {
        return Err(crate::Error::TooLarge {
            limit: maximum_bytes,
        });
    }
    let decoded = T::decode(bytes)?;
    if decoded.encode_to_vec() != bytes {
        return Err(crate::Error::NonCanonicalProtobuf);
    }
    Ok(decoded)
}

fn fixed<const N: usize>(bytes: &[u8], label: &str) -> crate::Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| crate::Error::Invalid(format!("{label} must be {N} bytes")))
}

/// Verify typed import delegation against independently selected owner/Spool
/// pins and actual current time. No incoming keyring enrolls its carried root.
/// Key-role exclusions and job associations use bounded canonical JSON arrays
/// of hex keys and [key, logical-job] pairs respectively.
#[wasm_bindgen(js_name = verifyImportDelegation)]
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
    now_unix_seconds: i64,
    max_capability_ttl_seconds: i64,
) -> Result<Vec<u8>, JsError> {
    use crate::import_delegation::{CurrentContext, Revocation, Selection};
    fn ids(value: &str, width: usize) -> crate::Result<Vec<Vec<u8>>> {
        if value.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
            return Err(crate::Error::TooLarge {
                limit: heddle_api::import_authority::MAX_BUNDLE_BYTES,
            });
        }
        let values: Vec<String> =
            serde_json::from_str(value).map_err(|e| crate::Error::Invalid(e.to_string()))?;
        if values.len() > 4096 {
            return Err(crate::Error::TooLarge { limit: 4096 });
        }
        values
            .iter()
            .map(|v| {
                let bytes = hex::decode(v).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                if bytes.len() != width {
                    return Err(crate::Error::Invalid("wrong identifier width".into()));
                }
                Ok(bytes)
            })
            .collect()
    }
    let result = (|| -> crate::Result<Vec<u8>> {
        let limits = VerificationLimits::new(max_capability_ttl_seconds)?;
        let keyring = crate::verify_clone_keyring_bytes(keyring, now_unix_seconds, limits, &[])?;
        let history: crate::wire::OwnerHistory =
            canonical_message(accepted_owner_history, limits.max_bundle_bytes())?;
        let mut owner = crate::verify_owner_root(
            history
                .root
                .as_ref()
                .ok_or_else(|| crate::Error::Invalid("owner root missing".into()))?,
        )?;
        if history.accepted_transitions.len() > 64 {
            return Err(crate::Error::TooLarge { limit: 64 });
        }
        for transition in &history.accepted_transitions {
            owner = crate::apply_accepted_transition(&owner, transition, now_unix_seconds, limits)?;
        }
        if history.state_hash != owner.state_hash() {
            return Err(crate::Error::Hybrid(heddle_api::hybrid_codec::Reject::Root));
        }
        let certificate =
            canonical_message(certificate, heddle_api::import_authority::MAX_RECORD_BYTES)?;
        let permission = if permission.is_empty() {
            None
        } else {
            Some(canonical_message(
                permission,
                heddle_api::import_authority::MAX_RECORD_BYTES,
            )?)
        };
        let initial = fixed(selected_initial_owner_id, "initial owner id")?;
        let digest = fixed(selected_spool_genesis_digest, "Spool genesis digest")?;
        let forbidden = ids(forbidden_job_keys_json, 32)?;
        let cancelled = ids(cancelled_ids_json, 32)?;
        let revoked = ids(revoked_key_ids_json, 32)?;
        if known_job_associations_json.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
            return Err(crate::Error::TooLarge {
                limit: heddle_api::import_authority::MAX_BUNDLE_BYTES,
            });
        }
        let pairs: Vec<[String; 2]> = serde_json::from_str(known_job_associations_json)
            .map_err(|e| crate::Error::Invalid(e.to_string()))?;
        if pairs.len() > 4096 {
            return Err(crate::Error::TooLarge { limit: 4096 });
        }
        let associations = pairs
            .iter()
            .map(|p| {
                let key = hex::decode(&p[0]).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                let job = hex::decode(&p[1]).map_err(|e| crate::Error::Invalid(e.to_string()))?;
                fixed::<32>(&key, "job key")?;
                fixed::<16>(&job, "logical job")?;
                Ok((key, job))
            })
            .collect::<crate::Result<Vec<_>>>()?;
        let context = CurrentContext {
            selection: Selection {
                spool_genesis_digest: &digest,
                initial_owner_id: &initial,
                owner: &owner,
                keyring: &keyring,
                limits,
            },
            now: now_unix_seconds,
            forbidden_job_keys: &forbidden,
            known_job_associations: &associations,
        };
        let verified = crate::import_delegation::verify_current(
            &certificate,
            permission.as_ref(),
            &context,
            |r| match r {
                Revocation::Cancellation(_, id) => cancelled.iter().any(|c| c == id),
                Revocation::Key(id) => revoked.iter().any(|k| k == id),
            },
        )?;
        Ok(verified.scope().digest().to_vec())
    })();
    result.map_err(js_error)
}

/// Exact crate version backing this generated package.
#[wasm_bindgen(js_name = verifierVersion)]
#[must_use]
pub fn verifier_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Verify a canonical protobuf owner root and return its stable ids as JSON.
#[wasm_bindgen(js_name = verifyOwnerRoot)]
pub fn verify_owner_root_binding(signed_owner_root: &[u8]) -> Result<String, JsError> {
    let signed: SignedOwnerRoot =
        canonical_message(signed_owner_root, MAX_OWNER_ROOT_BYTES).map_err(js_error)?;
    let state = verify_owner_root(&signed).map_err(js_error)?;
    json(&OwnerStateSummary {
        owner_id_hex: hex::encode(state.owner_id()),
        state_hash_hex: hex::encode(state.state_hash()),
        sequence: state.sequence(),
    })
}

/// Verify one owner-anchored purge using only caller-supplied public evidence.
///
/// Protobuf inputs must be their exact canonical encoded bytes. The returned
/// JSON is the serialized `Decision`; malformed untrusted evidence fails
/// closed as `deny: malformed`. Invalid caller configuration is a JS error.
#[wasm_bindgen(js_name = verifyPurgeAuthorization)]
#[allow(clippy::too_many_arguments)]
pub fn verify_purge_authorization_binding(
    authorization: &[u8],
    operation_body: &[u8],
    payload: &[u8],
    owner_genesis: &[u8],
    current_owner_state_hash: &[u8],
    spool_uuid: &[u8],
    spool_path_segments: Vec<String>,
    now_unix_seconds: i64,
    max_capability_ttl_seconds: i64,
) -> Result<String, JsError> {
    let limits = VerificationLimits::new(max_capability_ttl_seconds).map_err(js_error)?;
    let Ok(body) =
        canonical_message::<PurgeOperationSigningBody>(operation_body, MAX_OPERATION_BODY_BYTES)
    else {
        return json(&Decision::Deny(Denial::Malformed));
    };
    let Ok(genesis) =
        canonical_message::<SignedSpoolOwnerGenesis>(owner_genesis, MAX_OWNER_GENESIS_BYTES)
    else {
        return json(&Decision::Deny(Denial::Malformed));
    };
    let Ok(current_state_hash) = fixed(current_owner_state_hash, "current owner state hash") else {
        return json(&Decision::Deny(Denial::Malformed));
    };
    let Ok(spool_uuid) = fixed(spool_uuid, "spool UUID") else {
        return json(&Decision::Deny(Denial::Malformed));
    };
    json(&verify_purge_authorization_bytes(
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
    first_position: u64,
    event_count: u32,
    revoked_capability_ids_hex: Vec<String>,
    revoked_subject_ids_hex: Vec<String>,
    now_unix_seconds: i64,
    max_capability_ttl_seconds: i64,
) -> Result<bool, JsError> {
    let limits = VerificationLimits::new(max_capability_ttl_seconds).map_err(js_error)?;
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
