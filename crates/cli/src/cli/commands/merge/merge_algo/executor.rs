// SPDX-License-Identifier: Apache-2.0
// Internal merge-driver helpers fan out across our/their/base trees +
// rename maps + conflict markers — each helper carries distinct
// semantic state and clippy's 7-arg default is conservative here.
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::ObjectStore,
};
use repo::Repository;

use crate::cli::commands::merge::{
    merge_algo::ConflictLabels, merge_renames::MergeRenameMap, rename_matcher::flatten_tree,
};

pub(super) fn merge_with_renames(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    rename_map: &MergeRenameMap,
    labels: ConflictLabels<'_>,
) -> Result<(Tree, Vec<String>)> {
    let store = repo.store();
    let base_flat = flatten_tree(store, base_tree, "")?;
    let our_flat = flatten_tree(store, our_tree, "")?;
    let their_flat = flatten_tree(store, their_tree, "")?;
    let mut merged_flat = HashMap::new();
    let mut conflicts = Vec::new();
    let mut claimed_paths = HashSet::new();

    apply_renames(
        store,
        &rename_map.our_renames,
        &rename_map.their_renames,
        &their_flat,
        &mut merged_flat,
        &mut conflicts,
        &mut claimed_paths,
        labels,
    )?;
    apply_renames(
        store,
        &rename_map.their_renames,
        &rename_map.our_renames,
        &our_flat,
        &mut merged_flat,
        &mut conflicts,
        &mut claimed_paths,
        labels,
    )?;

    merge_remaining_paths(
        store,
        &base_flat,
        &our_flat,
        &their_flat,
        &claimed_paths,
        &mut merged_flat,
        &mut conflicts,
        labels,
    )?;

    Ok((build_nested_tree(store, &merged_flat)?, conflicts))
}

pub(super) fn merge_without_renames(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    labels: ConflictLabels<'_>,
) -> Result<(Tree, Vec<String>)> {
    three_way_merge_recursive(repo, base_tree, our_tree, their_tree, "", labels)
}

fn apply_renames(
    store: &dyn ObjectStore,
    active_renames: &HashMap<String, crate::cli::commands::merge::rename_matcher::RenameMatch>,
    opposing_renames: &HashMap<String, crate::cli::commands::merge::rename_matcher::RenameMatch>,
    opposing_flat: &HashMap<String, (ContentHash, objects::object::EntryType)>,
    merged_flat: &mut HashMap<String, ContentHash>,
    conflicts: &mut Vec<String>,
    claimed_paths: &mut HashSet<String>,
    labels: ConflictLabels<'_>,
) -> Result<()> {
    for (base_path, rename) in active_renames {
        if let Some(opposing_rename) = opposing_renames.get(base_path)
            && rename.to_path != opposing_rename.to_path
        {
            conflicts.push(format!(
                "rename/rename conflict: {} → {} vs {}",
                base_path, rename.to_path, opposing_rename.to_path
            ));
            merged_flat.insert(rename.to_path.clone(), rename.to_hash);
            merged_flat.insert(opposing_rename.to_path.clone(), opposing_rename.to_hash);
            claimed_paths.insert(base_path.clone());
            claimed_paths.insert(rename.to_path.clone());
            claimed_paths.insert(opposing_rename.to_path.clone());
            continue;
        }

        claimed_paths.insert(base_path.clone());
        claimed_paths.insert(rename.to_path.clone());

        if let Some((opposing_hash, _)) = opposing_flat.get(base_path) {
            if opposing_hash != &rename.from_hash {
                let merged_hash = three_way_content_merge(
                    store,
                    &rename.from_hash,
                    &rename.to_hash,
                    opposing_hash,
                    &rename.to_path,
                    conflicts,
                    labels,
                )?;
                merged_flat.insert(rename.to_path.clone(), merged_hash);
            } else {
                merged_flat.insert(rename.to_path.clone(), rename.to_hash);
            }
            continue;
        }

        if !opposing_renames.contains_key(base_path) {
            conflicts.push(format!(
                "rename/delete conflict: {} renamed to {} but deleted on other side",
                base_path, rename.to_path
            ));
        }
        merged_flat.insert(rename.to_path.clone(), rename.to_hash);
    }

    Ok(())
}

