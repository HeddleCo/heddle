// SPDX-License-Identifier: Apache-2.0
//! Agent timeline operation objects.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::object::{ContentHash, StateId};

/// Current timeline operation schema version.
pub const TIMELINE_OPERATION_SCHEMA_VERSION: u16 = LatestTimelineOperationSchema::VERSION;

mod sealed {
    pub trait Sealed {}
}

trait VersionedTimelineOperationSchema: sealed::Sealed {
    const VERSION: u16;
    const NAME: &'static str;

    fn encode(envelope: &TimelineOperationEnvelope) -> Result<Vec<u8>, TimelineCodecError>;
    fn decode(bytes: &[u8]) -> Result<TimelineOperationEnvelope, TimelineCodecError>;
}

struct TimelineOperationV1Schema;
type LatestTimelineOperationSchema = TimelineOperationV1Schema;

impl sealed::Sealed for TimelineOperationV1Schema {}

impl VersionedTimelineOperationSchema for TimelineOperationV1Schema {
    const VERSION: u16 = 1;
    const NAME: &'static str = "timeline-operation-v1";

    fn encode(envelope: &TimelineOperationEnvelope) -> Result<Vec<u8>, TimelineCodecError> {
        if envelope.schema_version != Self::VERSION {
            return Err(TimelineCodecError::UnsupportedVersion(
                envelope.schema_version,
            ));
        }
        if envelope.kind != envelope.body.kind() {
            return Err(TimelineCodecError::KindBodyMismatch {
                kind: envelope.kind,
                body: envelope.body.kind(),
            });
        }
        let wire = TimelineOperationEnvelopeWireV1 {
            schema_version: Self::VERSION,
            kind: envelope.kind.as_str().to_string(),
            labels: canonical_timeline_labels(&envelope.labels),
            body: envelope.body.encode_body()?,
        };
        rmp_serde::to_vec_named(&wire).map_err(|err| TimelineCodecError::Encoding(err.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<TimelineOperationEnvelope, TimelineCodecError> {
        let wire: TimelineOperationEnvelopeWireV1 =
            rmp_serde::from_slice(bytes).map_err(|err| {
                TimelineCodecError::Decoding(format!("decode {} envelope: {err}", Self::NAME))
            })?;
        if wire.schema_version != Self::VERSION {
            return Err(TimelineCodecError::UnsupportedVersion(wire.schema_version));
        }
        let kind = TimelineOperationKind::try_from(wire.kind.as_str())?;
        let body = TimelineOperationBodyV1::decode_body(kind, &wire.body)?;
        Ok(TimelineOperationEnvelope {
            schema_version: wire.schema_version,
            kind,
            labels: canonical_timeline_labels(&wire.labels),
            body,
        })
    }
}

/// Content-addressed identifier for a timeline operation envelope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimelineOperationId([u8; 32]);

impl TimelineOperationId {
    /// Compute an operation id from canonical timeline operation envelope bytes.
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("timeline-operation", bytes);
        Self(*hash.as_bytes())
    }

    /// Create an id from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Decode from a 32-byte slice.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, TimelineOperationIdParseError> {
        if bytes.len() != 32 {
            return Err(TimelineOperationIdParseError::InvalidLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(Self(arr))
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hexadecimal for filesystem storage.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Full display form.
    pub fn to_string_full(&self) -> String {
        format!(
            "tl-{}",
            base32::encode(base32::Alphabet::Crockford, &self.0).to_lowercase()
        )
    }

    /// Short display form.
    pub fn short(&self) -> String {
        let full = self.to_string_full();
        full[..18.min(full.len())].to_string()
    }
}

impl fmt::Debug for TimelineOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TimelineOperationId({})", self.short())
    }
}

impl fmt::Display for TimelineOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

/// Error parsing a timeline operation id.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TimelineOperationIdParseError {
    #[error("invalid length (expected 32 bytes)")]
    InvalidLength,
}

macro_rules! timeline_string_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Generate a new opaque id.
            pub fn generate() -> Self {
                let bytes: [u8; 10] = rand::random();
                Self(format!(
                    "{}{}",
                    $prefix,
                    base32::encode(base32::Alphabet::Crockford, &bytes).to_lowercase()
                ))
            }

