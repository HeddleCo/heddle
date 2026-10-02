// SPDX-License-Identifier: Apache-2.0
//! Canonical payload carried by `refs/notes/heddle`.
//!
//! Git projection owns where the payload is stored and how the notes ref is
//! updated. The object model owns these durable bytes so projection, ingest,
//! fsck, and hosted consumers cannot grow independent JSON schemas.
//!
//! `source_state`, when present, is the canonical named MessagePack body
//! (`rmp_serde::to_vec_named`) hex-encoded inside that JSON document.

use serde::{Deserialize, Serialize};

use super::{Agent, AttributionEvidenceV1, Blob, State, Status};
use crate::error::HeddleError;

/// Portable Heddle metadata attached to a projected Git commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeddleNote {
    pub state_id: String,
    pub change_id: String,
    /// Hex-encoded canonical MessagePack. Absent or null when the note has no
    /// embedded state. A JSON object is the retired inline `State` and is
    /// rejected so serde_json never monomorphizes `State`'s deserializer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "source_state_msgpack"
    )]
    pub source_state: Option<State>,
    /// Canonical attribution evidence bytes, hex-encoded for portability.
    /// Required whenever the embedded State commits to an evidence blob.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_hex_bytes"
    )]
    pub attribution_evidence: Option<Vec<u8>>,
    /// Whether Git projection changed the parent graph represented by the
    /// embedded source state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents_rewritten: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
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
    pub agent: Option<Agent>,
}

/// Serde adapter for [`HeddleNote::source_state`].
///
/// `None` stays JSON null (and is omitted by `skip_serializing_if`). `Some`
/// is `hex(rmp_serde::to_vec_named(state))`. JSON objects are not decoded as
/// `State`; callers must re-export the note with a current heddle.
mod source_state_msgpack {
    use serde::de::Error as _;
    use serde::ser::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::State;

    pub fn serialize<S>(value: &Option<State>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            None => serializer.serialize_none(),
            Some(state) => {
                let bytes = rmp_serde::to_vec_named(state).map_err(S::Error::custom)?;
                serializer.serialize_str(&hex::encode(bytes))
            }
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<State>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(value) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let bytes = hex::decode(value).map_err(D::Error::custom)?;
        rmp_serde::from_slice(&bytes)
            .map(Some)
            .map_err(D::Error::custom)
    }
}

mod optional_hex_bytes {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(bytes) => serializer.serialize_str(&hex::encode(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|value| {
                if value.len() > super::super::ATTRIBUTION_EVIDENCE_MAX_BYTES * 2 {
                    return Err(D::Error::custom(
                        "Git-note attribution evidence exceeds its size limit",
                    ));
                }
                hex::decode(value).map_err(D::Error::custom)
            })
            .transpose()
    }
}