fn merge_remaining_paths(
    store: &dyn ObjectStore,
    base_flat: &HashMap<String, (ContentHash, objects::object::EntryType)>,
    our_flat: &HashMap<String, (ContentHash, objects::object::EntryType)>,
    their_flat: &HashMap<String, (ContentHash, objects::object::EntryType)>,
    claimed_paths: &HashSet<String>,
    merged_flat: &mut HashMap<String, ContentHash>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<()> {
    let all_paths: HashSet<&String> = base_flat
        .keys()
        .chain(our_flat.keys())
        .chain(their_flat.keys())
        .collect();

    for path in all_paths {
        if claimed_paths.contains(path) {
            continue;
        }

        match (
            base_flat.get(path),
            our_flat.get(path),
            their_flat.get(path),
        ) {
            (None, None, Some((hash, _))) | (None, Some((hash, _)), None) => {
                merged_flat.insert(path.clone(), *hash);
            }
            (None, Some((our_hash, _)), Some((their_hash, _))) => {
                let merged_hash = if our_hash == their_hash {
                    *our_hash
                } else {
                    content_conflict_merge(store, our_hash, their_hash, path, conflicts, labels)?
                };
                merged_flat.insert(path.clone(), merged_hash);
            }
            (Some(_), None, None) | (None, None, None) => {}
            (Some((base_hash, _)), Some((our_hash, _)), None) => {
                if our_hash != base_hash {
                    let merged_hash =
                        modify_delete_conflict_merge(store, our_hash, path, conflicts, labels)?;
                    merged_flat.insert(path.clone(), merged_hash);
                }
            }
            (Some((base_hash, _)), None, Some((their_hash, _))) => {
                if their_hash != base_hash {
                    let merged_hash =
                        modify_delete_conflict_merge(store, their_hash, path, conflicts, labels)?;
                    merged_flat.insert(path.clone(), merged_hash);
                }
            }
            (Some((base_hash, _)), Some((our_hash, _)), Some((their_hash, _))) => {
                let merged_hash = if our_hash == their_hash {
                    *our_hash
                } else if our_hash == base_hash {
                    *their_hash
                } else if their_hash == base_hash {
                    *our_hash
                } else {
                    three_way_content_merge(
                        store, base_hash, our_hash, their_hash, path, conflicts, labels,
                    )?
                };
                merged_flat.insert(path.clone(), merged_hash);
            }
        }
    }

    Ok(())
}

fn three_way_content_merge(
    store: &dyn ObjectStore,
    base_hash: &ContentHash,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    if our_hash == their_hash {
        return Ok(*our_hash);
    }
    if our_hash == base_hash {
        return Ok(*their_hash);
    }
    if their_hash == base_hash {
        return Ok(*our_hash);
    }

    text_hunk_merge_blobs(store, base_hash, our_hash, their_hash, path, conflicts, labels)
}

fn text_hunk_merge_blobs(
    store: &dyn ObjectStore,
    base_hash: &ContentHash,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    use merge::{ConflictMarkers, MergeOutcome, text_hunk_merge_with_markers};

    let base_content = load_blob_content(store, base_hash, path)?;
    let our_content = load_blob_content(store, our_hash, path)?;
    let their_content = load_blob_content(store, their_hash, path)?;

    let markers = ConflictMarkers {
        ours: labels.current,
        theirs: labels.incoming,
    };
    match text_hunk_merge_with_markers(&base_content, &our_content, &their_content, markers) {
        MergeOutcome::Clean(bytes) => {
            let blob = Blob::new(bytes);
            Ok(store.put_blob(&blob)?)
        }
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => {
            let blob = Blob::new(merged_bytes_with_markers);
            let hash = store.put_blob(&blob)?;
            conflicts.push(path.to_string());
            Ok(hash)
        }
        // Binary inputs: fall back to whole-file conflict markers, matching
        // git's `binary file changed in both` shape. DeleteVsModify is
        // never produced by text_hunk_merge (its signature has all three
        // inputs present; deletion is detected at the tree layer).
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => {
            content_conflict_merge(store, our_hash, their_hash, path, conflicts, labels)
        }
    }
}

/// Load a blob's bytes, surfacing an error if it's missing from the store.
///
/// A missing blob during a three-way merge means the object store cannot
/// satisfy a hash the tree layer believes exists — corrupt store, dropped
/// pack, broken ref. Coercing the missing input to empty (`unwrap_or_default`)
/// causes the merger to silently produce a result where the other side
/// "wins" with no markers, committing a merge that loses data. Bail loudly
/// so the user sees the corruption instead of inheriting a silent rewrite.
fn load_blob_content(
    store: &dyn ObjectStore,
    hash: &ContentHash,
    path: &str,
) -> Result<Vec<u8>> {
    let blob = store.get_blob(hash)?.ok_or_else(|| {
        anyhow!(
            "merge input blob {hash} for path {path:?} is missing from the object store; \
             aborting to avoid silently merging against empty content"
        )
    })?;
    Ok(blob.content().to_vec())
}

fn content_conflict_merge(
    store: &dyn ObjectStore,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    let our_content = load_blob_content(store, our_hash, path)?;
    let their_content = load_blob_content(store, their_hash, path)?;
    let blob = Blob::new(format_conflict_content(
        &our_content,
        &their_content,
        labels,
    ));
    let hash = store.put_blob(&blob)?;
    conflicts.push(path.to_string());
    Ok(hash)
}

fn modify_delete_conflict_merge(
    store: &dyn ObjectStore,
    kept_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    let kept_content = load_blob_content(store, kept_hash, path)?;
    let blob = Blob::new(format_conflict_content(&kept_content, &[], labels));
    let hash = store.put_blob(&blob)?;
    conflicts.push(path.to_string());
    Ok(hash)
}

fn format_conflict_content(
    our_content: &[u8],
    their_content: &[u8],
    labels: ConflictLabels<'_>,
) -> Vec<u8> {
    let our_text = String::from_utf8_lossy(our_content);
    let their_text = String::from_utf8_lossy(their_content);
    // Conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) MUST start at
    // column 0 — git diff/mergetool, IDE conflict resolvers, and the
    // hunk-level merge engine all parse line-anchored. If a side's
    // content lacks a trailing newline, inject one so the following
    // marker doesn't get glued onto the last content line.
    let our_sep = if our_text.is_empty() || our_text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let their_sep = if their_text.is_empty() || their_text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!(
        "<<<<<<< {}\n{}{}=======\n{}{}>>>>>>> {}\n",
        labels.current, our_text, our_sep, their_text, their_sep, labels.incoming
    )
    .into_bytes()
}

fn build_nested_tree(store: &dyn ObjectStore, flat: &HashMap<String, ContentHash>) -> Result<Tree> {
    let mut top_files = Vec::new();
    let mut subdirs: HashMap<String, HashMap<String, ContentHash>> = HashMap::new();

    for (path, hash) in flat {
        if let Some((directory, rest)) = path.split_once('/') {
            subdirs
                .entry(directory.to_string())
                .or_default()
                .insert(rest.to_string(), *hash);
        } else {
            top_files.push((path.clone(), *hash));
        }
    }

    let mut entries = Vec::new();
    for (name, hash) in top_files {
        entries.push(TreeEntry::file(name, hash, false)?);
    }
    for (directory, sub_flat) in subdirs {
        let subtree = build_nested_tree(store, &sub_flat)?;
        let hash = store.put_tree(&subtree)?;
        entries.push(TreeEntry::directory(directory, hash)?);
    }

    Ok(Tree::from_entries(entries))
}

fn three_way_merge_recursive(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    prefix: &str,
    labels: ConflictLabels<'_>,
) -> Result<(Tree, Vec<String>)> {
    let base_entries: HashMap<&str, &TreeEntry> = base_tree
        .entries()
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let our_entries: HashMap<&str, &TreeEntry> = our_tree
        .entries()
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let their_entries: HashMap<&str, &TreeEntry> = their_tree
        .entries()
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let mut merged_entries = Vec::new();
    let mut conflicts = Vec::new();

    let all_names: HashSet<&str> = base_entries
        .keys()
        .chain(our_entries.keys())
        .chain(their_entries.keys())
        .copied()
        .collect();

    for name in all_names {
        let conflict_path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match (
            base_entries.get(name),
            our_entries.get(name),
            their_entries.get(name),
        ) {
            (None, None, Some(their)) => merged_entries.push((*their).clone()),
            (None, Some(our), None) => merged_entries.push((*our).clone()),
            (Some(base), None, Some(their)) => merge_delete_changed_entry(
                repo,
                base,
                their,
                &conflict_path,
                &mut merged_entries,
                &mut conflicts,
                labels,
            )?,
            (Some(base), Some(our), None) => merge_delete_changed_entry(
                repo,
                base,
                our,
                &conflict_path,
                &mut merged_entries,
                &mut conflicts,
                labels,
            )?,
            (None, Some(our), Some(their)) => {
                merge_added_entries(
                    repo,
                    our,
                    their,
                    &conflict_path,
                    &mut merged_entries,
                    &mut conflicts,
                    labels,
                )?;
            }
            (Some(base), Some(our), Some(their)) => {
                merge_changed_entries(
                    repo,
                    base,
                    our,
                    their,
                    &conflict_path,
                    &mut merged_entries,
                    &mut conflicts,
                    labels,
                )?;
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    Ok((Tree::from_entries(merged_entries), conflicts))
}

fn merge_delete_changed_entry(
    repo: &Repository,
    base_entry: &&TreeEntry,
    kept_entry: &&TreeEntry,
    conflict_path: &str,
    merged_entries: &mut Vec<TreeEntry>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<()> {
    if kept_entry == base_entry {
        return Ok(());
    }

    let kept_content = conflict_entry_content(repo, kept_entry)?;
    let blob = Blob::new(format_conflict_content(&kept_content, &[], labels));
    let hash = repo.store().put_blob(&blob)?;
    merged_entries.push(TreeEntry::file(kept_entry.name.clone(), hash, false)?);
    conflicts.push(conflict_path.to_string());
    Ok(())
}

fn merge_added_entries(
    repo: &Repository,
    our_entry: &&TreeEntry,
    their_entry: &&TreeEntry,
    conflict_path: &str,
    merged_entries: &mut Vec<TreeEntry>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<()> {
    if our_entry == their_entry {
        merged_entries.push((**our_entry).clone());
    } else if our_entry.is_tree() && their_entry.is_tree() {
        let (entry, sub_conflicts) = merge_subtrees(
            repo,
            &Tree::new(),
            our_entry,
            their_entry,
            conflict_path,
            labels,
        )?;
        merged_entries.push(entry);
        conflicts.extend(sub_conflicts);
    } else {
        let conflict_content = generate_conflict_content(repo, our_entry, their_entry, labels)?;
        let blob = Blob::new(conflict_content);
        let hash = repo.store().put_blob(&blob)?;
        merged_entries.push(TreeEntry::file(our_entry.name.clone(), hash, false)?);
        conflicts.push(conflict_path.to_string());
    }

    Ok(())
}

fn merge_changed_entries(
    repo: &Repository,
    base_entry: &&TreeEntry,
    our_entry: &&TreeEntry,
    their_entry: &&TreeEntry,
    conflict_path: &str,
    merged_entries: &mut Vec<TreeEntry>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<()> {
    if our_entry == their_entry {
        merged_entries.push((**our_entry).clone());
    } else if our_entry == base_entry {
        merged_entries.push((**their_entry).clone());
    } else if their_entry == base_entry {
        merged_entries.push((**our_entry).clone());
    } else if let Some(entry) =
        merge_mode_content_orthogonal_change(base_entry, our_entry, their_entry)
    {
        merged_entries.push(entry);
    } else if our_entry.is_tree() && their_entry.is_tree() {
        let base_subtree = if base_entry.is_tree() {
            repo.store().get_tree(&base_entry.hash)?.unwrap_or_default()
        } else {
            Tree::new()
        };
        let (entry, sub_conflicts) = merge_subtrees(
            repo,
            &base_subtree,
            our_entry,
            their_entry,
            conflict_path,
            labels,
        )?;
        merged_entries.push(entry);
        conflicts.extend(sub_conflicts);
    } else if base_entry.is_blob() && our_entry.is_blob() && their_entry.is_blob() {
        let merged_hash = three_way_content_merge(
            repo.store(),
            &base_entry.hash,
            &our_entry.hash,
            &their_entry.hash,
            conflict_path,
            conflicts,
            labels,
        )?;
        merged_entries.push(TreeEntry::file(
            our_entry.name.clone(),
            merged_hash,
            our_entry.is_executable(),
        )?);
    } else {
        let conflict_content = generate_conflict_content(repo, our_entry, their_entry, labels)?;
        let blob = Blob::new(conflict_content);
        let hash = repo.store().put_blob(&blob)?;
        merged_entries.push(TreeEntry::file(our_entry.name.clone(), hash, false)?);
        conflicts.push(conflict_path.to_string());
    }

    Ok(())
}

fn merge_mode_content_orthogonal_change(
    base_entry: &TreeEntry,
    our_entry: &TreeEntry,
    their_entry: &TreeEntry,
) -> Option<TreeEntry> {
    if !base_entry.is_blob() || !our_entry.is_blob() || !their_entry.is_blob() {
        return None;
    }
    if our_entry.name != their_entry.name || our_entry.name != base_entry.name {
        return None;
    }

    let our_content_unchanged = our_entry.hash == base_entry.hash;
    let their_content_unchanged = their_entry.hash == base_entry.hash;
    let our_mode_unchanged = our_entry.mode == base_entry.mode;
    let their_mode_unchanged = their_entry.mode == base_entry.mode;

    if our_content_unchanged && their_mode_unchanged {
        let mut entry = (*their_entry).clone();
        entry.mode = our_entry.mode;
        Some(entry)
    } else if their_content_unchanged && our_mode_unchanged {
        let mut entry = (*our_entry).clone();
        entry.mode = their_entry.mode;
        Some(entry)
    } else {
        None
    }
}

fn merge_subtrees(
    repo: &Repository,
    base_subtree: &Tree,
    our_entry: &TreeEntry,
    their_entry: &TreeEntry,
    prefix: &str,
    labels: ConflictLabels<'_>,
) -> Result<(TreeEntry, Vec<String>)> {
    let our_subtree = repo.store().get_tree(&our_entry.hash)?.unwrap_or_default();
    let their_subtree = repo
        .store()
        .get_tree(&their_entry.hash)?
        .unwrap_or_default();
    let (merged_subtree, conflicts) = three_way_merge_recursive(
        repo,
        base_subtree,
        &our_subtree,
        &their_subtree,
        prefix,
        labels,
    )?;
    let merged_hash = repo.store().put_tree(&merged_subtree)?;
    Ok((
        TreeEntry::directory(our_entry.name.clone(), merged_hash)?,
        conflicts,
    ))
}

fn generate_conflict_content(
    repo: &Repository,
    our_entry: &TreeEntry,
    their_entry: &TreeEntry,
    labels: ConflictLabels<'_>,
) -> Result<Vec<u8>> {
    let our_content = conflict_entry_content(repo, our_entry)?;
    let their_content = conflict_entry_content(repo, their_entry)?;
    Ok(format_conflict_content(
        &our_content,
        &their_content,
        labels,
    ))
}

fn conflict_entry_content(repo: &Repository, entry: &TreeEntry) -> Result<Vec<u8>> {
    if entry.is_tree() {
        let tree = repo.store().get_tree(&entry.hash)?.unwrap_or_default();
        let mut names: Vec<&str> = tree
            .entries()
            .iter()
            .map(|child| child.name.as_str())
            .collect();
        names.sort_unstable();
        let listing = if names.is_empty() {
            String::from("<empty directory>")
        } else {
            format!("<directory>\n{}", names.join("\n"))
        };
        Ok(listing.into_bytes())
    } else {
        Ok(repo.require_blob(&entry.hash)?.content().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objects::store::InMemoryStore;

    /// A missing blob during the three-way text merge must surface as an
    /// error, not be silently coerced to empty bytes. The pre-fix code
    /// `unwrap_or_default()`-ed the load and would produce a "clean" merge
    /// where the present side won and the missing side appeared as deletion
    /// — committing data loss without conflict markers.
    #[test]
    fn missing_merge_blob_surfaces_as_error_not_silent_empty() {
        let store = InMemoryStore::new();
        let present = Blob::new(b"X\nY\n".to_vec());
        let present_hash = store.put_blob(&present).unwrap();
        // Hash not present in the store — simulates a corrupt object store
        // or dropped pack.
        let missing = Blob::new(b"zzz".to_vec());
        let missing_hash = missing.hash();
        assert_ne!(present_hash, missing_hash);

        let err = load_blob_content(&store, &missing_hash, "src/foo.rs")
            .expect_err("expected a hard error on missing merge input");
        let msg = format!("{err}");
        assert!(
            msg.contains("missing from the object store"),
            "error must surface the missing-blob diagnostic, got: {msg}"
        );
        assert!(
            msg.contains("src/foo.rs"),
            "error must mention the affected path so operators can locate \
             the corrupt entry; got: {msg}"
        );
    }

    #[test]
    fn present_merge_blob_loads_content() {
        let store = InMemoryStore::new();
        let blob = Blob::new(b"hello\nworld\n".to_vec());
        let hash = store.put_blob(&blob).unwrap();
        let content = load_blob_content(&store, &hash, "src/bar.rs")
            .expect("blob present in store");
        assert_eq!(content, b"hello\nworld\n");
    }
}