// SPDX-License-Identifier: MIT OR Apache-2.0

use prost::Message;
use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::{
    Decision, Denial, PurgeContext, VerificationLimits,
    conformance::{run_fixture, run_keyring_fixture, run_transfer_fixture},
    verify_owner_root, verify_purge_authorization_bytes,
    wire::{PurgeOperationSigningBody, SignedOwnerRoot, SignedSpoolOwnerGenesis},
};

const MAX_OWNER_ROOT_BYTES: usize = 64 * 1024;
const MAX_OPERATION_BODY_BYTES: usize = 4 * 1024;
const MAX_OWNER_GENESIS_BYTES: usize = 4 * 1024;

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
