// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, path::Path};

use objects::{
    object::{Attribution, Blob, Principal, State, StateId, Tree, TreeEntry},
    store::ObjectStore,
};
use tempfile::TempDir;

use super::{CommitGraphIndex, HistoryQuery, Repository};

const HISTORY_LEN: usize = 2_048;
const QUERY_LIMIT: usize = 20;
const ANCESTOR_GATE: u64 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HistoryCounts {
    ancestors_visited: u64,
    objects_decoded: u64,
}

#[test]
#[ignore = "release-only bounded-history contract; run with `HEDDLE_PROFILE=1 cargo test --release -p heddle-repo bounded_history_reads_release_contract -- --ignored --nocapture`"]
fn bounded_history_reads_release_contract() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "bounded-history performance contract requires cargo test --release"
    );
    assert!(
        std::env::var("HEDDLE_PROFILE").is_ok(),
        "bounded-history performance contract requires HEDDLE_PROFILE=1"
    );

    let temp = TempDir::new().expect("history fixture tempdir");
    let repo = Repository::init_default(temp.path()).expect("initialize history fixture");
    let old_tree = put_file_tree(&repo, b"old line\n");
    let new_tree = put_file_tree(&repo, b"new line\n");

    let mut tip = put_state(&repo, old_tree, Vec::new(), "root");
    for generation in 1..HISTORY_LEN {
        tip = put_state(
            &repo,
            old_tree,
            vec![tip.id()],
            &format!("history-{generation}"),
        );
    }
    let left = put_state(&repo, old_tree, vec![tip.id()], "left");
    let right = put_state(&repo, old_tree, vec![tip.id()], "right");
    let blame_tip = put_state(&repo, new_tree, vec![tip.id()], "blame-tip");

    let mut graph = CommitGraphIndex::new(&repo);
    graph.ensure_loaded(left.id()).expect("warm left ancestry");
    graph
        .ensure_loaded(right.id())
        .expect("warm right ancestry");
    graph
        .ensure_loaded(blame_tip.id())
        .expect("warm query ancestry");

    let before_merge = heddle_perf_contract::snapshot();
    assert_eq!(
        graph
            .find_merge_base(&left.id(), &right.id())
            .expect("bounded merge-base"),
        Some(tip.id())
    );
    let merge_counts = counts_since(before_merge);
    assert_history_bound("merge-base", merge_counts, 4, 0);

    let before_query = heddle_perf_contract::snapshot();
    let history = repo
        .query_history(&HistoryQuery::new(Some(blame_tip.id())).with_limit(QUERY_LIMIT))
        .expect("bounded history query");
    assert_eq!(history.len(), QUERY_LIMIT);
    let query_counts = counts_since(before_query);
    assert_history_bound(
        "history-query",
        query_counts,
        QUERY_LIMIT as u64,
        QUERY_LIMIT as u64,
    );

    let before_blame = heddle_perf_contract::snapshot();
    repo.get_file_provenance_for_state(&blame_tip, Path::new("fixture.txt"))
        .expect("bounded blame")
        .expect("fixture provenance");
    let blame_counts = counts_since(before_blame);
    assert_history_bound("blame", blame_counts, 1, 8);

    let unbounded_visited =
        full_ancestry_visits(&repo, left.id()) + full_ancestry_visits(&repo, right.id());
    let before_negative = heddle_perf_contract::snapshot();
    heddle_perf_contract::record_ancestors_visited(unbounded_visited);
    let negative_counts = counts_since(before_negative);
    assert_eq!(negative_counts.ancestors_visited, unbounded_visited);
    assert!(
        negative_counts.ancestors_visited > ANCESTOR_GATE,
        "unbounded negative control did not make the ancestry gate red: {negative_counts:?}"
    );

    println!(
        "HISTORY_NEGATIVE_CONTROL ancestors_visited={} gate={} outcome=red",
        negative_counts.ancestors_visited, ANCESTOR_GATE
    );
    println!("HISTORY GATE green");
}

fn assert_history_bound(
    operation: &str,
    counts: HistoryCounts,
    max_ancestors: u64,
    max_objects: u64,
) {
    println!(
        "HISTORY_GATE operation={operation} ancestors_visited={} history_objects_decoded={} ancestor_gate={max_ancestors} decode_gate={max_objects}",
        counts.ancestors_visited, counts.objects_decoded
    );
    assert!(
        counts.ancestors_visited > 0,
        "{operation} did not register ancestry visits"
    );
    if max_objects > 0 {
        assert!(
            counts.objects_decoded > 0,
            "{operation} did not register decoded history objects"
        );
    }
    assert!(
        counts.ancestors_visited <= max_ancestors,
        "{operation} ancestry gate red: {counts:?}, max ancestors {max_ancestors}"
    );
    assert!(
        counts.objects_decoded <= max_objects,
        "{operation} decode gate red: {counts:?}, max objects {max_objects}"
    );
}

fn counts_since(before: heddle_perf_contract::StructuralCounters) -> HistoryCounts {
    let after = heddle_perf_contract::snapshot();
    HistoryCounts {
        ancestors_visited: after
            .ancestors_visited
            .checked_sub(before.ancestors_visited)
            .expect("ancestry counter is monotonic"),
        objects_decoded: after
            .history_objects_decoded
            .checked_sub(before.history_objects_decoded)
            .expect("history decode counter is monotonic"),
    }
}

fn put_file_tree(repo: &Repository, content: &[u8]) -> objects::object::ContentHash {
    let blob = repo
        .store()
        .put_blob(&Blob::from_slice(content))
        .expect("put fixture blob");
    repo.store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("fixture.txt".to_string(), blob, false).expect("fixture tree entry"),
        ]))
        .expect("put fixture tree")
}

fn put_state(
    repo: &Repository,
    tree: objects::object::ContentHash,
    parents: Vec<StateId>,
    principal: &str,
) -> State {
    let state = State::new(
        tree,
        parents,
        Attribution::human(Principal::new(
            principal,
            format!("{principal}@example.com"),
        )),
    );
    repo.store().put_state(&state).expect("put fixture state");
    state
}

fn full_ancestry_visits(repo: &Repository, start: StateId) -> u64 {
    let mut visited = HashSet::new();
    let mut stack = vec![start];
    while let Some(state_id) = stack.pop() {
        if !visited.insert(state_id) {
            continue;
        }
        let state = repo
            .store()
            .get_state(&state_id)
            .expect("read unbounded fixture state")
            .expect("unbounded fixture state exists");
        stack.extend(state.parents);
    }
    visited.len() as u64
}
