// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Git notes attached at `refs/notes/heddle` carry Heddle state metadata
//! (change_id, agent, confidence, status) without polluting the commit
//! message — and so without changing the commit SHA.
//!
//! This is the history-carrying half of the Phase B identity model. The
//! `bridge-mapping.json` sidecar is a local rebuild cache; notes are the
//! portable source that survives plain Git clones and exports.
//!
//! Sley provides the tree-backed notes plumbing; this module owns Heddle's
//! JSON payload and the fixed `refs/notes/heddle` location.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use objects::object::{ChangeId, State, Status};
use serde::{Deserialize, Serialize};
use sley::{ObjectId, Repository};

use super::git_core::{GitBridgeError, GitResult, git_err};

/// The notes ref heddle uses. Git-compatible notes readers can opt into
/// this location, while Heddle reads and writes it natively.
pub const NOTES_REF: &str = "refs/notes/heddle";

/// JSON payload stored inside each note blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeddleNote {
    /// The heddle change_id this commit corresponds to.
    pub change_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<NoteAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Either "draft" or "published".
    pub status: String,
    // --- W2/R6 tail fields below; new fields go here. All optional + skip-if-none. ---
    /// Per-scope counts of annotations dropped at export because their
    /// visibility exceeded the export's audience tier. Populated when the
    /// caller exports with `--notes` and `--audience`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_annotations_breakdown: Option<OmittedBreakdown>,
    /// Per-module signal counts on the state at export time. Read-only
    /// metadata for downstream tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_counts: Option<SignalCounts>,
    /// Author + agent attribution rolled up into a richer shape than the
    /// commit's own author signature can carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<NoteAttribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteAgent {
    pub provider: String,
    pub model: String,
}

/// Per-scope omitted-annotation counts emitted alongside `refs/notes/heddle`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OmittedBreakdown {
    #[serde(default)]
    pub internal: u32,
    #[serde(default)]
    pub team: u32,
    #[serde(default)]
    pub restricted: u32,
}

/// Per-module risk-signal fire counts on this state.
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
    /// Construct a note from a heddle state (the form written on export).
    pub fn from_state(state: &State) -> Self {
        let status = match state.status {
            Status::Draft => "draft".to_string(),
            Status::Published => "published".to_string(),
        };
        let agent = state.attribution.agent.as_ref().map(|a| NoteAgent {
            provider: a.provider.clone(),
            model: a.model.clone(),
        });
        Self {
            change_id: state.change_id.to_string_full(),
            agent,
            confidence: state.confidence,
            status,
            omitted_annotations_breakdown: None,
            signal_counts: None,
            attribution: None,
        }
    }

    /// R6 builder: set the per-scope omitted-annotation breakdown.
    pub fn with_omitted_breakdown(mut self, breakdown: OmittedBreakdown) -> Self {
        self.omitted_annotations_breakdown = Some(breakdown);
        self
    }

    /// R6 builder: set the per-module signal counts.
    pub fn with_signal_counts(mut self, counts: SignalCounts) -> Self {
        self.signal_counts = Some(counts);
        self
    }

    /// R6 builder: set richer attribution (principal + agent).
    pub fn with_attribution(mut self, attribution: NoteAttribution) -> Self {
        self.attribution = Some(attribution);
        self
    }

    pub fn to_json_bytes(&self) -> GitResult<Vec<u8>> {
        serde_json::to_vec_pretty(self)
            .map_err(|e| GitBridgeError::Git(format!("note serialize: {e}")))
    }

    pub fn from_json_bytes(bytes: &[u8]) -> GitResult<Self> {
        serde_json::from_slice(bytes).map_err(|e| GitBridgeError::Git(format!("note parse: {e}")))
    }
}

fn notes_ref() -> sley::notes::NotesRef {
    sley::notes::NotesRef::expand(NOTES_REF)
}

