// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Git notes attached at `refs/notes/heddle` carry Heddle state metadata
//! (change_id, agent, confidence, status) without polluting the commit
//! message — and so without changing the commit SHA.
//!
//! This is the history-carrying half of the export identity model. The
//! `git-projection-mapping.json` sidecar is a local served/export cache; notes are
//! the portable source that survives plain Git clones and exports.
//!
//! Sley provides the tree-backed notes plumbing. This module owns the fixed
//! `refs/notes/heddle` location and re-exports the object model's one canonical
//! payload codec for projection callers.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use objects::object::StateId;
use sley::{ObjectId, Repository};

use super::git_core::{GitProjectionError, GitProjectionResult, git_err};

pub use objects::object::{HeddleNote, NoteAgent, NoteAttribution, OmittedBreakdown, SignalCounts};

/// The notes ref heddle uses. Git-compatible notes readers can opt into
/// this location, while Heddle reads and writes it natively.
pub const NOTES_REF: &str = "refs/notes/heddle";

fn notes_ref() -> sley::notes::NotesRef {
    sley::notes::NotesRef::expand(NOTES_REF)
}

/// Attach `note` to `commit_oid` in `repo` under `refs/notes/heddle`.
///
/// Each call creates one new notes commit on top of any previous notes
/// history. The notes ref is updated atomically via sley's notes plumbing.
pub fn write_note(
    repo: &Repository,
    commit_oid: ObjectId,
    note: &HeddleNote,
) -> GitProjectionResult<()> {
    let json = note
        .to_json_bytes()
        .map_err(|error| GitProjectionError::Git(format!("note serialize: {error}")))?;
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
        &git_projection_notes_identity(),
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
) -> GitProjectionResult<()> {
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
        &git_projection_notes_identity(),
        sley::notes::notes_ref_expected(&refs, &notes_ref).map_err(git_err)?,
    )
    .map_err(git_err)?;
    Ok(())
}

/// Look up the note attached to `commit_oid`, if any.
pub fn read_note(
    repo: &Repository,
    commit_oid: ObjectId,
) -> GitProjectionResult<Option<HeddleNote>> {
    let Some(bytes) = repo
        .read_note_bytes(&notes_ref(), &commit_oid)
        .map_err(git_err)?
    else {
        return Ok(None);
    };
    HeddleNote::from_json_bytes(&bytes)
        .map(Some)
        .map_err(|error| GitProjectionError::Git(format!("note parse: {error}")))
}

/// Read every portable Git↔Heddle identity recorded under `refs/notes/heddle`.
pub fn read_identity_mappings(repo: &Repository) -> GitProjectionResult<Vec<(StateId, ObjectId)>> {
    read_all_notes(repo)?
        .into_iter()
        .map(|(oid, note)| {
            Ok((
                StateId::parse(&note.state_id)
                    .map_err(|error| GitProjectionError::InvalidMapping(error.to_string()))?,
                oid,
            ))
        })
        .collect()
}

/// Read every (commit_oid → note) entry under `refs/notes/heddle`.
pub fn read_all_notes(repo: &Repository) -> GitProjectionResult<HashMap<ObjectId, HeddleNote>> {
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

fn git_projection_notes_identity() -> sley::notes::NotesCommitIdentity {
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