            /// Create an id from an existing string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the id string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

timeline_string_id!(TimelineStepId, "tls-");
timeline_string_id!(TimelineBranchId, "tlb-");

/// Explicit timeline operation kind stored in every operation envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimelineOperationKind {
    ToolCallStarted,
    ToolCallFinished,
    CursorMoved,
    BranchCreated,
}

impl TimelineOperationKind {
    /// Stable wire string for this operation kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolCallStarted => "tool_call_started",
            Self::ToolCallFinished => "tool_call_finished",
            Self::CursorMoved => "cursor_moved",
            Self::BranchCreated => "branch_created",
        }
    }
}

impl TryFrom<&str> for TimelineOperationKind {
    type Error = TimelineCodecError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "tool_call_started" => Ok(Self::ToolCallStarted),
            "tool_call_finished" => Ok(Self::ToolCallFinished),
            "cursor_moved" => Ok(Self::CursorMoved),
            "branch_created" => Ok(Self::BranchCreated),
            other => Err(TimelineCodecError::UnknownKind(other.to_string())),
        }
    }
}

impl Serialize for TimelineOperationKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TimelineOperationKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value.as_str()).map_err(de::Error::custom)
    }
}

/// Safety labels attached to timeline operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineLabel {
    RepoReversible,
    ExternalSideEffectsUnknown,
    IgnoredPathTouched,
    OutsideRepoTouched,
    PurgeBoundary,
    CaptureFailed,
}

/// Scrubbed metadata for native tool payloads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolPayloadMetadata {
    pub summary: Option<String>,
    pub hash: Option<ContentHash>,
}

/// Native harness tool-call identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeToolCallRefV1 {
    pub harness: String,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_call_id: String,
}

/// Tool-call terminal status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineToolCallStatus {
    Succeeded,
    Failed,
    Cancelled,
}

/// Why the timeline cursor moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineCursorMoveReason {
    SeekToolCall,
    Undo,
    Redo,
    Reset,
    AutoAdvance,
}

/// Why a timeline branch was created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineBranchReason {
    EditFromRewoundCursor,
    ExplicitFork,
    Retry,
    FanOut,
}

/// A v1 timeline operation envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineOperationEnvelope {
    pub schema_version: u16,
    pub kind: TimelineOperationKind,
    pub labels: Vec<TimelineLabel>,
    pub body: TimelineOperationBodyV1,
}

impl TimelineOperationEnvelope {
    /// Build a v1 envelope for a body.
    pub fn new(body: TimelineOperationBodyV1, labels: Vec<TimelineLabel>) -> Self {
        Self {
            schema_version: TIMELINE_OPERATION_SCHEMA_VERSION,
            kind: body.kind(),
            labels,
            body,
        }
    }

    /// Encode the envelope as canonical msgpack bytes.
    pub fn encode(&self) -> Result<Vec<u8>, TimelineCodecError> {
        LatestTimelineOperationSchema::encode(self)
    }

    /// Decode canonical msgpack bytes into an envelope.
    pub fn decode(bytes: &[u8]) -> Result<Self, TimelineCodecError> {
        match timeline_operation_schema_version(bytes)? {
            TimelineOperationV1Schema::VERSION => TimelineOperationV1Schema::decode(bytes),
            other => Err(TimelineCodecError::UnsupportedVersion(other)),
        }
    }

    /// Compute this envelope's content-addressed operation id.
    pub fn operation_id(&self) -> Result<TimelineOperationId, TimelineCodecError> {
        Ok(TimelineOperationId::for_bytes(&self.encode()?))
    }
}

#[derive(Serialize, Deserialize)]
struct TimelineOperationEnvelopeVersionProbe {
    schema_version: u16,
}

#[derive(Serialize, Deserialize)]
struct TimelineOperationEnvelopeWireV1 {
    schema_version: u16,
    kind: String,
    labels: Vec<TimelineLabel>,
    body: Vec<u8>,
}

/// V1 timeline operation body variants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineOperationBodyV1 {
    ToolCallStarted(ToolCallStartedV1),
    ToolCallFinished(ToolCallFinishedV1),
    CursorMoved(CursorMovedV1),
    BranchCreated(BranchCreatedV1),
}

impl TimelineOperationBodyV1 {
    fn kind(&self) -> TimelineOperationKind {
        match self {
            Self::ToolCallStarted(_) => TimelineOperationKind::ToolCallStarted,
            Self::ToolCallFinished(_) => TimelineOperationKind::ToolCallFinished,
            Self::CursorMoved(_) => TimelineOperationKind::CursorMoved,
            Self::BranchCreated(_) => TimelineOperationKind::BranchCreated,
        }
    }

