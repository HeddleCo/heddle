// SPDX-License-Identifier: MIT OR Apache-2.0
//! Portable owner-authz v2 fixtures used by API and downstream repositories.

use prost::Message;
use serde::{Deserialize, Serialize};

use crate::{
    Decision, Error, PurgeContext, Result, TransferOwner, VerificationLimits,
    verify_clone_keyring_bytes, verify_owner_root, verify_purge_authorization_bytes,
    verify_resource_transfer,
    wire::{
        CloneAuthorizationKeyring, PurgeOperationSigningBody, ResourceOwnershipTransfer,
        SignedOwnerRoot, SignedSpoolOwnerGenesis,
    },
};

/// Maximum JSON fixture size accepted by the adapter.
pub const MAX_FIXTURE_BYTES: usize = 16 * 1024 * 1024;

/// Versioned cross-repository purge conformance fixture.
#[derive(Debug, Deserialize)]
pub struct ConformanceFixture {
    /// Exactly two for this adapter.
    pub format_version: u32,
    /// Caller-selected capability TTL ceiling.
    pub max_capability_ttl_seconds: i64,
    /// Cases evaluated independently.
    pub cases: Vec<ConformanceCase>,
}

/// One self-contained accept/deny purge case using canonical protobuf hex.
#[derive(Debug, Deserialize)]
pub struct ConformanceCase {
    /// Stable case identifier printed by every adapter.
    pub name: String,
    /// Exact expected decision, including denial category.
    pub expected: Decision,
    /// Canonical protobuf-encoded `SidecarAuthorization`.
    pub authorization_hex: String,
    /// Canonical protobuf-encoded `PurgeOperationSigningBody`.
    pub operation_body_hex: String,
    /// Raw purge payload.
    pub payload_hex: String,
    /// Canonical protobuf-encoded self-signed spool genesis.
    pub owner_genesis_hex: String,
    /// Current owner state hash.
    pub current_owner_state_hash_hex: String,
    /// Exact spool UUID.
    pub spool_uuid_hex: String,
    /// Exact canonical path used for selector matching.
    pub spool_path_segments: Vec<String>,
    /// Evaluation time supplied to the pure verifier.
    pub now_unix_seconds: i64,
}

/// Result emitted for one portable fixture case.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConformanceOutcome {
    /// Stable fixture case identifier.
    pub name: String,
    /// Fixture's expected decision.
    pub expected: Decision,
    /// Verifier's actual decision.
    pub actual: Decision,
    /// Whether actual and expected are identical.
    pub matches: bool,
}

/// Versioned ownership-transfer fixture.
#[derive(Debug, Deserialize)]
pub struct TransferConformanceFixture {
    /// Exactly two for this adapter.
    pub format_version: u32,
    /// Complete and incomplete transfer cases.
    pub cases: Vec<TransferConformanceCase>,
}

/// One self-contained ownership-transfer case.
#[derive(Debug, Deserialize)]
pub struct TransferConformanceCase {
    /// Stable case identifier.
    pub name: String,
    /// Whether transfer verification must accept.
    pub expected_accept: bool,
    /// Canonical protobuf-encoded transfer.
    pub transfer_hex: String,
    /// Canonical protobuf-encoded source owner root.
    pub source_owner_root_hex: String,
    /// Canonical protobuf-encoded destination owner root.
    pub destination_owner_root_hex: String,
    /// Stable source owner UUID.
    pub source_owner_uuid_hex: String,
    /// Stable destination owner UUID.
    pub destination_owner_uuid_hex: String,
    /// Stable resource UUID.
    pub resource_uuid_hex: String,
    /// Expected gap-free transfer sequence.
    pub transfer_sequence: u64,
}

/// Versioned self-rooted clone-keyring fixture.
#[derive(Debug, Deserialize)]
pub struct KeyringConformanceFixture {
    /// Exactly two for this adapter.
    pub format_version: u32,
    /// Caller-selected capability TTL ceiling.
    pub max_capability_ttl_seconds: i64,
    /// Well-formed and malformed keyring cases.
    pub cases: Vec<KeyringConformanceCase>,
}

