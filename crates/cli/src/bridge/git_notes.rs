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
//! gix v0.80 has no high-level notes API; we hand-roll the standard tree
//! layout (entry name = full 40-hex commit SHA, entry blob = serialized JSON)
//! using the same primitives the rest of the bridge already relies on
//! (`write_blob`, `edit_tree`, `new_commit_as`, `set_reference`).

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use gix::{hash::ObjectId, refs::transaction::PreviousValue};
use objects::object::{State, Status};
use serde::{Deserialize, Serialize};

use super::git_core::{GitBridgeError, GitResult, git_err, set_reference};

/// The notes ref heddle uses. Standard `git notes --ref=heddle` reads
/// from this location, and `git log --notes=heddle` displays them inline.
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

/// Resolve the current notes-commit OID and the tree it points at. Returns
/// `(None, empty_tree)` when `refs/notes/heddle` does not yet exist.
fn read_notes_head(repo: &gix::Repository) -> GitResult<(Option<ObjectId>, ObjectId)> {
    let parent = match repo.find_reference(NOTES_REF) {
        Ok(reference) => {
            let mut reference = reference;
            Some(reference.peel_to_id().map_err(git_err)?.detach())
        }
        Err(_) => None,
    };
    let tree_oid = if let Some(commit_oid) = parent {
        let commit = repo.find_commit(commit_oid).map_err(git_err)?;
        commit.tree_id().map_err(git_err)?.detach()
    } else {
        gix::hash::ObjectId::empty_tree(repo.object_hash())
    };
    Ok((parent, tree_oid))
}

/// Attach `note` to `commit_oid` in `repo` under `refs/notes/heddle`.
///
/// Each call creates one new notes commit on top of any previous notes
/// history. The notes ref is updated atomically via the bridge's standard
/// `set_reference` helper.
pub fn write_note(
    repo: &gix::Repository,
    commit_oid: ObjectId,
    note: &HeddleNote,
) -> GitResult<()> {
    let json = note.to_json_bytes()?;
    let blob_oid = repo.write_blob(&json).map_err(git_err)?.detach();

    let (parent_commit, current_tree_oid) = read_notes_head(repo)?;
    let mut editor = repo.edit_tree(current_tree_oid).map_err(git_err)?;
    let entry_name = commit_oid.to_hex_with_len(40).to_string();
    editor
        .upsert(
            entry_name.as_str(),
            gix::object::tree::EntryKind::Blob,
            blob_oid,
        )
        .map_err(git_err)?;
    let new_tree_oid = editor.write().map_err(git_err)?.detach();

    let signature = bridge_notes_signature();
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let parents: Vec<ObjectId> = parent_commit.iter().copied().collect();
    let new_commit = repo
        .new_commit_as(
            signature.to_ref(&mut committer_buf),
            signature.to_ref(&mut author_buf),
            "heddle: state metadata",
            new_tree_oid,
            parents,
        )
        .map_err(git_err)?;

    let constraint = match parent_commit {
        Some(prev) => PreviousValue::ExistingMustMatch(gix::refs::Target::Object(prev)),
        None => PreviousValue::MustNotExist,
    };
    set_reference(
        repo,
        NOTES_REF,
        new_commit.id,
        constraint,
        "heddle: write state note",
    )?;
    Ok(())
}

/// Look up the note attached to `commit_oid`, if any.
pub fn read_note(repo: &gix::Repository, commit_oid: ObjectId) -> GitResult<Option<HeddleNote>> {
    let Ok(reference) = repo.find_reference(NOTES_REF) else {
        return Ok(None);
    };
    let mut reference = reference;
    let notes_commit_oid = reference.peel_to_id().map_err(git_err)?.detach();
    let notes_commit = repo.find_commit(notes_commit_oid).map_err(git_err)?;
    let notes_tree_oid = notes_commit.tree_id().map_err(git_err)?.detach();
    let notes_tree = repo.find_tree(notes_tree_oid).map_err(git_err)?;

    let target_name = commit_oid.to_hex_with_len(40).to_string();
    for entry in notes_tree.iter() {
        let entry = entry.map_err(git_err)?;
        if *entry.filename() == *target_name.as_bytes() {
            let object = repo.find_object(entry.object_id()).map_err(git_err)?;
            return HeddleNote::from_json_bytes(&object.data).map(Some);
        }
    }
    Ok(None)
}

/// Read every (commit_oid → note) entry under `refs/notes/heddle`. Used by
/// the import path's identity-recovery scan.
pub fn read_all_notes(repo: &gix::Repository) -> GitResult<HashMap<ObjectId, HeddleNote>> {
    let mut out = HashMap::new();
    let Ok(reference) = repo.find_reference(NOTES_REF) else {
        return Ok(out);
    };
    let mut reference = reference;
    let notes_commit_oid = reference.peel_to_id().map_err(git_err)?.detach();
    let notes_commit = repo.find_commit(notes_commit_oid).map_err(git_err)?;
    let notes_tree_oid = notes_commit.tree_id().map_err(git_err)?.detach();
    let notes_tree = repo.find_tree(notes_tree_oid).map_err(git_err)?;

    for entry in notes_tree.iter() {
        let entry = entry.map_err(git_err)?;
        let name = entry.filename().to_string();
        let Ok(target_oid) = name.parse::<ObjectId>() else {
            continue;
        };
        let object = repo.find_object(entry.object_id()).map_err(git_err)?;
        // Skip entries that aren't well-formed heddle notes — could be left
        // over from `git notes --ref=heddle add` by an external tool.
        if let Ok(note) = HeddleNote::from_json_bytes(&object.data) {
            out.insert(target_oid, note);
        }
    }
    Ok(out)
}

fn bridge_notes_signature() -> gix::actor::Signature {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    gix::actor::Signature {
        name: "Heddle".into(),
        email: "heddle@local".into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}