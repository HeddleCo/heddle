// SPDX-License-Identifier: Apache-2.0
//! Canonical payload carried by `refs/notes/heddle`.
//!
//! Git projection owns where the payload is stored and how the notes ref is
//! updated. The object model owns these durable bytes so projection, ingest,
//! fsck, and hosted consumers cannot grow independent JSON schemas.

use serde::{Deserialize, Serialize};

use super::{State, Status};

/// Portable Heddle metadata attached to a projected Git commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeddleNote {
    pub state_id: String,
    pub change_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_state: Option<State>,
    /// Whether Git projection changed the parent graph represented by the
    /// embedded source state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents_rewritten: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<NoteAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Either `draft` or `published`.
    pub status: String,
    /// Per-scope counts of annotations omitted from a Git export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_annotations_breakdown: Option<OmittedBreakdown>,
    /// Per-module risk-signal counts observed at export time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_counts: Option<SignalCounts>,
    /// Author and agent attribution not representable by a Git signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<NoteAttribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteAgent {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OmittedBreakdown {
    #[serde(default)]
    pub internal: u32,
    #[serde(default)]
    pub team: u32,
    #[serde(default)]
    pub restricted: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalCounts {
    #[serde(default)]
    pub novelty: u32,
    #[serde(default)]
    pub test_reachability: u32,
    #[serde(default)]
    pub pattern_deviation: u32,
    #[serde(default)]
    pub invariant_adjacency: u32,
    #[serde(default)]
    pub self_flagged_uncertainty: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteAttribution {
    pub principal_name: String,
    pub principal_email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<NoteAgent>,
}

impl HeddleNote {
    /// Construct the canonical note for a state projected without rewriting
    /// its parent graph.
    pub fn from_state(state: &State) -> Self {
        let status = match state.status {
            Status::Draft => "draft".to_string(),
            Status::Published => "published".to_string(),
        };
        let agent = state.attribution.agent.as_ref().map(|agent| NoteAgent {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
        });
        Self {
            state_id: state.id().to_string_full(),
            change_id: state.change_id.to_string_full(),
            source_state: Some(state.clone()),
            parents_rewritten: false,
            agent,
            confidence: state.confidence,
            status,
            omitted_annotations_breakdown: None,
            signal_counts: None,
            attribution: None,
        }
    }

    /// Construct a note for a Git projection whose parent graph differs from
    /// the embedded source state.
    pub fn from_projected_state(state: &State) -> Self {
        let mut note = Self::from_state(state);
        note.parents_rewritten = true;
        note
    }

    pub fn with_omitted_breakdown(mut self, breakdown: OmittedBreakdown) -> Self {
        self.omitted_annotations_breakdown = Some(breakdown);
        self
    }

    pub fn with_signal_counts(mut self, counts: SignalCounts) -> Self {
        self.signal_counts = Some(counts);
        self
    }

    pub fn with_attribution(mut self, attribution: NoteAttribution) -> Self {
        self.attribution = Some(attribution);
        self
    }

    /// Encode the one canonical JSON representation written to Git notes.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Decode the canonical Git-note representation.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let mut note: Self = serde_json::from_slice(bytes)?;
        if let Some(source_state) = &mut note.source_state {
            source_state.state_id = source_state.id();
        }
        Ok(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Attribution, Principal, State, Tree};

    fn state() -> State {
        State::new(
            Tree::new().hash(),
            Vec::new(),
            Attribution::human(Principal::new("Test User", "test@example.com")),
        )
    }

    #[test]
    fn canonical_note_roundtrips_every_field() {
        let note = HeddleNote::from_projected_state(&state())
            .with_omitted_breakdown(OmittedBreakdown {
                internal: 1,
                team: 2,
                restricted: 3,
            })
            .with_signal_counts(SignalCounts {
                novelty: 4,
                test_reachability: 5,
                pattern_deviation: 6,
                invariant_adjacency: 7,
                self_flagged_uncertainty: 8,
            })
            .with_attribution(NoteAttribution {
                principal_name: "Test User".to_string(),
                principal_email: "test@example.com".to_string(),
                agent: Some(NoteAgent {
                    provider: "openai".to_string(),
                    model: "codex".to_string(),
                }),
            });

        let bytes = note.to_json_bytes().expect("encode canonical note");
        assert_eq!(
            HeddleNote::from_json_bytes(&bytes).expect("decode canonical note"),
            note
        );
    }

    #[test]
    fn old_note_without_parent_marker_defaults_to_unmodified_graph() {
        let source = state();
        let bytes = serde_json::json!({
            "state_id": source.id().to_string_full(),
            "change_id": source.change_id.to_string_full(),
            "status": "draft"
        })
        .to_string();

        let note = HeddleNote::from_json_bytes(bytes.as_bytes()).expect("decode note");
        assert!(!note.parents_rewritten);
    }

    #[test]
    fn foreign_note_missing_required_identity_is_not_a_heddle_note() {
        let bytes = br#"{"state_id":"hs-deadbeef","status":"published"}"#;
        assert!(HeddleNote::from_json_bytes(bytes).is_err());
    }
}
