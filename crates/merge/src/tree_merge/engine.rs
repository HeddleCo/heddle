// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use objects::{object::Tree, store::ObjectStore};
use tracing::debug;

use super::{
    DetectedRename, DirectoryRename, MergeBlobSource, MergeOptions, TreeMergeResult,
    executor::{merge_with_renames, merge_without_renames},
    rename_matcher::infer_directory_renames,
    renames::{MergeRenameMap, detect_merge_renames},
};

pub(crate) fn merge_trees(
    store: &impl ObjectStore,
    blob_source: &impl MergeBlobSource,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    options: MergeOptions<'_>,
) -> Result<TreeMergeResult> {
    let rename_map = detect_merge_renames(
        store,
        base_tree,
        our_tree,
        their_tree,
        options.rename_options.threshold,
        options.rename_options.semantic_similarity,
    )?;

    let (tree, conflicts) =
        if rename_map.our_renames.is_empty() && rename_map.their_renames.is_empty() {
            merge_without_renames(
                store,
                blob_source,
                base_tree,
                our_tree,
                their_tree,
                options.labels,
                options.semantic_merge,
            )?
        } else {
            merge_with_renames(
                store,
                blob_source,
                base_tree,
                our_tree,
                their_tree,
                &rename_map,
                options.labels,
                options.semantic_merge,
            )?
        };

    let renames = collect_renames(&rename_map);
    let mut all_renames = rename_map.our_renames.clone();
    for (path, rename) in &rename_map.their_renames {
        all_renames
            .entry(path.clone())
            .or_insert_with(|| rename.clone());
    }

    Ok(TreeMergeResult {
        tree,
        conflicts,
        renames,
        directory_renames: infer_directory_renames(&all_renames)
            .into_iter()
            .map(|(from, to)| DirectoryRename { from, to })
            .collect(),
    })
}

fn collect_renames(rename_map: &MergeRenameMap) -> Vec<DetectedRename> {
    let mut renames = Vec::new();

    for rename in rename_map.our_renames.values() {
        debug!(from = %rename.from_path, to = %rename.to_path, score = rename.score, "Detected rename on our side");
        renames.push(DetectedRename {
            from: rename.from_path.clone(),
            to: rename.to_path.clone(),
            score: rename.score,
        });
    }
    for rename in rename_map.their_renames.values() {
        if rename_map.our_renames.contains_key(&rename.from_path) {
            continue;
        }
        debug!(from = %rename.from_path, to = %rename.to_path, score = rename.score, "Detected rename on their side");
        renames.push(DetectedRename {
            from: rename.from_path.clone(),
            to: rename.to_path.clone(),
            score: rename.score,
        });
    }

    renames.sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));
    renames
}
