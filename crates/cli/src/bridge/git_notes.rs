// SPDX-License-Identifier: Apache-2.0
//! Git notes attached at `refs/notes/heddle` carry Heddle state metadata
//! (change_id, agent, confidence, status) without polluting the commit
//! message — and so without changing the commit SHA.
//!
//! This is the "fallback channel" half of the Phase B identity model. The
//! primary channel is the `bridge-mapping.json` sidecar; notes are consulted
//! when the sidecar is missing or empty (e.g., a developer ran `git clone
//! <url>` of a heddle-exported repo without copying the heddle dir).
//!
//! Notes use the standard tree layout (entry name = full 40-hex commit SHA,
//! entry blob = serialized JSON). Tree read/write is delegated to
//! [`sley-notes`] (fanout-aware reads, flat writes, incremental upsert/remove).

use std::collections::HashMap;

use git_substrate::{
    bridge_reflog_committer, iter_notes, notes_ref_expected, read_note_bytes, remove_notes_for,
    upsert_note_bytes_for, GitRepo, NotesCommitIdentity, NotesRef, ObjectId,
};
use objects::object::{State, Status};
use serde::{Deserialize, Serialize};

use super::git_core::{git_err, GitBridgeError, GitResult};

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

fn heddle_notes_ref() -> NotesRef {
    NotesRef::expand(NOTES_REF)
}

fn notes_commit_identity() -> NotesCommitIdentity {
    let actor = bridge_reflog_committer();
    NotesCommitIdentity {
        author: actor.clone(),
        committer: actor,
    }
}

/// Attach `note` to `commit_oid` in `repo` under `refs/notes/heddle`.
///
/// Each call creates one new notes commit on top of any previous notes
/// history when the payload changes. The notes ref is updated atomically
/// via compare-and-swap on the prior notes head.
pub fn write_note(repo: &GitRepo, commit_oid: &ObjectId, note: &HeddleNote) -> GitResult<()> {
    write_note_repo(repo, commit_oid, note)
}

pub(crate) fn write_note_repo(
    repo: &GitRepo,
    commit_oid: &ObjectId,
    note: &HeddleNote,
) -> GitResult<()> {
    let json = note.to_json_bytes()?;
    let notes_ref = heddle_notes_ref();
    let store = repo.ref_store();
    let ref_expected = notes_ref_expected(&store, &notes_ref).map_err(git_err)?;
    upsert_note_bytes_for(
        repo.git_dir(),
        repo.object_format(),
        &store,
        &notes_ref,
        commit_oid,
        &json,
        "heddle: write state note",
        &notes_commit_identity(),
        ref_expected,
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
    repo: &GitRepo,
    commit_oids: &std::collections::HashSet<ObjectId>,
) -> GitResult<()> {
    remove_notes_repo(repo, commit_oids)
}

pub(crate) fn remove_notes_repo(
    repo: &GitRepo,
    commit_oids: &std::collections::HashSet<ObjectId>,
) -> GitResult<()> {
    if commit_oids.is_empty() {
        return Ok(());
    }
    let notes_ref = heddle_notes_ref();
    let store = repo.ref_store();
    let ref_expected = notes_ref_expected(&store, &notes_ref).map_err(git_err)?;
    if ref_expected.is_none() {
        return Ok(());
    }
    let annotated: Vec<ObjectId> = commit_oids.iter().copied().collect();
    remove_notes_for(
        repo.git_dir(),
        repo.object_format(),
        &store,
        &notes_ref,
        &annotated,
        "heddle: retract state metadata",
        &notes_commit_identity(),
        ref_expected,
    )
    .map_err(git_err)?;
    Ok(())
}

/// Look up the note attached to `commit_oid`, if any.
pub fn read_note(repo: &GitRepo, commit_oid: &ObjectId) -> GitResult<Option<HeddleNote>> {
    read_note_repo(repo, commit_oid)
}

pub(crate) fn read_note_repo(repo: &GitRepo, commit_oid: &ObjectId) -> GitResult<Option<HeddleNote>> {
    let notes_ref = heddle_notes_ref();
    let store = repo.ref_store();
    let Some(bytes) = read_note_bytes(
        repo.git_dir(),
        repo.object_format(),
        &store,
        &notes_ref,
        commit_oid,
    )
    .map_err(git_err)?
    else {
        return Ok(None);
    };
    HeddleNote::from_json_bytes(&bytes).map(Some)
}

/// Read every (commit_oid → note) entry under `refs/notes/heddle`. Used by
/// the import path's identity-recovery scan.
pub fn read_all_notes(repo: &GitRepo) -> GitResult<HashMap<ObjectId, HeddleNote>> {
    read_all_notes_repo(repo)
}

pub(crate) fn read_all_notes_repo(repo: &GitRepo) -> GitResult<HashMap<ObjectId, HeddleNote>> {
    let mut out = HashMap::new();
    let notes_ref = heddle_notes_ref();
    let store = repo.ref_store();
    for entry in iter_notes(
        repo.git_dir(),
        repo.object_format(),
        &store,
        &notes_ref,
    )
    .map_err(git_err)?
    {
        let entry = entry.map_err(git_err)?;
        if let Ok(data) = repo.read_blob(&entry.blob).map_err(git_err)
            && let Ok(note) = HeddleNote::from_json_bytes(&data)
        {
            out.insert(entry.annotated, note);
        }
    }
    Ok(out)
}