    fn encode_body(&self) -> Result<Vec<u8>, TimelineCodecError> {
        match self {
            Self::ToolCallStarted(body) => encode_body(body),
            Self::ToolCallFinished(body) => encode_body(body),
            Self::CursorMoved(body) => encode_body(body),
            Self::BranchCreated(body) => encode_body(body),
        }
    }

    fn decode_body(kind: TimelineOperationKind, bytes: &[u8]) -> Result<Self, TimelineCodecError> {
        match kind {
            TimelineOperationKind::ToolCallStarted => decode_body(bytes).map(Self::ToolCallStarted),
            TimelineOperationKind::ToolCallFinished => {
                decode_body(bytes).map(Self::ToolCallFinished)
            }
            TimelineOperationKind::CursorMoved => decode_body(bytes).map(Self::CursorMoved),
            TimelineOperationKind::BranchCreated => decode_body(bytes).map(Self::BranchCreated),
        }
    }
}

/// Tool-call start operation body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStartedV1 {
    pub thread: String,
    pub step_id: TimelineStepId,
    pub branch_id: TimelineBranchId,
    pub parent_step_id: Option<TimelineStepId>,
    pub native: NativeToolCallRefV1,
    pub tool_name: String,
    pub before_state: StateId,
    pub payload: Option<TimelineToolPayloadMetadata>,
    pub started_at_ms: i64,
}

/// Tool-call finish operation body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFinishedV1 {
    pub thread: String,
    pub step_id: TimelineStepId,
    pub branch_id: TimelineBranchId,
    pub native: NativeToolCallRefV1,
    pub status: TimelineToolCallStatus,
    pub before_state: StateId,
    pub after_state: StateId,
    pub capture_state: Option<StateId>,
    pub capture_oplog_batch_id: Option<u64>,
    pub changed: bool,
    pub touched_paths: Vec<String>,
    pub payload: Option<TimelineToolPayloadMetadata>,
    pub finished_at_ms: i64,
}

/// Cursor movement operation body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorMovedV1 {
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub from_step_id: Option<TimelineStepId>,
    pub to_step_id: Option<TimelineStepId>,
    pub from_state: StateId,
    pub to_state: StateId,
    pub reason: TimelineCursorMoveReason,
    pub moved_at_ms: i64,
}

/// Branch creation operation body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCreatedV1 {
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub parent_branch_id: Option<TimelineBranchId>,
    pub from_step_id: Option<TimelineStepId>,
    pub from_state: StateId,
    pub reason: TimelineBranchReason,
    pub created_at_ms: i64,
}

/// Timeline operation codec error.
#[derive(Debug, thiserror::Error)]
pub enum TimelineCodecError {
    #[error("unsupported timeline operation schema version {0}")]
    UnsupportedVersion(u16),
    #[error("unknown timeline operation kind {0}")]
    UnknownKind(String),
    #[error("timeline operation kind {kind:?} does not match body kind {body:?}")]
    KindBodyMismatch {
        kind: TimelineOperationKind,
        body: TimelineOperationKind,
    },
    #[error("timeline operation encoding error: {0}")]
    Encoding(String),
    #[error("timeline operation decoding error: {0}")]
    Decoding(String),
}

fn encode_body<T: Serialize>(body: &T) -> Result<Vec<u8>, TimelineCodecError> {
    rmp_serde::to_vec_named(body).map_err(|err| TimelineCodecError::Encoding(err.to_string()))
}

fn decode_body<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, TimelineCodecError> {
    rmp_serde::from_slice(bytes).map_err(|err| TimelineCodecError::Decoding(err.to_string()))
}

fn timeline_operation_schema_version(bytes: &[u8]) -> Result<u16, TimelineCodecError> {
    let probe: TimelineOperationEnvelopeVersionProbe = rmp_serde::from_slice(bytes)
        .map_err(|err| TimelineCodecError::Decoding(format!("decode timeline version: {err}")))?;
    Ok(probe.schema_version)
}

fn canonical_timeline_labels(labels: &[TimelineLabel]) -> Vec<TimelineLabel> {
    let mut labels = labels.to_vec();
    labels.sort_by_key(timeline_label_order);
    labels.dedup();
    labels
}