/// Attach `note` to `commit_oid` in `repo` under `refs/notes/heddle`.
///
/// Each call creates one new notes commit on top of any previous notes
/// history. The notes ref is updated atomically via sley's notes plumbing.
pub fn write_note(repo: &Repository, commit_oid: ObjectId, note: &HeddleNote) -> GitResult<()> {
    let json = note.to_json_bytes()?;
    let notes_ref = notes_ref();
    let refs = repo.references();
    sley::notes::upsert_note_bytes_for(
        repo.git_dir(),
        repo.object_format(),
        &refs,
        &notes_ref,
        &commit_oid,
        &json,
        "heddle: state metadata",
        &bridge_notes_identity(),
        sley::notes::notes_ref_expected(&refs, &notes_ref).map_err(git_err)?,
    )
    .map_err(git_err)?;
    Ok(())
}

/// Retract the notes attached to `commit_oids` from `refs/notes/heddle`.
///
/// The notes ref copies to the public mirror alongside branches and tags
/// (`collect_ref_updates` picks up `refs/notes/*`), so a note left behind for a
/// commit that has since been embargoed/retracted is a metadata leak: the
/// mirror keeps publishing a note whose payload (and tree entry) references the
/// withheld commit. This is the notes-ref sibling of the branch/tag retraction
/// the exporter already performs (heddle#316).
///
/// Writes a single new notes commit dropping every present entry, then advances
/// `refs/notes/heddle` to it. A genuine fast-forward (the new commit descends
/// from the prior notes head), so it survives the bridge's FF guard on push.
/// No-op — no new commit, no ref churn — when the notes ref is absent or none
/// of `commit_oids` actually has an entry.
pub fn remove_notes(
    repo: &Repository,
    commit_oids: &std::collections::HashSet<ObjectId>,
) -> GitResult<()> {
    if commit_oids.is_empty() {
        return Ok(());
    }
    let notes_ref = notes_ref();
    let refs = repo.references();
    let annotated: Vec<ObjectId> = commit_oids.iter().copied().collect();
    sley::notes::remove_notes_for(
        repo.git_dir(),
        repo.object_format(),
        &refs,
        &notes_ref,
        &annotated,
        "heddle: retract state metadata",
        &bridge_notes_identity(),
        sley::notes::notes_ref_expected(&refs, &notes_ref).map_err(git_err)?,
    )
    .map_err(git_err)?;
    Ok(())
}

/// Look up the note attached to `commit_oid`, if any.
pub fn read_note(repo: &Repository, commit_oid: ObjectId) -> GitResult<Option<HeddleNote>> {
    let Some(bytes) = repo
        .read_note_bytes(&notes_ref(), &commit_oid)
        .map_err(git_err)?
    else {
        return Ok(None);
    };
    HeddleNote::from_json_bytes(&bytes).map(Some)
}

/// Read every portable Git↔Heddle identity recorded under `refs/notes/heddle`.
pub(crate) fn read_identity_mappings(repo: &Repository) -> GitResult<Vec<(ChangeId, ObjectId)>> {
    read_all_notes(repo)?
        .into_iter()
        .map(|(oid, note)| Ok((ChangeId::parse(&note.change_id)?, oid)))
        .collect()
}

/// Read every (commit_oid → note) entry under `refs/notes/heddle`.
pub(crate) fn read_all_notes(repo: &Repository) -> GitResult<HashMap<ObjectId, HeddleNote>> {
    let mut out = HashMap::new();
    for note_entry in repo.list_notes(&notes_ref()).map_err(git_err)? {
        let object = repo.read_object(&note_entry.blob).map_err(git_err)?;
        // Skip entries that aren't well-formed heddle notes — could be left
        // over from `git notes --ref=heddle add` by an external tool.
        if object.object_type != sley::GitObjectType::Blob {
            continue;
        }
        if let Ok(note) = HeddleNote::from_json_bytes(&object.body) {
            out.insert(note_entry.annotated, note);
        }
    }
    Ok(out)
}

fn bridge_notes_identity() -> sley::notes::NotesCommitIdentity {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ident = format!("Heddle <heddle@local> {seconds} +0000").into_bytes();
    sley::notes::NotesCommitIdentity {
        author: ident.clone(),
        committer: ident,
    }
}