/// One clone-keyring verification case without an ownership transfer.
#[derive(Debug, Deserialize)]
pub struct KeyringConformanceCase {
    /// Stable case identifier.
    pub name: String,
    /// Whether keyring verification must accept.
    pub expected_accept: bool,
    /// Canonical protobuf-encoded clone keyring.
    pub keyring_hex: String,
    /// Evaluation time supplied to the pure verifier.
    pub now_unix_seconds: i64,
}

/// Boolean guard result emitted by transfer and keyring adapters.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardOutcome {
    /// Stable fixture case identifier.
    pub name: String,
    /// Fixture's expected result.
    pub expected_accept: bool,
    /// Verifier's actual result.
    pub accepted: bool,
    /// Whether actual and expected are identical.
    pub matches: bool,
}

fn hex_bytes(value: &str, maximum_bytes: usize) -> Result<Vec<u8>> {
    if value.len() > maximum_bytes.saturating_mul(2) {
        return Err(Error::TooLarge {
            limit: maximum_bytes,
        });
    }
    hex::decode(value).map_err(|error| Error::Invalid(format!("fixture hex: {error}")))
}

fn fixed<const N: usize>(value: &str, label: &str) -> Result<[u8; N]> {
    hex_bytes(value, N)?
        .try_into()
        .map_err(|_| Error::Invalid(format!("{label} must be {N} bytes")))
}

fn canonical_message<T>(value: &str, maximum_bytes: usize) -> Result<T>
where
    T: Message + Default,
{
    let bytes = hex_bytes(value, maximum_bytes)?;
    let decoded = T::decode(bytes.as_slice())?;
    if decoded.encode_to_vec() != bytes {
        return Err(Error::NonCanonicalProtobuf);
    }
    Ok(decoded)
}

/// Run every purge case without filesystem, clock, registry, or network access.
pub fn run_fixture(json: &str) -> Result<Vec<ConformanceOutcome>> {
    if json.len() > MAX_FIXTURE_BYTES {
        return Err(Error::TooLarge {
            limit: MAX_FIXTURE_BYTES,
        });
    }
    let fixture: ConformanceFixture = serde_json::from_str(json)
        .map_err(|error| Error::Invalid(format!("fixture JSON: {error}")))?;
    if fixture.format_version != 2 || fixture.cases.is_empty() {
        return Err(Error::Invalid(
            "conformance fixture version or case set is invalid".to_owned(),
        ));
    }
    let limits = VerificationLimits::new(fixture.max_capability_ttl_seconds)?;
    fixture
        .cases
        .into_iter()
        .map(|case| {
            let authorization = hex_bytes(
                &case.authorization_hex,
                limits.max_bundle_bytes().saturating_add(256),
            )?;
            let body: PurgeOperationSigningBody =
                canonical_message(&case.operation_body_hex, 4096)?;
            let payload = hex_bytes(&case.payload_hex, limits.max_payload_bytes())?;
            let owner_genesis: SignedSpoolOwnerGenesis =
                canonical_message(&case.owner_genesis_hex, 4096)?;
            let current_state_hash = fixed(
                &case.current_owner_state_hash_hex,
                "current owner state hash",
            )?;
            let spool_uuid = fixed(&case.spool_uuid_hex, "spool UUID")?;
            let actual = verify_purge_authorization_bytes(
                &authorization,
                &body,
                &payload,
                &PurgeContext {
                    owner_genesis: &owner_genesis,
                    current_owner_state_hash: &current_state_hash,
                    spool_uuid: &spool_uuid,
                    spool_path_segments: &case.spool_path_segments,
                    now_unix_seconds: case.now_unix_seconds,
                    limits,
                },
            );
            Ok(ConformanceOutcome {
                name: case.name,
                expected: case.expected,
                actual,
                matches: actual == case.expected,
            })
        })
        .collect()
}

