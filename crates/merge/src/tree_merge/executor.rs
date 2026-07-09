// SPDX-License-Identifier: Apache-2.0
// Internal merge-driver helpers fan out across our/their/base trees +
// rename maps + conflict markers — each helper carries distinct
// semantic state and clippy's 7-arg default is conservative here.
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use objects::{
    object::{Blob, ContentHash, EntryType, Tree, TreeEntry},
    store::ObjectStore,
    util::gitlink_placeholder_bytes,
};

use super::{
    ConflictLabels, MergeBlobSource, MergeError, RenameMatch, SemanticMergeFn,
    rename_matcher::{FlatLeaf, FlatTree, flatten_tree},
    renames::MergeRenameMap,
};

/// Executable-bit policy for content merges and conflict markers:
/// preserve +x when either side is executable (union). Prefer keeping the
/// bit over inventing `100644` when both sides carried executable mode.
fn union_executable(left: bool, right: bool) -> bool {
    left || right
}

fn leaf_from_rename(rename: &RenameMatch) -> FlatLeaf {
    FlatLeaf {
        hash: rename.to_hash,
        entry_type: rename.to_entry_type,
        executable: rename.to_executable,
    }
}

fn leaf_to_tree_entry(name: impl Into<String>, leaf: &FlatLeaf) -> Result<TreeEntry> {
    let name = name.into();
    match leaf.entry_type {
        EntryType::Blob => Ok(TreeEntry::file(name, leaf.hash, leaf.executable)?),
        EntryType::Symlink => Ok(TreeEntry::symlink(name, leaf.hash)?),
        EntryType::Tree | EntryType::Gitlink | EntryType::Spoollink => Err(anyhow!(
            "flat merge leaf {name:?} has non-leaf entry type {:?}",
            leaf.entry_type
        )),
    }
}

/// Resolve mode/kind after content is chosen for a two-sided leaf merge.
///
/// Content-merge / conflict-marker output is always blob bytes. Keep
/// symlink kind only when both inputs were symlinks and content was not
/// rewritten. Executable uses the union policy for blob results.
fn resolve_merged_leaf(
    hash: ContentHash,
    our: &FlatLeaf,
    their: &FlatLeaf,
    content_was_merged: bool,
) -> FlatLeaf {
    if !content_was_merged
        && our.entry_type == their.entry_type
        && our.entry_type == EntryType::Symlink
    {
        return FlatLeaf::symlink(hash);
    }
    if !content_was_merged && our.entry_type == their.entry_type && our.entry_type == EntryType::Blob
    {
        return FlatLeaf::blob(hash, union_executable(our.executable, their.executable));
    }
    FlatLeaf::blob(
        hash,
        union_executable(
            our.entry_type == EntryType::Blob && our.executable,
            their.entry_type == EntryType::Blob && their.executable,
        ),
    )
}

