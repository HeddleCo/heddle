// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use objects::object::Tree;
use repo::Repository;
use tracing::debug;

use super::{
    ConflictLabels, MergeResult,
    executor::{merge_with_renames, merge_without_renames},
};
use crate::cli::commands::merge::{
    merge_renames::{DEFAULT_RENAME_THRESHOLD, detect_merge_renames},
    rename_matcher::infer_directory_renames,
};

pub(crate) fn three_way_merge(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
) -> Result<MergeResult> {
    three_way_merge_with_labels(
        repo,
        base_tree,
        our_tree,
        their_tree,
        ConflictLabels::DEFAULT,
    )
}

pub(crate) fn three_way_merge_with_labels(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    labels: ConflictLabels<'_>,
) -> Result<MergeResult> {
    let rename_map = detect_merge_renames(
        repo.store(),
        base_tree,
        our_tree,
        their_tree,
        DEFAULT_RENAME_THRESHOLD,
    )?;

    let (tree, conflicts) =
        if rename_map.our_renames.is_empty() && rename_map.their_renames.is_empty() {
            merge_without_renames(repo, base_tree, our_tree, their_tree, labels)?
        } else {
            merge_with_renames(repo, base_tree, our_tree, their_tree, &rename_map, labels)?
        };

    let renames = collect_renames(&rename_map);
    let mut all_renames = rename_map.our_renames.clone();
    for (path, rename) in &rename_map.their_renames {
        all_renames
            .entry(path.clone())
            .or_insert_with(|| rename.clone());
    }

    Ok(MergeResult {
        tree,
        conflicts,
        renames,
        directory_renames: infer_directory_renames(&all_renames),
    })
}

fn collect_renames(
    rename_map: &crate::cli::commands::merge::merge_renames::MergeRenameMap,
) -> Vec<(String, String, f64)> {
    let mut renames = Vec::new();

    for rename in rename_map.our_renames.values() {
        debug!(from = %rename.from_path, to = %rename.to_path, score = rename.score, "Detected rename on our side");
        renames.push((
            rename.from_path.clone(),
            rename.to_path.clone(),
            rename.score,
        ));
    }
    for rename in rename_map.their_renames.values() {
        if rename_map.our_renames.contains_key(&rename.from_path) {
            continue;
        }
        debug!(from = %rename.from_path, to = %rename.to_path, score = rename.score, "Detected rename on their side");
        renames.push((
            rename.from_path.clone(),
            rename.to_path.clone(),
            rename.score,
        ));
    }

    renames.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    renames
}