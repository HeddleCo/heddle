// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::{ChangeId, Tree},
    store::LocalObjectStore,
};
use repo::Repository;

use crate::cli::commands::{bench_detect_renames, bench_find_merge_base, bench_three_way_merge};

pub fn find_merge_base_for_bench(
    repo: &Repository,
    state_a: &ChangeId,
    state_b: &ChangeId,
) -> anyhow::Result<Option<ChangeId>> {
    bench_find_merge_base(repo, state_a, state_b)
}

pub fn three_way_merge_for_bench(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
) -> anyhow::Result<(Tree, usize, usize, usize)> {
    bench_three_way_merge(repo, base_tree, our_tree, their_tree)
}

pub fn detect_renames_for_bench(
    store: &impl LocalObjectStore,
    base_tree: &Tree,
    branch_tree: &Tree,
) -> anyhow::Result<(usize, usize, usize)> {
    let (rename_count, stats) = bench_detect_renames(store, base_tree, branch_tree)?;
    Ok((
        rename_count,
        stats.metadata_candidate_pairs,
        stats.scored_pairs,
    ))
}