impl HeddleNote {
    /// Construct the canonical note for a state projected without rewriting
    /// its parent graph.
    pub fn from_state(state: &State) -> Self {
        let status = match state.status {
            Status::Draft => "draft".to_string(),
            Status::Published => "published".to_string(),
        };
        let agent = state.attribution.agent.clone();
        Self {
            state_id: state.id().to_string_full(),
            change_id: state.change_id.to_string_full(),
            source_state: Some(state.clone()),
            attribution_evidence: None,
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

    /// Return the validated, content-addressed attribution blob carried by this note.
    /// A portable source State may not omit or replace its committed evidence.
    pub fn attribution_evidence_blob(&self) -> crate::error::Result<Option<Blob>> {
        let blob = self
            .attribution_evidence
            .as_ref()
            .map(|bytes| {
                AttributionEvidenceV1::from_bytes(bytes)
                    .and_then(|evidence| {
                        evidence.validate_legacy_agent(
                            self.attribution
                                .as_ref()
                                .and_then(|value| value.agent.as_ref())
                                .or(self.agent.as_ref()),
                        )?;
                        // Both compatibility copies can be read independently.
                        // A nested override must not mask a contradictory agent.
                        if let Some(agent) = &self.agent {
                            evidence.validate_legacy_agent(Some(agent))?;
                        }
                        if let Some(state) = &self.source_state {
                            evidence.validate_legacy_agent(state.attribution.agent.as_ref())?;
                        }
                        Ok(())
                    })
                    .map_err(|error| {
                        HeddleError::InvalidObject(format!(
                            "invalid Git-note attribution evidence: {error}"
                        ))
                    })?;
                Ok::<_, HeddleError>(Blob::new(bytes.clone()))
            })
            .transpose()?;
        if let Some(state) = &self.source_state
            && state.attribution_evidence != blob.as_ref().map(Blob::hash)
        {
            return Err(HeddleError::InvalidObject(
                "Git-note attribution evidence differs from embedded State".into(),
            ));
        }
        Ok(blob)
    }

    /// Encode the one canonical JSON representation written to Git notes.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        self.attribution_evidence_blob()
            .map_err(<serde_json::Error as serde::ser::Error>::custom)?;
        serde_json::to_vec_pretty(self)
    }

    /// Decode the canonical Git-note representation.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let mut note: Self = serde_json::from_slice(bytes)?;
        if let Some(source_state) = &mut note.source_state {
            source_state.state_id = source_state.id();
        }
        note.attribution_evidence_blob()
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
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
                agent: Some(Agent::new("openai", "codex")),
            });

        let bytes = note.to_json_bytes().expect("encode canonical note");
        let encoded: serde_json::Value =
            serde_json::from_slice(&bytes).expect("note stays a JSON document");
        let source_state = encoded["source_state"]
            .as_str()
            .expect("source_state is hex text, not an embedded State object");
        assert!(
            !source_state.is_empty() && source_state.chars().all(|c| c.is_ascii_hexdigit()),
            "source_state must be hex-encoded MessagePack: {source_state}"
        );
        assert_eq!(
            HeddleNote::from_json_bytes(&bytes).expect("decode canonical note"),
            note
        );
    }

    #[test]
    fn null_source_state_is_absent() {
        let source = state();
        let null_note = serde_json::json!({
            "state_id": source.id().to_string_full(),
            "change_id": source.change_id.to_string_full(),
            "source_state": null,
            "status": "draft"
        })
        .to_string();
        let note = HeddleNote::from_json_bytes(null_note.as_bytes()).expect("null source_state");
        assert!(note.source_state.is_none());
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

#[cfg(test)]
mod attribution_tests {
    use super::*;
    use crate::object::{
        Attribution, AttributionBasis, AttributionClaim, AttributionSource, Principal, Tree,
    };

    fn evidence() -> Blob {
        AttributionEvidenceV1 {
            harness: Some(AttributionClaim::new(
                "codex",
                AttributionBasis::Observed,
                AttributionSource::Process,
            )),
            ..Default::default()
        }
        .to_blob()
        .expect("evidence")
    }

    fn state() -> State {
        State::new(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("Test", "test@example.test")),
        )
    }

    #[test]
    fn attribution_note_roundtrip_requires_exact_typed_blob() {
        let evidence = evidence();
        let state = state().with_attribution_evidence(evidence.hash());
        let mut note = HeddleNote::from_state(&state);
        assert!(
            note.to_json_bytes().is_err(),
            "cannot publish source metadata without evidence"
        );
        let missing = serde_json::to_vec(&note).expect("unvalidated hostile note");
        assert!(HeddleNote::from_json_bytes(&missing).is_err());
        note.attribution_evidence = Some(evidence.content().to_vec());
        let bytes = note.to_json_bytes().expect("complete note");
        let roundtrip = HeddleNote::from_json_bytes(&bytes).expect("decode");
        assert_eq!(roundtrip.source_state, Some(state));
        assert_eq!(
            roundtrip
                .attribution_evidence_blob()
                .expect("blob")
                .expect("present")
                .hash(),
            evidence.hash()
        );
        let mut wrong = note.clone();
        wrong.agent = Some(Agent::new("unobserved", "unobserved"));
        assert!(
            wrong.to_json_bytes().is_err(),
            "note display metadata cannot contradict committed claims"
        );
        let mut wrong = note.clone();
        wrong.attribution_evidence = Some(b"not canonical evidence".to_vec());
        assert!(wrong.to_json_bytes().is_err());
        let mut another = AttributionEvidenceV1::from_blob(&evidence).expect("evidence");
        another.harness.as_mut().expect("harness").value = "other".into();
        wrong.attribution_evidence = Some(another.to_bytes().expect("other evidence"));
        assert!(
            wrong.to_json_bytes().is_err(),
            "well-formed evidence still needs the exact committed hash"
        );
    }

    #[test]
    fn attribution_note_rejects_conflicting_compatibility_agents() {
        let mut evidence = AttributionEvidenceV1::default();
        evidence.selected.provider = Some(AttributionClaim::new(
            "router",
            AttributionBasis::Configured,
            AttributionSource::Configuration,
        ));
        evidence.selected.model = Some(AttributionClaim::new(
            "selected-model",
            AttributionBasis::RequestReported,
            AttributionSource::Request,
        ));
        let blob = evidence.to_blob().expect("evidence");
        let agent = evidence.legacy_agent().expect("agent");
        let mut state = state().with_attribution_evidence(blob.hash());
        state.attribution.agent = Some(agent.clone());
        state.state_id = state.id();
        let mut note = HeddleNote::from_state(&state).with_attribution(NoteAttribution {
            principal_name: "Test".into(),
            principal_email: "test@example.test".into(),
            agent: Some(agent),
        });
        note.attribution_evidence = Some(blob.into_content());
        assert!(note.to_json_bytes().is_ok());

        note.agent = Some(Agent::new("unobserved", "unobserved"));
        assert!(note.to_json_bytes().is_err());
        let hostile = serde_json::to_vec(&note).expect("unvalidated note");
        assert!(HeddleNote::from_json_bytes(&hostile).is_err());
    }

    #[test]
    fn legacy_note_stays_unchanged_and_rewritten_note_keeps_evidence() {
        let legacy = HeddleNote::from_state(&state());
        let bytes = legacy.to_json_bytes().expect("legacy");
        let expected = format!(
            "{{\n  \"state_id\": \"{}\",\n  \"change_id\": \"{}\",\n  \"source_state\": \"{}\",\n  \"status\": \"draft\"\n}}",
            legacy.state_id,
            legacy.change_id,
            hex::encode(
                rmp_serde::to_vec_named(legacy.source_state.as_ref().expect("source"))
                    .expect("legacy source bytes")
            ),
        );
        assert_eq!(
            bytes,
            expected.as_bytes(),
            "legacy Git note bytes and field order remain unchanged"
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("attribution_evidence"));
        let evidence = evidence();
        let mut rewritten =
            HeddleNote::from_projected_state(&state().with_attribution_evidence(evidence.hash()));
        rewritten.source_state = None;
        rewritten.attribution_evidence = Some(evidence.content().to_vec());
        let decoded =
            HeddleNote::from_json_bytes(&rewritten.to_json_bytes().expect("rewritten note"))
                .expect("decode");
        assert_eq!(
            decoded
                .attribution_evidence_blob()
                .expect("blob")
                .expect("present")
                .hash(),
            evidence.hash()
        );
    }
}