fn timeline_label_order(label: &TimelineLabel) -> u8 {
    match label {
        TimelineLabel::RepoReversible => 0,
        TimelineLabel::ExternalSideEffectsUnknown => 1,
        TimelineLabel::IgnoredPathTouched => 2,
        TimelineLabel::OutsideRepoTouched => 3,
        TimelineLabel::PurgeBoundary => 4,
        TimelineLabel::CaptureFailed => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body() -> TimelineOperationBodyV1 {
        TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
            thread: "main".to_string(),
            step_id: TimelineStepId::new("tls-step"),
            branch_id: TimelineBranchId::new("tlb-main"),
            parent_step_id: None,
            native: NativeToolCallRefV1 {
                harness: "opencode".to_string(),
                session_id: Some("session-1".to_string()),
                message_id: Some("message-1".to_string()),
                tool_call_id: "call-1".to_string(),
            },
            tool_name: "shell".to_string(),
            before_state: StateId::from_bytes([1; 32]),
            payload: Some(TimelineToolPayloadMetadata {
                summary: Some("listed files".to_string()),
                hash: Some(ContentHash::compute_typed(
                    "timeline-tool-payload",
                    b"scrubbed",
                )),
            }),
            started_at_ms: 1_700_000_000_000,
        })
    }

    fn sample_envelope() -> TimelineOperationEnvelope {
        TimelineOperationEnvelope::new(
            sample_body(),
            vec![
                TimelineLabel::RepoReversible,
                TimelineLabel::IgnoredPathTouched,
            ],
        )
    }

    fn sample_native(tool_call_id: &str) -> NativeToolCallRefV1 {
        NativeToolCallRefV1 {
            harness: "opencode".to_string(),
            session_id: Some("session-1".to_string()),
            message_id: Some("message-1".to_string()),
            tool_call_id: tool_call_id.to_string(),
        }
    }

    fn sample_payload(summary: &str) -> TimelineToolPayloadMetadata {
        TimelineToolPayloadMetadata {
            summary: Some(summary.to_string()),
            hash: Some(ContentHash::compute_typed(
                "timeline-tool-payload",
                summary.as_bytes(),
            )),
        }
    }

    fn golden_envelopes() -> Vec<(&'static str, TimelineOperationEnvelope)> {
        vec![
            (
                "tool_call_started",
                TimelineOperationEnvelope::new(
                    TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
                        thread: "main".to_string(),
                        step_id: TimelineStepId::new("tls-step"),
                        branch_id: TimelineBranchId::new("tlb-main"),
                        parent_step_id: None,
                        native: sample_native("call-1"),
                        tool_name: "bash".to_string(),
                        before_state: StateId::from_bytes([1; 32]),
                        payload: Some(sample_payload("started")),
                        started_at_ms: 1_700_000_000_001,
                    }),
                    vec![
                        TimelineLabel::IgnoredPathTouched,
                        TimelineLabel::RepoReversible,
                        TimelineLabel::IgnoredPathTouched,
                    ],
                ),
            ),
            (
                "tool_call_finished",
                TimelineOperationEnvelope::new(
                    TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                        thread: "main".to_string(),
                        step_id: TimelineStepId::new("tls-step"),
                        branch_id: TimelineBranchId::new("tlb-main"),
                        native: sample_native("call-1"),
                        status: TimelineToolCallStatus::Succeeded,
                        before_state: StateId::from_bytes([1; 32]),
                        after_state: StateId::from_bytes([2; 32]),
                        capture_state: Some(StateId::from_bytes([2; 32])),
                        capture_oplog_batch_id: Some(42),
                        changed: true,
                        touched_paths: vec!["tracked.txt".to_string()],
                        payload: Some(sample_payload("finished")),
                        finished_at_ms: 1_700_000_000_002,
                    }),
                    vec![
                        TimelineLabel::ExternalSideEffectsUnknown,
                        TimelineLabel::RepoReversible,
                    ],
                ),
            ),
            (
                "cursor_moved",
                TimelineOperationEnvelope::new(
                    TimelineOperationBodyV1::CursorMoved(CursorMovedV1 {
                        thread: "main".to_string(),
                        branch_id: TimelineBranchId::new("tlb-main"),
                        from_step_id: Some(TimelineStepId::new("tls-step")),
                        to_step_id: None,
                        from_state: StateId::from_bytes([2; 32]),
                        to_state: StateId::from_bytes([1; 32]),
                        reason: TimelineCursorMoveReason::Undo,
                        moved_at_ms: 1_700_000_000_003,
                    }),
                    Vec::new(),
                ),
            ),
            (
                "branch_created",
                TimelineOperationEnvelope::new(
                    TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                        thread: "main".to_string(),
                        branch_id: TimelineBranchId::new("tlb-child"),
                        parent_branch_id: Some(TimelineBranchId::new("tlb-main")),
                        from_step_id: Some(TimelineStepId::new("tls-step")),
                        from_state: StateId::from_bytes([2; 32]),
                        reason: TimelineBranchReason::ExplicitFork,
                        created_at_ms: 1_700_000_000_004,
                    }),
                    vec![TimelineLabel::RepoReversible],
                ),
            ),
        ]
    }

    #[test]
    fn timeline_encode_decode_round_trips() {
        let envelope = sample_envelope();
        let bytes = envelope.encode().unwrap();
        let decoded = TimelineOperationEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.schema_version, TIMELINE_OPERATION_SCHEMA_VERSION);
        assert_eq!(decoded.kind, TimelineOperationKind::ToolCallStarted);
    }

    #[test]
    fn timeline_operation_id_is_stable_over_bytes() {
        let bytes = sample_envelope().encode().unwrap();
        let id = TimelineOperationId::for_bytes(&bytes);
        assert_eq!(id, TimelineOperationId::for_bytes(&bytes));
        assert_ne!(id, TimelineOperationId::for_bytes(b"different"));
        assert_eq!(
            TimelineOperationId::try_from_slice(id.as_bytes()).unwrap(),
            id
        );
        assert!(id.to_string().starts_with("tl-"));
    }

    #[test]
    fn timeline_operation_golden_fixtures_match_canonical_bytes_and_ids() {
        let actual = golden_envelopes()
            .into_iter()
            .map(|(name, envelope)| {
                let bytes = envelope.encode().unwrap();
                let decoded = TimelineOperationEnvelope::decode(&bytes).unwrap();
                assert_eq!(decoded.encode().unwrap(), bytes);
                (
                    name,
                    bytes.len(),
                    TimelineOperationId::for_bytes(&bytes).to_hex(),
                )
            })
            .collect::<Vec<_>>();
        let expected = vec![
            (
                "tool_call_started",
                484,
                "1d24c2d5e716e6e37e714af7318e37d050f6fa94ec19e4c46e8d955c5613af99".to_string(),
            ),
            (
                "tool_call_finished",
                637,
                "050d4345073bbfa132b868f68b267593a4bc8f57bebd2f36bf2426da92fa2b9f".to_string(),
            ),
            (
                "cursor_moved",
                260,
                "811a82ee991d9a50041c86ef649942837ae8cc4eca073b52c9d7458b707b3fb3".to_string(),
            ),
            (
                "branch_created",
                258,
                "50d2f8103b083a86f40e8c9d250ec7f061c2fcc24314230b5d4b574b73035502".to_string(),
            ),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn timeline_decode_rejects_unknown_version() {
        let bytes = sample_envelope().encode().unwrap();
        let mut wire: TimelineOperationEnvelopeWireV1 = rmp_serde::from_slice(&bytes).unwrap();
        wire.schema_version = 99;
        let bytes = rmp_serde::to_vec_named(&wire).unwrap();
        assert!(matches!(
            TimelineOperationEnvelope::decode(&bytes),
            Err(TimelineCodecError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn timeline_decode_rejects_unknown_kind() {
        let bytes = sample_envelope().encode().unwrap();
        let mut wire: TimelineOperationEnvelopeWireV1 = rmp_serde::from_slice(&bytes).unwrap();
        wire.kind = "tool_call_teleported".to_string();
        let bytes = rmp_serde::to_vec_named(&wire).unwrap();
        assert!(matches!(
            TimelineOperationEnvelope::decode(&bytes),
            Err(TimelineCodecError::UnknownKind(kind)) if kind == "tool_call_teleported"
        ));
    }

    #[test]
    fn generated_step_and_branch_ids_are_prefixed() {
        assert!(TimelineStepId::generate().as_str().starts_with("tls-"));
        assert!(TimelineBranchId::generate().as_str().starts_with("tlb-"));
    }
}