pub(super) fn merge_with_renames(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    rename_map: &MergeRenameMap,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<(Tree, Vec<String>)> {
    let base_flat = flatten_tree(store, base_tree, "")?;
    let our_flat = flatten_tree(store, our_tree, "")?;
    let their_flat = flatten_tree(store, their_tree, "")?;
    let mut merged_flat = HashMap::new();
    let mut conflicts = Vec::new();
    let mut claimed_paths = HashSet::new();

    apply_renames(
        store,
        blob_source,
        &rename_map.our_renames,
        &rename_map.their_renames,
        &their_flat,
        &mut merged_flat,
        &mut conflicts,
        &mut claimed_paths,
        labels,
        semantic_merge,
    )?;
    apply_renames(
        store,
        blob_source,
        &rename_map.their_renames,
        &rename_map.our_renames,
        &our_flat,
        &mut merged_flat,
        &mut conflicts,
        &mut claimed_paths,
        labels,
        semantic_merge,
    )?;

    merge_remaining_paths(
        store,
        blob_source,
        &base_flat,
        &our_flat,
        &their_flat,
        &claimed_paths,
        &mut merged_flat,
        &mut conflicts,
        labels,
        semantic_merge,
    )?;

    Ok((build_nested_tree(store, &merged_flat)?, conflicts))
}

pub(super) fn merge_without_renames(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<(Tree, Vec<String>)> {
    three_way_merge_recursive(
        store,
        blob_source,
        base_tree,
        our_tree,
        their_tree,
        "",
        labels,
        semantic_merge,
    )
}

fn apply_renames(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    active_renames: &HashMap<String, RenameMatch>,
    opposing_renames: &HashMap<String, RenameMatch>,
    opposing_flat: &FlatTree,
    merged_flat: &mut HashMap<String, FlatLeaf>,
    conflicts: &mut Vec<String>,
    claimed_paths: &mut HashSet<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<()> {
    for (base_path, rename) in active_renames {
        if let Some(opposing_rename) = opposing_renames.get(base_path)
            && rename.to_path != opposing_rename.to_path
        {
            conflicts.push(format!(
                "rename/rename conflict: {} → {} vs {}",
                base_path, rename.to_path, opposing_rename.to_path
            ));
            merged_flat.insert(rename.to_path.clone(), leaf_from_rename(rename));
            merged_flat.insert(
                opposing_rename.to_path.clone(),
                leaf_from_rename(opposing_rename),
            );
            claimed_paths.insert(base_path.clone());
            claimed_paths.insert(rename.to_path.clone());
            claimed_paths.insert(opposing_rename.to_path.clone());
            continue;
        }

        claimed_paths.insert(base_path.clone());
        claimed_paths.insert(rename.to_path.clone());

        if let Some(opposing) = opposing_flat.get(base_path) {
            if opposing.hash != rename.from_hash {
                let merged_hash = three_way_content_merge(
                    store,
                    blob_source,
                    &rename.from_hash,
                    &rename.to_hash,
                    &opposing.hash,
                    &rename.to_path,
                    conflicts,
                    labels,
                    semantic_merge,
                )?;
                let active_leaf = leaf_from_rename(rename);
                merged_flat.insert(
                    rename.to_path.clone(),
                    resolve_merged_leaf(merged_hash, &active_leaf, opposing, true),
                );
            } else {
                // Opposing side kept base content; take rename destination
                // and union executable with opposing (usually base mode).
                let mut leaf = leaf_from_rename(rename);
                if leaf.entry_type == EntryType::Blob {
                    leaf.executable = union_executable(leaf.executable, opposing.executable);
                }
                merged_flat.insert(rename.to_path.clone(), leaf);
            }
            continue;
        }

        if !opposing_renames.contains_key(base_path) {
            conflicts.push(format!(
                "rename/delete conflict: {} renamed to {} but deleted on other side",
                base_path, rename.to_path
            ));
        }
        merged_flat.insert(rename.to_path.clone(), leaf_from_rename(rename));
    }

    Ok(())
}

fn merge_remaining_paths(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_flat: &FlatTree,
    our_flat: &FlatTree,
    their_flat: &FlatTree,
    claimed_paths: &HashSet<String>,
    merged_flat: &mut HashMap<String, FlatLeaf>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
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

        match (base_flat.get(path), our_flat.get(path), their_flat.get(path)) {
            (None, None, Some(leaf)) | (None, Some(leaf), None) => {
                merged_flat.insert(path.clone(), *leaf);
            }
            (None, Some(our), Some(their)) => {
                if our.hash == their.hash && our.entry_type == their.entry_type {
                    merged_flat.insert(
                        path.clone(),
                        FlatLeaf {
                            hash: our.hash,
                            entry_type: our.entry_type,
                            executable: union_executable(our.executable, their.executable),
                        },
                    );
                } else {
                    let merged_hash = content_conflict_merge(
                        store,
                        blob_source,
                        &our.hash,
                        &their.hash,
                        path,
                        conflicts,
                        labels,
                    )?;
                    merged_flat.insert(
                        path.clone(),
                        resolve_merged_leaf(merged_hash, our, their, true),
                    );
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
            (Some(base), Some(our), None) => {
                if our.hash != base.hash
                    || our.entry_type != base.entry_type
                    || our.executable != base.executable
                {
                    let merged_hash = modify_delete_conflict_merge(
                        store,
                        blob_source,
                        &our.hash,
                        path,
                        conflicts,
                        labels,
                    )?;
                    merged_flat.insert(
                        path.clone(),
                        FlatLeaf {
                            hash: merged_hash,
                            entry_type: EntryType::Blob,
                            executable: our.entry_type == EntryType::Blob && our.executable,
                        },
                    );
                }
            }
            (Some(base), None, Some(their)) => {
                if their.hash != base.hash
                    || their.entry_type != base.entry_type
                    || their.executable != base.executable
                {
                    let merged_hash = modify_delete_conflict_merge(
                        store,
                        blob_source,
                        &their.hash,
                        path,
                        conflicts,
                        labels,
                    )?;
                    merged_flat.insert(
                        path.clone(),
                        FlatLeaf {
                            hash: merged_hash,
                            entry_type: EntryType::Blob,
                            executable: their.entry_type == EntryType::Blob && their.executable,
                        },
                    );
                }
            }
            (Some(base), Some(our), Some(their)) => {
                let (merged_hash, content_was_merged) = if our.hash == their.hash {
                    (our.hash, false)
                } else if our.hash == base.hash {
                    (their.hash, false)
                } else if their.hash == base.hash {
                    (our.hash, false)
                } else {
                    (
                        three_way_content_merge(
                            store,
                            blob_source,
                            &base.hash,
                            &our.hash,
                            &their.hash,
                            path,
                            conflicts,
                            labels,
                            semantic_merge,
                        )?,
                        true,
                    )
                };
                let leaf = if !content_was_merged
                    && our.hash == their.hash
                    && our.entry_type == their.entry_type
                {
                    FlatLeaf {
                        hash: merged_hash,
                        entry_type: our.entry_type,
                        executable: union_executable(our.executable, their.executable),
                    }
                } else if !content_was_merged && our.hash == base.hash {
                    FlatLeaf {
                        hash: merged_hash,
                        entry_type: their.entry_type,
                        executable: if their.entry_type == EntryType::Blob {
                            union_executable(our.executable, their.executable)
                        } else {
                            false
                        },
                    }
                } else if !content_was_merged && their.hash == base.hash {
                    FlatLeaf {
                        hash: merged_hash,
                        entry_type: our.entry_type,
                        executable: if our.entry_type == EntryType::Blob {
                            union_executable(our.executable, their.executable)
                        } else {
                            false
                        },
                    }
                } else {
                    resolve_merged_leaf(merged_hash, our, their, content_was_merged)
                };
                merged_flat.insert(path.clone(), leaf);
            }
        }
    }

    Ok(())
}

fn three_way_content_merge(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_hash: &ContentHash,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
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

    text_hunk_merge_blobs(
        store,
        blob_source,
        base_hash,
        our_hash,
        their_hash,
        path,
        conflicts,
        labels,
        semantic_merge,
    )
}

fn text_hunk_merge_blobs(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_hash: &ContentHash,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<ContentHash> {
    use crate::{ConflictMarkers, MergeOutcome, MergeStrategy, text_hunk_merge_with_markers};

    let base_content = load_blob_content(blob_source, base_hash, path)?;
    let our_content = load_blob_content(blob_source, our_hash, path)?;
    let their_content = load_blob_content(blob_source, their_hash, path)?;

    let markers = ConflictMarkers {
        ours: labels.current,
        theirs: labels.incoming,
    };
    // Route based on the caller's chosen merge strategy. `Semantic`
    // invokes the optional caller-supplied driver, which itself should
    // fall back to `text_hunk_merge` on unparseable / unknown-language
    // files. `HunkOnly` preserves the historical path verbatim.
    let outcome = match labels.strategy {
        MergeStrategy::Semantic => semantic_merge
            .map(|merge| {
                merge(
                    &base_content,
                    &our_content,
                    &their_content,
                    std::path::Path::new(path),
                    markers,
                )
            })
            .unwrap_or_else(|| {
                text_hunk_merge_with_markers(&base_content, &our_content, &their_content, markers)
            }),
        MergeStrategy::HunkOnly => {
            text_hunk_merge_with_markers(&base_content, &our_content, &their_content, markers)
        }
    };
    match outcome {
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
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => content_conflict_merge(
            store,
            blob_source,
            our_hash,
            their_hash,
            path,
            conflicts,
            labels,
        ),
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
    blob_source: &impl MergeBlobSource,
    hash: &ContentHash,
    path: &str,
) -> Result<Vec<u8>> {
    blob_source.load_blob(hash, path)
}

/// Load a tree, surfacing an error if it's missing from the store.
///
/// Same hazard shape as [`load_blob_content`]: a hash that the tree layer
/// records as an entry MUST resolve to an object in the store. The
/// pre-#90 code used `get_tree(...)?.unwrap_or_default()` which silently
/// substituted an empty `Tree`, so the recursive merger treated the
/// subtree as "deleted on both sides" and the user committed a merge
/// that erased every file under that path with no conflict markers.
///
/// `label` is folded into the error message so the operator can locate
/// the corrupt entry — typically the recursive-merge path or the
/// merge-side entry name.
fn require_subtree(store: &impl ObjectStore, hash: &ContentHash, label: &str) -> Result<Tree> {
    store.get_tree(hash)?.ok_or_else(|| {
        anyhow!(merge_integrity_refusal(
            "merge input subtree {hash} for {label} is missing from the object store; \
             aborting to avoid silently merging against an empty subtree",
            format!("merge input {label} references missing subtree {hash} in the object store"),
            "the recursive merge would use an empty subtree and could silently delete every tracked file below that path",
            "HEAD, refs, and worktree were left unchanged; any merge scratch objects written before this refusal are unreachable until a successful capture",
        ))
    })
}

fn merge_integrity_refusal(
    summary: impl Into<String>,
    unsafe_condition: impl AsRef<str>,
    would_change: impl AsRef<str>,
    preserved: impl AsRef<str>,
) -> MergeError {
    MergeError::repository_integrity_refusal(
        summary,
        unsafe_condition.as_ref(),
        would_change.as_ref(),
        preserved.as_ref(),
    )
}

fn content_conflict_merge(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    our_hash: &ContentHash,
    their_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    let our_content = load_blob_content(blob_source, our_hash, path)?;
    let their_content = load_blob_content(blob_source, their_hash, path)?;
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
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    kept_hash: &ContentHash,
    path: &str,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
) -> Result<ContentHash> {
    let kept_content = load_blob_content(blob_source, kept_hash, path)?;
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

fn build_nested_tree(store: &impl ObjectStore, flat: &HashMap<String, FlatLeaf>) -> Result<Tree> {
    let mut top_files = Vec::new();
    let mut subdirs: HashMap<String, HashMap<String, FlatLeaf>> = HashMap::new();

    for (path, leaf) in flat {
        if let Some((directory, rest)) = path.split_once('/') {
            subdirs
                .entry(directory.to_string())
                .or_default()
                .insert(rest.to_string(), *leaf);
        } else {
            top_files.push((path.clone(), *leaf));
        }
    }

    let mut entries = Vec::new();
    for (name, leaf) in top_files {
        entries.push(leaf_to_tree_entry(name, &leaf)?);
    }
    for (directory, sub_flat) in subdirs {
        let subtree = build_nested_tree(store, &sub_flat)?;
        let hash = store.put_tree(&subtree)?;
        entries.push(TreeEntry::directory(directory, hash)?);
    }

    Ok(Tree::from_entries(entries))
}

fn three_way_merge_recursive(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    prefix: &str,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<(Tree, Vec<String>)> {
    let base_entries: HashMap<&str, &TreeEntry> = base_tree
        .entries()
        .iter()
        .map(|entry| (entry.name(), entry))
        .collect();
    let our_entries: HashMap<&str, &TreeEntry> = our_tree
        .entries()
        .iter()
        .map(|entry| (entry.name(), entry))
        .collect();
    let their_entries: HashMap<&str, &TreeEntry> = their_tree
        .entries()
        .iter()
        .map(|entry| (entry.name(), entry))
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
                store,
                blob_source,
                base,
                their,
                &conflict_path,
                &mut merged_entries,
                &mut conflicts,
                labels,
            )?,
            (Some(base), Some(our), None) => merge_delete_changed_entry(
                store,
                blob_source,
                base,
                our,
                &conflict_path,
                &mut merged_entries,
                &mut conflicts,
                labels,
            )?,
            (None, Some(our), Some(their)) => {
                merge_added_entries(
                    store,
                    blob_source,
                    our,
                    their,
                    &conflict_path,
                    &mut merged_entries,
                    &mut conflicts,
                    labels,
                    semantic_merge,
                )?;
            }
            (Some(base), Some(our), Some(their)) => {
                merge_changed_entries(
                    store,
                    blob_source,
                    base,
                    our,
                    their,
                    &conflict_path,
                    &mut merged_entries,
                    &mut conflicts,
                    labels,
                    semantic_merge,
                )?;
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    Ok((Tree::from_entries(merged_entries), conflicts))
}

fn merge_delete_changed_entry(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
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

    let kept_content = conflict_entry_content(store, blob_source, kept_entry)?;
    let blob = Blob::new(format_conflict_content(&kept_content, &[], labels));
    let hash = store.put_blob(&blob)?;
    // Conflict marker content is always a blob, but preserve +x when the
    // kept side was executable so materialize does not silently drop mode.
    merged_entries.push(TreeEntry::file(
        kept_entry.name().to_string(),
        hash,
        kept_entry.is_blob() && kept_entry.is_executable(),
    )?);
    conflicts.push(conflict_path.to_string());
    Ok(())
}

fn merge_added_entries(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    our_entry: &&TreeEntry,
    their_entry: &&TreeEntry,
    conflict_path: &str,
    merged_entries: &mut Vec<TreeEntry>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<()> {
    if our_entry == their_entry {
        merged_entries.push((**our_entry).clone());
    } else if our_entry.is_tree() && their_entry.is_tree() {
        let (entry, sub_conflicts) = merge_subtrees(
            store,
            blob_source,
            &Tree::new(),
            our_entry,
            their_entry,
            conflict_path,
            labels,
            semantic_merge,
        )?;
        merged_entries.push(entry);
        conflicts.extend(sub_conflicts);
    } else {
        let conflict_content =
            generate_conflict_content(store, blob_source, our_entry, their_entry, labels)?;
        let blob = Blob::new(conflict_content);
        let hash = store.put_blob(&blob)?;
        merged_entries.push(TreeEntry::file(
            our_entry.name().to_string(),
            hash,
            union_executable(
                our_entry.is_blob() && our_entry.is_executable(),
                their_entry.is_blob() && their_entry.is_executable(),
            ),
        )?);
        conflicts.push(conflict_path.to_string());
    }

    Ok(())
}

fn merge_changed_entries(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_entry: &&TreeEntry,
    our_entry: &&TreeEntry,
    their_entry: &&TreeEntry,
    conflict_path: &str,
    merged_entries: &mut Vec<TreeEntry>,
    conflicts: &mut Vec<String>,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
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
            let Some(hash) = base_entry.tree_hash() else {
                return Err(anyhow!(merge_integrity_refusal(
                    format!("merge base entry at {conflict_path:?} has tree type but no tree hash"),
                    format!(
                        "merge base path {conflict_path:?} records a tree entry without a tree object hash"
                    ),
                    "the recursive merge cannot load a trustworthy base subtree and could silently erase or mis-merge descendants",
                    "HEAD, refs, and worktree were left unchanged; merge stopped before applying the malformed subtree",
                )));
            };
            require_subtree(store, &hash, &format!("base subtree at {conflict_path:?}"))?
        } else {
            Tree::new()
        };
        let (entry, sub_conflicts) = merge_subtrees(
            store,
            blob_source,
            &base_subtree,
            our_entry,
            their_entry,
            conflict_path,
            labels,
            semantic_merge,
        )?;
        merged_entries.push(entry);
        conflicts.extend(sub_conflicts);
    } else if base_entry.is_blob() && our_entry.is_blob() && their_entry.is_blob() {
        let (Some(base_hash), Some(our_hash), Some(their_hash)) = (
            base_entry.blob_hash(),
            our_entry.blob_hash(),
            their_entry.blob_hash(),
        ) else {
            return Err(anyhow!(merge_integrity_refusal(
                format!("blob merge entry at {conflict_path:?} did not carry blob hashes"),
                format!(
                    "merge path {conflict_path:?} records blob entries without all required blob object hashes"
                ),
                "the content merge cannot load all three inputs and could otherwise merge against empty bytes",
                "HEAD, refs, and worktree were left unchanged; merge stopped before applying the malformed blob entries",
            )));
        };
        let merged_hash = three_way_content_merge(
            store,
            blob_source,
            &base_hash,
            &our_hash,
            &their_hash,
            conflict_path,
            conflicts,
            labels,
            semantic_merge,
        )?;
        merged_entries.push(TreeEntry::file(
            our_entry.name().to_string(),
            merged_hash,
            union_executable(our_entry.is_executable(), their_entry.is_executable()),
        )?);
    } else {
        let conflict_content =
            generate_conflict_content(store, blob_source, our_entry, their_entry, labels)?;
        let blob = Blob::new(conflict_content);
        let hash = store.put_blob(&blob)?;
        merged_entries.push(TreeEntry::file(
            our_entry.name().to_string(),
            hash,
            union_executable(
                our_entry.is_blob() && our_entry.is_executable(),
                their_entry.is_blob() && their_entry.is_executable(),
            ),
        )?);
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
    if our_entry.name() != their_entry.name() || our_entry.name() != base_entry.name() {
        return None;
    }

    let our_content_unchanged = our_entry.blob_hash() == base_entry.blob_hash();
    let their_content_unchanged = their_entry.blob_hash() == base_entry.blob_hash();
    let our_mode_unchanged = our_entry.mode() == base_entry.mode();
    let their_mode_unchanged = their_entry.mode() == base_entry.mode();

    if our_content_unchanged && their_mode_unchanged {
        their_entry.with_mode(our_entry.mode()).ok()
    } else if their_content_unchanged && our_mode_unchanged {
        our_entry.with_mode(their_entry.mode()).ok()
    } else {
        None
    }
}

fn merge_subtrees(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_subtree: &Tree,
    our_entry: &TreeEntry,
    their_entry: &TreeEntry,
    prefix: &str,
    labels: ConflictLabels<'_>,
    semantic_merge: Option<SemanticMergeFn>,
) -> Result<(TreeEntry, Vec<String>)> {
    let Some(our_hash) = our_entry.tree_hash() else {
        return Err(anyhow!(merge_integrity_refusal(
            format!(
                "merge entry {:?}/{} has tree type but no tree hash",
                prefix,
                our_entry.name()
            ),
            format!(
                "our merge path {:?}/{} records a tree entry without a tree object hash",
                prefix,
                our_entry.name()
            ),
            "the recursive merge cannot load our subtree and could silently drop or overwrite descendants",
            "HEAD, refs, and worktree were left unchanged; merge stopped before applying the malformed subtree",
        )));
    };
    let Some(their_hash) = their_entry.tree_hash() else {
        return Err(anyhow!(merge_integrity_refusal(
            format!(
                "merge entry {:?}/{} has tree type but no tree hash",
                prefix,
                their_entry.name()
            ),
            format!(
                "their merge path {:?}/{} records a tree entry without a tree object hash",
                prefix,
                their_entry.name()
            ),
            "the recursive merge cannot load their subtree and could silently drop or overwrite descendants",
            "HEAD, refs, and worktree were left unchanged; merge stopped before applying the malformed subtree",
        )));
    };
    let our_subtree = require_subtree(
        store,
        &our_hash,
        &format!("our subtree at {prefix:?}/{}", our_entry.name()),
    )?;
    let their_subtree = require_subtree(
        store,
        &their_hash,
        &format!("their subtree at {prefix:?}/{}", their_entry.name()),
    )?;
    let (merged_subtree, conflicts) = three_way_merge_recursive(
        store,
        blob_source,
        base_subtree,
        &our_subtree,
        &their_subtree,
        prefix,
        labels,
        semantic_merge,
    )?;
    let merged_hash = store.put_tree(&merged_subtree)?;
    Ok((
        TreeEntry::directory(our_entry.name().to_string(), merged_hash)?,
        conflicts,
    ))
}

fn generate_conflict_content(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    our_entry: &TreeEntry,
    their_entry: &TreeEntry,
    labels: ConflictLabels<'_>,
) -> Result<Vec<u8>> {
    let our_content = conflict_entry_content(store, blob_source, our_entry)?;
    let their_content = conflict_entry_content(store, blob_source, their_entry)?;
    Ok(format_conflict_content(
        &our_content,
        &their_content,
        labels,
    ))
}

fn conflict_entry_content(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    entry: &TreeEntry,
) -> Result<Vec<u8>> {
    if entry.is_tree() {
        let Some(hash) = entry.tree_hash() else {
            return Err(anyhow!(merge_integrity_refusal(
                format!(
                    "conflict entry {:?} has tree type but no tree hash",
                    entry.name()
                ),
                format!(
                    "conflict path {:?} records a tree entry without a tree object hash",
                    entry.name()
                ),
                "the conflict renderer cannot describe the subtree and could otherwise hide a malformed tree entry",
                "HEAD, refs, and worktree were left unchanged; merge stopped before writing conflict content for the malformed entry",
            )));
        };
        let tree = require_subtree(
            store,
            &hash,
            &format!("conflict-entry subtree {:?}", entry.name()),
        )?;
        let mut names: Vec<&str> = tree.entries().iter().map(|child| child.name()).collect();
        names.sort_unstable();
        let listing = if names.is_empty() {
            String::from("<empty directory>")
        } else {
            format!("<directory>\n{}", names.join("\n"))
        };
        Ok(listing.into_bytes())
    } else if let Some(hash) = entry.leaf_content_hash() {
        load_blob_content(blob_source, &hash, entry.name())
    } else if let Some(target) = entry.gitlink_target() {
        Ok(gitlink_placeholder_bytes(&target))
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use objects::store::InMemoryStore;

    use super::*;

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

        let blob_source = &store;
        let err = load_blob_content(&blob_source, &missing_hash, "src/foo.rs")
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
        assert!(
            msg.contains("Unsafe condition:")
                && msg.contains("src/foo.rs")
                && msg.contains(&missing_hash.to_hex()),
            "unsafe condition must name the path and missing blob hash: {msg}"
        );
        assert!(
            msg.contains("Would change:")
                && msg.contains("empty bytes")
                && msg.contains("silent content loss"),
            "would-change text must describe the loss mode: {msg}"
        );
        assert!(
            msg.contains("Preserved:")
                && msg.contains("HEAD")
                && msg.contains("worktree were left unchanged"),
            "preserved-state text must say what remained untouched: {msg}"
        );
    }

    #[test]
    fn present_merge_blob_loads_content() {
        let store = InMemoryStore::new();
        let blob = Blob::new(b"hello\nworld\n".to_vec());
        let hash = store.put_blob(&blob).unwrap();
        let blob_source = &store;
        let content =
            load_blob_content(&blob_source, &hash, "src/bar.rs").expect("blob present in store");
        assert_eq!(content, b"hello\nworld\n");
    }

    #[test]
    fn merge_trees_with_missing_base_blob_refuses_with_plain_store_source() {
        let store = InMemoryStore::new();
        let missing_base_hash = Blob::new(b"base\n".to_vec()).hash();
        let our_hash = store.put_blob(&Blob::new(b"ours\n".to_vec())).unwrap();
        let their_hash = store.put_blob(&Blob::new(b"theirs\n".to_vec())).unwrap();
        let base_tree = Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), missing_base_hash, false).unwrap(),
        ]);
        let our_tree = Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), our_hash, false).unwrap(),
        ]);
        let their_tree = Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), their_hash, false).unwrap(),
        ]);

        let blob_source = &store;
        let err = match crate::merge_trees(
            &store,
            &blob_source,
            &base_tree,
            &our_tree,
            &their_tree,
            crate::MergeOptions::default(),
        ) {
            Ok(_) => panic!("plain store blob source must refuse absent merge blobs"),
            Err(err) => err,
        };

        assert!(
            err.downcast_ref::<crate::MergeError>().is_some(),
            "missing plain-store blob should stay typed as MergeError: {err}"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("file.txt") && msg.contains(&missing_base_hash.to_hex()),
            "merge-level refusal should name the path and missing base blob: {msg}"
        );
    }

    /// Symmetric guard for [`load_blob_content`]: a missing subtree
    /// during recursive three-way merge must surface as a hard error
    /// rather than coercing to `Tree::default()`. The pre-#90 code did
    /// `get_tree(...)?.unwrap_or_default()`, which made the recursive
    /// merger see an "empty" subtree on one side and silently delete
    /// every file under that path in the merged result — no conflict,
    /// no diff, just data loss.
    #[test]
    fn missing_merge_subtree_surfaces_as_error_not_silent_empty() {
        // Build a non-empty subtree's hash without ever inserting it
        // into the store. Using a non-empty tree avoids any concern
        // about empty-tree canonical-hash collisions. The hash is
        // structurally valid; it just doesn't resolve to any object —
        // exactly what a dropped pack / corrupt store / missing
        // partial-fetch blob looks like to a downstream caller.
        let blob_hash = Blob::new(b"placeholder\n".to_vec()).hash();
        let phantom_subtree = Tree::from_entries(vec![
            TreeEntry::file("inner.rs".to_string(), blob_hash, false).unwrap(),
        ]);
        let phantom_hash = phantom_subtree.hash();

        let store = InMemoryStore::new();
        assert!(
            store.get_tree(&phantom_hash).unwrap().is_none(),
            "test setup: phantom subtree must not be present in the store",
        );

        let err = require_subtree(&store, &phantom_hash, "src/sub")
            .expect_err("expected a hard error on missing merge subtree");
        let msg = format!("{err}");
        assert!(
            msg.contains("missing from the object store"),
            "error must surface the missing-subtree diagnostic, got: {msg}"
        );
        assert!(
            msg.contains("src/sub"),
            "error must mention the affected merge-side label so operators \
             can locate the corrupt entry; got: {msg}"
        );
        assert!(
            msg.contains("heddle fsck"),
            "error must point at the recovery command so operators have a \
             next step instead of just a stack trace; got: {msg}"
        );
        assert!(
            msg.contains("Unsafe condition:")
                && msg.contains("src/sub")
                && msg.contains(&phantom_hash.to_hex()),
            "unsafe condition must name the label and missing subtree hash: {msg}"
        );
        assert!(
            msg.contains("Would change:")
                && msg.contains("empty subtree")
                && msg.contains("silently delete"),
            "would-change text must describe the loss mode: {msg}"
        );
        assert!(
            msg.contains("Preserved:")
                && msg.contains("HEAD")
                && msg.contains("worktree were left unchanged"),
            "preserved-state text must say what remained untouched: {msg}"
        );
    }

    /// Happy path: when the subtree is present, the helper returns it
    /// rather than erroring. Belt-and-suspenders against a regression
    /// where someone tightens the require_subtree contract too far
    /// (e.g. always erroring because of a stray `Err(...)` branch).
    #[test]
    fn present_merge_subtree_loads_tree() {
        let store = InMemoryStore::new();
        let blob_hash = store.put_blob(&Blob::new(b"x".to_vec())).unwrap();
        let subtree = Tree::from_entries(vec![
            TreeEntry::file("inner.rs".to_string(), blob_hash, false).unwrap(),
        ]);
        let hash = store.put_tree(&subtree).unwrap();

        let loaded = require_subtree(&store, &hash, "src/sub").expect("subtree present in store");
        assert_eq!(loaded.entries().len(), 1);
        assert_eq!(loaded.entries()[0].name(), "inner.rs");
    }

    fn put_blob(store: &InMemoryStore, bytes: &[u8]) -> ContentHash {
        store.put_blob(&Blob::new(bytes.to_vec())).unwrap()
    }

    fn entry_by_name<'a>(tree: &'a Tree, name: &str) -> &'a TreeEntry {
        tree.entries()
            .iter()
            .find(|entry| entry.name() == name)
            .unwrap_or_else(|| panic!("missing entry {name}"))
    }

    /// Untouched executable + symlink must survive a three-way merge that
    /// only edits another path (recursive / no-rename path).
    #[test]
    fn merge_preserves_executable_and_symlink_when_other_path_changes() {
        let store = InMemoryStore::new();
        let script = put_blob(&store, b"#!/bin/sh\necho base\n");
        let link = put_blob(&store, b"target.txt");
        let other = put_blob(&store, b"other-base\n");
        let other_theirs = put_blob(&store, b"other-theirs\n");

        let base = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), script, true).unwrap(),
            TreeEntry::symlink("link".to_string(), link).unwrap(),
            TreeEntry::file("other.txt".to_string(), other, false).unwrap(),
        ]);
        let ours = base.clone();
        let theirs = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), script, true).unwrap(),
            TreeEntry::symlink("link".to_string(), link).unwrap(),
            TreeEntry::file("other.txt".to_string(), other_theirs, false).unwrap(),
        ]);

        let result = crate::merge_trees(
            &store,
            &&store,
            &base,
            &ours,
            &theirs,
            crate::MergeOptions::default(),
        )
        .expect("merge should succeed");
        assert!(
            result.conflicts.is_empty(),
            "conflicts: {:?}",
            result.conflicts
        );

        let tool = entry_by_name(&result.tree, "tool.sh");
        assert!(tool.is_blob(), "tool.sh must remain a blob");
        assert!(
            tool.is_executable(),
            "tool.sh must keep +x through merge that only touches other.txt"
        );
        assert_eq!(tool.blob_hash(), Some(script));

        let link_entry = entry_by_name(&result.tree, "link");
        assert!(
            link_entry.is_symlink(),
            "link must remain a symlink, not flattened to a file"
        );
        assert_eq!(link_entry.symlink_hash(), Some(link));

        let other_entry = entry_by_name(&result.tree, "other.txt");
        assert_eq!(other_entry.blob_hash(), Some(other_theirs));
    }

    /// Content merge where only theirs carries +x must still produce +x
    /// (union policy), not invent executable:false from "our" side alone.
    #[test]
    fn merge_content_union_preserves_executable_from_either_side() {
        let store = InMemoryStore::new();
        let base_hash = put_blob(&store, b"line1\nline2\nline3\n");
        let our_hash = put_blob(&store, b"OUR\nline2\nline3\n");
        let their_hash = put_blob(&store, b"line1\nline2\nTHEIR\n");

        let base = Tree::from_entries(vec![
            TreeEntry::file("script.sh".to_string(), base_hash, false).unwrap(),
        ]);
        let ours = Tree::from_entries(vec![
            TreeEntry::file("script.sh".to_string(), our_hash, false).unwrap(),
        ]);
        let theirs = Tree::from_entries(vec![
            TreeEntry::file("script.sh".to_string(), their_hash, true).unwrap(),
        ]);

        let result = crate::merge_trees(
            &store,
            &&store,
            &base,
            &ours,
            &theirs,
            crate::MergeOptions::default(),
        )
        .expect("disjoint content merge should succeed");
        assert!(result.conflicts.is_empty());

        let entry = entry_by_name(&result.tree, "script.sh");
        assert!(
            entry.is_executable(),
            "union policy: their +x must survive content merge"
        );
        let content = store
            .get_blob(&entry.blob_hash().unwrap())
            .unwrap()
            .unwrap();
        let text = String::from_utf8(content.content().to_vec()).unwrap();
        assert!(text.contains("OUR"), "missing our hunk: {text}");
        assert!(text.contains("THEIR"), "missing their hunk: {text}");
    }

    /// Conflict markers must not force 100644 when both sides were executable.
    #[test]
    fn merge_conflict_preserves_executable_when_both_sides_executable() {
        let store = InMemoryStore::new();
        let base_hash = put_blob(&store, b"base\n");
        let our_hash = put_blob(&store, b"ours-conflict\n");
        let their_hash = put_blob(&store, b"theirs-conflict\n");

        let base = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), base_hash, true).unwrap(),
        ]);
        let ours = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), our_hash, true).unwrap(),
        ]);
        let theirs = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), their_hash, true).unwrap(),
        ]);

        let result = crate::merge_trees(
            &store,
            &&store,
            &base,
            &ours,
            &theirs,
            crate::MergeOptions::default(),
        )
        .expect("conflicted merge still returns a tree");
        assert_eq!(result.conflicts, vec!["tool.sh".to_string()]);

        let entry = entry_by_name(&result.tree, "tool.sh");
        assert!(
            entry.is_executable(),
            "conflict markers must not drop +x when both sides were executable"
        );
        let content = store
            .get_blob(&entry.blob_hash().unwrap())
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(content.content());
        assert!(text.contains("<<<<<<<"), "expected conflict markers: {text}");
    }

    /// Rename path rebuilds via `build_nested_tree`; that rebuild must carry
    /// FileMode and Symlink kind for untouched leaves (not invent file/644).
    #[test]
    fn merge_rename_rebuild_preserves_executable_and_symlink() {
        let store = InMemoryStore::new();
        let script = put_blob(&store, b"#!/bin/sh\necho ok\n");
        let link = put_blob(&store, b"target.txt");
        let moved = put_blob(&store, b"fn main() {}\n");
        let other = put_blob(&store, b"other\n");
        let other_theirs = put_blob(&store, b"other-theirs\n");

        let base = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), script, true).unwrap(),
            TreeEntry::symlink("link".to_string(), link).unwrap(),
            TreeEntry::file("old.rs".to_string(), moved, false).unwrap(),
            TreeEntry::file("other.txt".to_string(), other, false).unwrap(),
        ]);
        // Our side renames old.rs → new.rs (forces merge_with_renames / flat rebuild).
        let ours = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), script, true).unwrap(),
            TreeEntry::symlink("link".to_string(), link).unwrap(),
            TreeEntry::file("new.rs".to_string(), moved, false).unwrap(),
            TreeEntry::file("other.txt".to_string(), other, false).unwrap(),
        ]);
        let theirs = Tree::from_entries(vec![
            TreeEntry::file("tool.sh".to_string(), script, true).unwrap(),
            TreeEntry::symlink("link".to_string(), link).unwrap(),
            TreeEntry::file("old.rs".to_string(), moved, false).unwrap(),
            TreeEntry::file("other.txt".to_string(), other_theirs, false).unwrap(),
        ]);

        let result = crate::merge_trees(
            &store,
            &&store,
            &base,
            &ours,
            &theirs,
            crate::MergeOptions::default(),
        )
        .expect("rename merge should succeed");
        assert!(
            result.conflicts.is_empty(),
            "unexpected conflicts: {:?}",
            result.conflicts
        );
        assert!(
            !result.renames.is_empty(),
            "expected rename detection to engage flat rebuild path"
        );

        let tool = entry_by_name(&result.tree, "tool.sh");
        assert!(
            tool.is_executable(),
            "flat rebuild must preserve +x on untouched tool.sh"
        );
        assert!(tool.is_blob());

        let link_entry = entry_by_name(&result.tree, "link");
        assert!(
            link_entry.is_symlink(),
            "flat rebuild must preserve symlink kind for untouched link"
        );

        assert!(
            entry_by_name(&result.tree, "new.rs").blob_hash() == Some(moved),
            "renamed path should land at new.rs"
        );
        assert_eq!(
            entry_by_name(&result.tree, "other.txt").blob_hash(),
            Some(other_theirs)
        );
    }

    #[test]
    fn build_nested_tree_carries_mode_and_symlink_kind() {
        let store = InMemoryStore::new();
        let script = put_blob(&store, b"#!/bin/sh\n");
        let link = put_blob(&store, b"tgt");
        let nested = put_blob(&store, b"nested\n");

        let mut flat = HashMap::new();
        flat.insert("tool.sh".to_string(), FlatLeaf::blob(script, true));
        flat.insert("link".to_string(), FlatLeaf::symlink(link));
        flat.insert("dir/inner.sh".to_string(), FlatLeaf::blob(nested, true));

        let tree = build_nested_tree(&store, &flat).unwrap();
        let tool = entry_by_name(&tree, "tool.sh");
        assert!(tool.is_executable());
        assert!(entry_by_name(&tree, "link").is_symlink());

        let dir = entry_by_name(&tree, "dir");
        assert!(dir.is_tree());
        let subtree = store.get_tree(&dir.tree_hash().unwrap()).unwrap().unwrap();
        let inner = entry_by_name(&subtree, "inner.sh");
        assert!(inner.is_executable());
    }
}
