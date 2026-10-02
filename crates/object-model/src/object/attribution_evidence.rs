// SPDX-License-Identifier: Apache-2.0
//! State-bound, structured attribution claims. A state signature binds these
//! bytes; it does not attest that a provider actually executed a model.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ContentHash;

#[path = "attribution_evidence_codec.rs"]
mod codec;

/// Largest canonical evidence record, including its format discriminator.
pub const ATTRIBUTION_EVIDENCE_MAX_BYTES: usize = 256 * 1024;
pub const ATTRIBUTION_MAX_OPERATIONS: usize = 64;
pub const ATTRIBUTION_MAX_COLLECTION_METHODS: usize = 6;
pub const ATTRIBUTION_MAX_FILE_CHANGES: usize = 32;
pub const ATTRIBUTION_PATH_MAX_BYTES: usize = 1024;
/// Largest individual identity value or scoped identifier, in UTF-8 bytes.
pub const ATTRIBUTION_VALUE_MAX_BYTES: usize = 256;
/// Blob-body discriminator. These bytes use ordinary typed Blob hashing.
pub const ATTRIBUTION_EVIDENCE_MAGIC: &[u8; 4] = b"HAE1";

/// Why a value is claimed. This is provenance, never provider attestation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributionBasis {
    /// Local configuration, not evidence of a particular inference.
    Configured,
    /// A deliberate attribution override supplied by the caller.
    Explicit,
    /// The source reported what a specific request selected.
    RequestReported,
    /// The source reported a response value; not independently attested.
    ResponseReported,
    /// Direct harness/process observation without request/response assurance.
    Observed,
    /// Compatibility attribution whose original evidence is unavailable.
    Legacy,
}

/// Closed collection surfaces; no raw payload, source path, or credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributionSource {
    HarnessHook,
    StatusLine,
    Transcript,
    SessionMetadata,
    Request,
    Response,
    Configuration,
    Environment,
    ExplicitArgument,
    Process,
    Legacy,
}

/// One bounded value and the exact kind of evidence available for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionClaim {
    pub value: String,
    pub basis: AttributionBasis,
    pub source: AttributionSource,
    /// Source event identity scoped by this record's actor/session/request IDs.
    pub observation_id: Option<String>,
}

impl AttributionClaim {
    /// Construct a claim without manufacturing a source observation identifier.
    pub fn new(
        value: impl Into<String>,
        basis: AttributionBasis,
        source: AttributionSource,
    ) -> Self {
        Self {
            value: value.into(),
            basis,
            source,
            observation_id: None,
        }
    }
}

/// Model selection or response claims, independently present field by field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelAttribution {
    /// In `selected`, a configured/request routing key. In `response`, only
    /// an explicitly reported backend, never an inferred model publisher.
    pub provider: Option<AttributionClaim>,
    /// Exact selected alias or response-reported identifier; never expanded.
    pub model: Option<AttributionClaim>,
    /// Explicit model revision only. Never parsed out of a model identifier.
    pub version: Option<AttributionClaim>,
    pub thought_level: Option<AttributionClaim>,
}

/// Which executable observation a harness version describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessVersionScope {
    CurrentInvocation,
    SessionCreation,
    InstalledBinary,
}

/// Separate identity namespaces. None means the source did not supply it.
/// These are attribution labels, not authenticated principal/delegation IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionScope {
    pub heddle_session_id: Option<String>,
    pub heddle_segment_id: Option<String>,
    pub harness_instance_id: Option<String>,
    pub harness_session_id: Option<String>,
    pub actor_id: Option<String>,
    pub parent_actor_id: Option<String>,
    pub parent_harness_session_id: Option<String>,
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
    pub response_id: Option<String>,
    pub attempt_id: Option<String>,
    /// Harness message identity, distinct from a provider response identifier.
    pub message_id: Option<String>,
    pub root_turn_id: Option<String>,
    pub tool_call_id: Option<String>,
}

/// One operation's identity, without a recursive contributor collection.
/// These claims retain their own observation scope across mixed captures.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionOperationIdentity {
    pub harness: Option<AttributionClaim>,
    pub harness_version: Option<AttributionClaim>,
    pub harness_version_scope: Option<HarnessVersionScope>,
    pub selected: ModelAttribution,
    pub response: ModelAttribution,
    pub scope: AttributionScope,
    /// All collector origins for this causal fact, independent of each claim's
    /// provenance source. Conflicting facts remain separate operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collection_methods: Vec<AttributionCollectionMethod>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributionCollectionMethod {
    Hook,
    EventStream,
    OpenTelemetry,
    Transcript,
    Proxy,
    Explicit,
}

/// A normalized repository-relative path and ordinary typed Blob hashes.
/// Absence means the file did not exist at the corresponding boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionFileChange {
    pub path: String,
    #[schemars(with = "Option<[u8; 32]>")]
    pub before: Option<ContentHash>,
    #[schemars(with = "Option<[u8; 32]>")]
    pub after: Option<ContentHash>,
}

/// Whether a reported operation's transition is linked to captured content.
/// Neither outcome attests provider execution or ownership of individual lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributionOperationResolution {
    /// Observation is incomplete or ambiguous; no actual producer is claimed.
    Unresolved,
    /// The reported operation's before/after transition reached the captured
    /// state. This is content linkage, never model execution attestation.
    ContentBound,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionOperation {
    pub identity: AttributionOperationIdentity,
    pub changes: Vec<AttributionFileChange>,
    pub resolution: AttributionOperationResolution,
}

/// Canonical capture-time attribution. The containing State commits its Blob
/// hash. Do not put that StateId or a final capture operation ID here: either
/// would create a hash cycle. Later discoveries require a new state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttributionEvidenceV1 {
    pub format_version: u8,
    pub harness: Option<AttributionClaim>,
    pub harness_version: Option<AttributionClaim>,
    /// Required exactly when a harness version is present.
    pub harness_version_scope: Option<HarnessVersionScope>,
    /// Selection/request claims can remain present beside a different response.
    pub selected: ModelAttribution,
    /// Every populated response claim must have ResponseReported basis.
    pub response: ModelAttribution,
    pub scope: AttributionScope,
    /// Individually scoped contributors; never flattened into the capture's
    /// legacy Agent. Omission preserves the original HAE1 canonical bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<AttributionOperation>,
    /// Some activity was omitted or opaque; absence is not complete coverage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub operations_incomplete: bool,
}

impl Default for AttributionEvidenceV1 {
    fn default() -> Self {
        Self {
            format_version: 1,
            harness: None,
            harness_version: None,
            harness_version_scope: None,
            selected: ModelAttribution::default(),
            response: ModelAttribution::default(),
            scope: AttributionScope::default(),
            operations: Vec::new(),
            operations_incomplete: false,
        }
    }
}

/// Invalid or unbound attribution evidence. Errors never echo captured values.
#[derive(Debug, thiserror::Error)]
pub enum AttributionEvidenceError {
    #[error("unsupported attribution evidence format")]
    UnsupportedVersion,
    #[error("attribution evidence exceeds its size limit")]
    TooLarge,
    #[error("invalid attribution evidence field: {0}")]
    InvalidField(&'static str),
    #[error("attribution evidence has no harness, actor/session, or model identity")]
    Empty,
    #[error("invalid attribution evidence encoding")]
    Encoding,
    #[error("attribution evidence is not canonically encoded")]
    NonCanonical,
    #[error("attribution evidence blob hash mismatch")]
    HashMismatch,
    #[error("legacy agent contradicts state-bound attribution evidence")]
    LegacyContradiction,
}

#[cfg(test)]
#[path = "attribution_evidence_tests.rs"]
mod tests;
