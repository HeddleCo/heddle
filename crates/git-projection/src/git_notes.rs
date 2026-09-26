// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Git notes attached at `refs/notes/heddle` carry Heddle state metadata
//! (change_id, agent, confidence, status) without polluting the commit
//! message — and so without changing the commit SHA.
//!
//! This is the portable half of the export identity model. The
//! `git-projection-mapping.json` sidecar is a local served/export cache; notes are
//! the portable source that survives plain Git clones and exports.
//!
//! Sley provides the tree-backed notes plumbing. This module owns the fixed
//! `refs/notes/heddle` location and re-exports the object model's one canonical
//! payload codec for projection callers.

use std::{
    collections::{BTreeMap, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};

pub use objects::object::{HeddleNote, NoteAttribution, OmittedBreakdown, SignalCounts};
use objects::{
    object::{State, StateId, TreeScheme},
    store::ObjectStore,
};
use repo::Repository as HeddleRepository;
use sley::{CommitObject, EntryKind, GitObjectType, ObjectId, Repository, TreeEditor};

use super::git_core::{GitProjectionError, GitProjectionResult, git_err};

/// The notes ref heddle uses. Git-compatible notes readers can opt into
/// this location, while Heddle reads and writes it natively.
pub const NOTES_REF: &str = "refs/notes/heddle";

/// Encode the complete served note set without reading or parenting on the
/// current notes ref. The fixed identity makes the target stable across exports.
/// Ref ownership and compare-and-swap are handled by the caller's reconciler.
pub fn rebuild_notes(
    repo: &Repository,
    entries: &[(ObjectId, Vec<u8>)],
) -> GitProjectionResult<Option<ObjectId>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut notes = entries.to_vec();
    notes.sort_by_key(|(oid, _)| oid.to_hex());
    if notes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(GitProjectionError::Git(
            "multiple served states project to the same Git note target".into(),
        ));
    }
    let mut root = TreeEditor::new();
    if notes.len() < 256 {
        for (annotated, bytes) in &notes {
            let blob = repo.write_blob(bytes.clone()).map_err(git_err)?;
            root.upsert(annotated.to_hex().into_bytes(), EntryKind::Blob, blob);
        }
    } else {
        let mut groups: BTreeMap<String, TreeEditor> = BTreeMap::new();
        for (annotated, bytes) in &notes {
            let hex = annotated.to_hex();
            let (prefix, suffix) = hex.split_at(2);
            let blob = repo.write_blob(bytes.clone()).map_err(git_err)?;
            groups
                .entry(prefix.to_string())
                .or_default()
                .upsert(suffix, EntryKind::Blob, blob);
        }
        for (prefix, subtree) in groups {
            root.upsert(
                prefix.into_bytes(),
                EntryKind::Tree,
                repo.write_tree(subtree).map_err(git_err)?,
            );
        }
    }
    let tree = repo.write_tree(root).map_err(git_err)?;
    let identity = b"Heddle <heddle@local> 0 +0000".to_vec();
    let commit = CommitObject {
        tree,
        parents: Vec::new(),
        author: identity.clone(),
        committer: identity,
        encoding: None,
        message: b"heddle: state metadata\n".to_vec(),
    };
    repo.write_raw_object(GitObjectType::Commit, commit.write())
        .map(Some)
        .map_err(git_err)
}

/// A V4 tree keeps private per-entry salts that Git cannot reconstruct. Keep
/// the source identity for lineage, but let Git import mint a state over the
/// reconstructed Git tree instead of claiming the embedded state is portable.
pub fn note_for_state(
    repo: &HeddleRepository,
    state: &State,
    parents_rewritten: bool,
) -> GitProjectionResult<HeddleNote> {
    let hosted_seed =
        objects::object::thread_replication::hosted_import::synthetic_initial_base()?.id();
    let omits_hosted_seed = state.parents.contains(&hosted_seed);
    let mut note = if parents_rewritten || omits_hosted_seed {
        HeddleNote::from_projected_state(state)
    } else {
        HeddleNote::from_state(state)
    };
    let tree = repo
        .store()
        .get_tree(&state.tree)?
        .ok_or_else(|| GitProjectionError::Git(format!("state tree {} is missing", state.tree)))?;
    if tree.scheme() == TreeScheme::V4Salted || omits_hosted_seed {
        note.source_state = None;
    }
    Ok(note)
}

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
