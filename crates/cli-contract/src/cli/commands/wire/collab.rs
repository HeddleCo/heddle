// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for `discuss`, `review`, and `watch`.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;

// ---- discuss ---------------------------------------------------------------

#[derive(Serialize, JsonSchema)]
pub struct DiscussionOutput {
    pub id: String,
    pub title: String,
    pub anchor: AnchorOutput,
    pub anchor_status: &'static str,
    pub visibility: String,
    pub thread_ref: Option<String>,
    pub status: &'static str,
    pub resolution: Option<ResolutionOutput>,
    pub conflict_operation_ids: Vec<String>,
    pub head_operation_ids: Vec<String>,
    pub display_head_operation_id: String,
    pub turns: Vec<TurnOutput>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AnchorOutput {
    Repository,
    State {
        state_id: String,
    },
    Change {
        change_id: String,
    },
    Path {
        state_id: String,
        path: String,
    },
    Symbol {
        state_id: String,
        path: String,
        symbol: String,
    },
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ResolutionOutput {
    AddressedByState {
        state_id: String,
    },
    AddressedByChange {
        change_id: String,
    },
    Dismissed {
        reason: String,
    },
    IntoAnnotation {
        annotation_kind: String,
        content: String,
        tags: Vec<String>,
    },
    Annotation {
        annotation_id: String,
    },
}

#[derive(Serialize, JsonSchema)]
pub struct TurnOutput {
    pub operation_id: String,
    pub author_name: String,
    pub author_email: String,
    pub agent: Option<String>,
    pub occurred_at_ms: i64,
    pub body: String,
    pub content_hash: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "DiscussionWriteSchema")]
pub struct DiscussionWriteOutput {
    pub output_kind: &'static str,
    pub operation_id: String,
    pub disposition: repo::CollaborationWriteDisposition,
    pub discussion: DiscussionOutput,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "DiscussionShowSchema")]
pub struct DiscussionShowOutput {
    pub output_kind: &'static str,
    pub discussion: DiscussionOutput,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "DiscussionListSchema")]
pub struct DiscussionListOutput {
    pub output_kind: &'static str,
    pub discussions: Vec<DiscussionOutput>,
}

// ---- review ----------------------------------------------------------------

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ReviewShowSchema")]
pub struct ReviewShowOutput {
    pub output_kind: &'static str,
    pub state_id: String,
    /// Named comparison base selected for the existing review payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub headline: String,
    pub agent_narrative: Option<String>,
    pub files_changed: u32,
    pub in_budget_signals: Vec<SignalView>,
    pub all_signals: Vec<SignalView>,
    pub discussions: Vec<DiscussionView>,
    pub signing_kinds: Vec<String>,
    pub signatures: Vec<SignatureView>,
}

#[derive(Serialize, JsonSchema)]
pub struct SignalView {
    pub kind: String,
    pub file: String,
    pub symbol: String,
    pub reason: String,
    pub producer: String,
    pub visibility: String,
}

#[derive(Serialize, JsonSchema)]
pub struct DiscussionView {
    pub id: String,
    pub file: String,
    pub symbol: String,
    pub status: String,
    pub body_changed_since_open: bool,
    pub anchor_ambiguous: bool,
    pub orphaned: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct SignatureView {
    pub actor_name: String,
    pub actor_email: String,
    pub kind: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub glyph: &'static str,
    pub is_agent: bool,
    pub signed_at_secs: i64,
    pub scope_kind: String,
    pub scope_symbols: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ReviewSignSchema")]
pub struct ReviewSignOutput {
    pub output_kind: &'static str,
    pub signature_id: String,
    pub state_id: String,
}

/// The pending review state echoed under `review next`'s `next` field.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct NextStateView {
    pub state_id: String,
    pub headline: String,
    pub existing_signatures: u32,
}

/// `review next` emits a stable envelope keyed by `output_kind`. When the
/// scan window holds a pending review, its view is flattened alongside
/// `output_kind` and echoed under `next`; otherwise only `output_kind` and
/// `next: null` are emitted. `next` is ALWAYS present — the wrapper keeps it
/// required (heddle#272 Codex r7).
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ReviewNextSchema")]
pub struct ReviewNextOutput {
    pub output_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_signatures: Option<u32>,
    pub next: RequiredNullableNextState,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct RequiredNullableNextState(pub Option<NextStateView>);

impl JsonSchema for RequiredNullableNextState {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("RequiredNullableNextState")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <Option<NextStateView> as JsonSchema>::json_schema(generator)
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ReviewHealthSchema")]
pub struct ReviewHealthOutput {
    pub output_kind: &'static str,
    pub entries: Vec<HealthEntry>,
    pub window_states: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct HealthEntry {
    pub module_id: String,
    pub fire_rate: f64,
    pub warn: bool,
}

// ---- watch -----------------------------------------------------------------

/// One `heddle watch` line.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "WatchLineSchema")]
pub struct WatchLineOutput {
    pub ts: String,
    pub thread: Option<String>,
    pub kind: String,
    pub state_id: Option<String>,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub actor: Option<WatchActorInfo>,
    /// Numeric oplog id, useful for downstream cursor tracking.
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct WatchActorInfo {
    pub provider: String,
    pub model: String,
}