/// Run every complete/incomplete ownership-transfer fixture case.
pub fn run_transfer_fixture(json: &str) -> Result<Vec<GuardOutcome>> {
    if json.len() > MAX_FIXTURE_BYTES {
        return Err(Error::TooLarge {
            limit: MAX_FIXTURE_BYTES,
        });
    }
    let fixture: TransferConformanceFixture = serde_json::from_str(json)
        .map_err(|error| Error::Invalid(format!("transfer fixture JSON: {error}")))?;
    if fixture.format_version != 2 || fixture.cases.is_empty() {
        return Err(Error::Invalid(
            "transfer fixture version or case set is invalid".to_owned(),
        ));
    }
    fixture
        .cases
        .into_iter()
        .map(|case| {
            let transfer: ResourceOwnershipTransfer =
                canonical_message(&case.transfer_hex, VerificationLimits::MAX_BUNDLE_BYTES)?;
            let source_root: SignedOwnerRoot =
                canonical_message(&case.source_owner_root_hex, 64 * 1024)?;
            let destination_root: SignedOwnerRoot =
                canonical_message(&case.destination_owner_root_hex, 64 * 1024)?;
            let source_state = verify_owner_root(&source_root)?;
            let destination_state = verify_owner_root(&destination_root)?;
            let source_uuid = fixed(&case.source_owner_uuid_hex, "source owner UUID")?;
            let destination_uuid =
                fixed(&case.destination_owner_uuid_hex, "destination owner UUID")?;
            let resource_uuid = fixed(&case.resource_uuid_hex, "resource UUID")?;
            let accepted = verify_resource_transfer(
                &transfer,
                &resource_uuid,
                case.transfer_sequence,
                TransferOwner {
                    stable_owner_uuid: &source_uuid,
                    state: &source_state,
                },
                TransferOwner {
                    stable_owner_uuid: &destination_uuid,
                    state: &destination_state,
                },
            )
            .is_ok();
            Ok(GuardOutcome {
                name: case.name,
                expected_accept: case.expected_accept,
                accepted,
                matches: accepted == case.expected_accept,
            })
        })
        .collect()
}

/// Run every well-formed/malformed self-rooted clone-keyring fixture case.
pub fn run_keyring_fixture(json: &str) -> Result<Vec<GuardOutcome>> {
    if json.len() > MAX_FIXTURE_BYTES {
        return Err(Error::TooLarge {
            limit: MAX_FIXTURE_BYTES,
        });
    }
    let fixture: KeyringConformanceFixture = serde_json::from_str(json)
        .map_err(|error| Error::Invalid(format!("keyring fixture JSON: {error}")))?;
    if fixture.format_version != 2 || fixture.cases.is_empty() {
        return Err(Error::Invalid(
            "keyring fixture version or case set is invalid".to_owned(),
        ));
    }
    let limits = VerificationLimits::new(fixture.max_capability_ttl_seconds)?;
    fixture
        .cases
        .into_iter()
        .map(|case| {
            let bytes = hex_bytes(&case.keyring_hex, limits.max_bundle_bytes())?;
            let keyring = CloneAuthorizationKeyring::decode(bytes.as_slice())?;
            if keyring.encode_to_vec() != bytes {
                return Err(Error::NonCanonicalProtobuf);
            }
            let accepted =
                verify_clone_keyring_bytes(&bytes, case.now_unix_seconds, limits, &[]).is_ok();
            Ok(GuardOutcome {
                name: case.name,
                expected_accept: case.expected_accept,
                accepted,
                matches: accepted == case.expected_accept,
            })
        })
        .collect()
}

/// Owner-authz v2 purge accept/deny fixture matrix embedded in the crate.
pub const FIXTURE_V2_JSON: &str = include_str!("../conformance/fixtures/v2.json");
/// Ownership-transfer v2 fixture matrix embedded in the crate.
pub const TRANSFER_FIXTURE_V2_JSON: &str = include_str!("../conformance/fixtures/transfer-v2.json");
/// Self-rooted clone-keyring v2 fixture matrix embedded in the crate.
pub const KEYRING_FIXTURE_V2_JSON: &str = include_str!("../conformance/fixtures/keyring-v2.json");
