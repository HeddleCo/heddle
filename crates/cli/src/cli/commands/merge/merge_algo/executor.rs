// SPDX-License-Identifier: Apache-2.0
// Internal merge-driver helpers fan out across our/their/base trees +
// rename maps + conflict markers — each helper carries distinct
// semantic state and clippy's 7-arg default is conservative here.
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};

use anyhow::Result;
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

    if let Some(merged_content) =
        try_line_based_content_merge(store, base_hash, our_hash, their_hash)?
    {
        let blob = Blob::new(merged_content);
        return Ok(store.put_blob(&blob)?);
    }

    content_conflict_merge(store, our_hash, their_hash, path, conflicts, labels)
}

fn try_line_based_content_merge(
    store: &dyn ObjectStore,
    base_hash: &ContentHash,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
) -> Result<Option<Vec<u8>>> {
    let Some(base_content) = store
        .get_blob(base_hash)?
        .map(|blob| blob.content().to_vec())
    else {
        return Ok(None);
    };
    let Some(our_content) = store
        .get_blob(our_hash)?
        .map(|blob| blob.content().to_vec())
    else {
        return Ok(None);
    };
    let Some(their_content) = store
        .get_blob(their_hash)?
        .map(|blob| blob.content().to_vec())
    else {
        return Ok(None);
    };

    let Ok(base_text) = String::from_utf8(base_content) else {
        return Ok(None);
    };
    let Ok(our_text) = String::from_utf8(our_content) else {
        return Ok(None);
    };
    let Ok(their_text) = String::from_utf8(their_content) else {
        return Ok(None);
    };

    Ok(merge_single_line_ranges(&base_text, &our_text, &their_text).map(String::into_bytes))
}

#[derive(Debug)]
struct LineChange {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

fn merge_single_line_ranges(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_lines = split_preserving_line_endings(base);
    let our_change = changed_line_range(&base_lines, &split_preserving_line_endings(ours));
    let their_change = changed_line_range(&base_lines, &split_preserving_line_endings(theirs));

    if our_change.start == their_change.start
        && our_change.end == their_change.end
        && our_change.start == our_change.end
    {
        let mut merged = base_lines;
        merged.splice(
            our_change.start..our_change.end,
            our_change
                .replacement
                .into_iter()
                .chain(their_change.replacement),
        );
        return Some(merged.concat());
    }

    let (first, second) = if our_change.end <= their_change.start {
        (our_change, their_change)
    } else if their_change.end <= our_change.start {
        (their_change, our_change)
    } else {
        return None;
    };

    let mut merged = base_lines;
    merged.splice(second.start..second.end, second.replacement);
    merged.splice(first.start..first.end, first.replacement);
    Some(merged.concat())
}

fn changed_line_range(base: &[String], changed: &[String]) -> LineChange {
    let prefix_len = base
        .iter()
        .zip(changed.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let suffix_len = base[prefix_len..]
        .iter()
        .rev()
        .zip(changed[prefix_len..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let base_end = base.len() - suffix_len;
    let changed_end = changed.len() - suffix_len;

    LineChange {
        start: prefix_len,
        end: base_end,
        replacement: changed[prefix_len..changed_end].to_vec(),
    }
}

fn split_preserving_line_endings(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    text.split_inclusive('\n')
        .map(ToString::to_string)
        .collect()
}

fn content_conflict_merge(
    store: &dyn ObjectStore,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    let our_content = store
        .get_blob(our_hash)?
        .map(|blob| blob.content().to_vec())
        .unwrap_or_default();
    let their_content = store
        .get_blob(their_hash)?
        .map(|blob| blob.content().to_vec())
        .unwrap_or_default();
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
    let kept_content = store
        .get_blob(kept_hash)?
        .map(|blob| blob.content().to_vec())
        .unwrap_or_default();
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
    format!(
        "<<<<<<< {}\n{}=======\n{}>>>>>>> {}\n",
        labels.current, our_text, their_text, labels.incoming
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