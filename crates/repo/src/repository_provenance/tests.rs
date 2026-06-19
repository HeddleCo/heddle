// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use objects::{
    object::{Attribution, Blob, ChangeId, ContentHash, Origin, Principal, State, Tree, TreeEntry},
    store::LocalObjectStore,
};
use tempfile::TempDir;

use super::helpers::{build_single_origin_provenance, lcs_line_matches};
use crate::Repository;

#[test]
fn lcs_preserves_existing_line_matches() {
    let old_lines = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let new_lines = vec!["a".to_string(), "x".to_string(), "c".to_string()];
    let matches = lcs_line_matches(&old_lines, &new_lines);
    assert_eq!(matches, vec![(0, 0), (2, 2)]);
}

/// Helper: write a single-file blob → tree → state to the store and
/// return the resulting state. Each call invents a fresh principal so
/// the per-state attribution differs across the chain.
fn put_state_with_file(
    store: &impl LocalObjectStore,
    file: &str,
    content: &[u8],
    parents: Vec<ChangeId>,
    principal_name: &str,
) -> State {
    let blob_hash = store.put_blob(&Blob::from_slice(content)).unwrap();
    let tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file(file.to_string(), blob_hash, false).unwrap(),
        ]))
        .unwrap();
    let state = State::new(
        tree_hash,
        parents,
        Attribution::human(Principal::new(
            principal_name.to_string(),
            format!("{principal_name}@example.com"),
        )),
    );
    store.put_state(&state).unwrap();
    state
}

#[test]
fn merge_provenance_credits_each_parent_for_its_lines() {
    // Reproduction of the issue caught while dogfooding `heddle blame`
    // on the imported ripgrep repo: with the legacy "merge => return
    // None" short-circuit, every line in a merge state's tree was
    // attributed to whoever pressed the merge button rather than to
    // the original authors on the branches that contributed the
    // lines. This test builds the smallest possible merge fixture
    // and asserts that lines unique to each side keep their author.
    //
    // Topology:
    //
    //          base (alice: "fn shared\n")
    //          /   \
    //   ours-branch   theirs-branch
    //   (bob,         (carol,
    //    "fn shared    "fn shared
    //     fn from_bob") fn from_carol")
    //          \   /
    //         merge (dave) — content union of both sides
    //         "fn shared\nfn from_bob\nfn from_carol\n"
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let base = put_state_with_file(store, "lib.rs", b"fn shared() {}\n", Vec::new(), "alice");
    let ours = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn from_bob() {}\n",
        vec![base.change_id],
        "bob",
    );
    let theirs = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn from_carol() {}\n",
        vec![base.change_id],
        "carol",
    );
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn from_bob() {}\nfn from_carol() {}\n",
        vec![ours.change_id, theirs.change_id],
        "dave",
    );

    let provenance = repo
        .get_file_provenance_for_state(&merge, Path::new("lib.rs"))
        .expect("provenance lookup should succeed for an N-parent merge")
        .expect("merge provenance should now be populated, not the legacy None");

    let line_origins = provenance
        .line_origin_set_indexes()
        .expect("validated provenance should yield indexes");

    // Helper: collect the set of principal names attributed to `line`.
    let principals_at = |line_idx: usize| -> Vec<String> {
        let set_idx = line_origins[line_idx];
        provenance.origin_sets[set_idx as usize]
            .origin_indexes
            .iter()
            .map(|i| {
                provenance.origins[*i as usize]
                    .attribution
                    .principal
                    .name
                    .clone()
            })
            .collect()
    };

    // Line 0 ("fn shared() {}") existed in base; alice is the authentic
    // author. Bob, carol, and dave all *carried* it forward but didn't
    // introduce it. The N-parent merge should still find alice via the
    // recursive walk into either ours or theirs.
    let line_0 = principals_at(0);
    assert!(
        line_0.contains(&"alice".to_string()),
        "line 0 (`fn shared`) should credit alice (the original author), got {line_0:?}"
    );

    // Line 1 ("fn from_bob") came from the ours branch. Bob authored
    // it; merging dave shouldn't take credit.
    let line_1 = principals_at(1);
    assert!(
        line_1.contains(&"bob".to_string()),
        "line 1 (`fn from_bob`) should credit bob, got {line_1:?}"
    );
    assert!(
        !line_1.contains(&"dave".to_string()),
        "line 1 must NOT credit dave (the merger), got {line_1:?}"
    );

    // Line 2 ("fn from_carol") came from the theirs branch. Same shape.
    let line_2 = principals_at(2);
    assert!(
        line_2.contains(&"carol".to_string()),
        "line 2 (`fn from_carol`) should credit carol, got {line_2:?}"
    );
    assert!(
        !line_2.contains(&"dave".to_string()),
        "line 2 must NOT credit dave (the merger), got {line_2:?}"
    );
}

#[test]
fn merge_with_genuinely_new_line_attributes_to_merger() {
    // If the merge commit *itself* introduces a line that wasn't on
    // either parent (this happens when a maintainer hand-resolves
    // conflicts and the resolution is genuinely novel), that line
    // should be credited to the merger — not to either parent.
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let base = put_state_with_file(store, "lib.rs", b"a\n", Vec::new(), "alice");
    let ours = put_state_with_file(store, "lib.rs", b"a\nb\n", vec![base.change_id], "bob");
    let theirs = put_state_with_file(store, "lib.rs", b"a\nc\n", vec![base.change_id], "carol");
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"a\nb\nc\nNOVEL\n",
        vec![ours.change_id, theirs.change_id],
        "dave",
    );

    let provenance = repo
        .get_file_provenance_for_state(&merge, Path::new("lib.rs"))
        .unwrap()
        .unwrap();
    let line_origins = provenance.line_origin_set_indexes().unwrap();

    let principals_at = |line_idx: usize| -> Vec<String> {
        let set_idx = line_origins[line_idx];
        provenance.origin_sets[set_idx as usize]
            .origin_indexes
            .iter()
            .map(|i| {
                provenance.origins[*i as usize]
                    .attribution
                    .principal
                    .name
                    .clone()
            })
            .collect()
    };

    // Line 3 (`NOVEL`) was introduced by the merge itself.
    let line_3 = principals_at(3);
    assert_eq!(
        line_3,
        vec!["dave".to_string()],
        "novel line in merge state should credit only the merger"
    );
}

#[test]
fn linear_commit_provenance_unchanged_under_n_parent_path() {
    // Sanity: the N-parent generalization must not regress the
    // single-parent case. After a chain alice → bob → carol on a
    // single file with one line per state, blame should still credit
    // alice for line 0, bob for line 1, carol for line 2.
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let s1 = put_state_with_file(store, "lib.rs", b"a\n", Vec::new(), "alice");
    let s2 = put_state_with_file(store, "lib.rs", b"a\nb\n", vec![s1.change_id], "bob");
    let s3 = put_state_with_file(store, "lib.rs", b"a\nb\nc\n", vec![s2.change_id], "carol");

    let provenance = repo
        .get_file_provenance_for_state(&s3, Path::new("lib.rs"))
        .unwrap()
        .unwrap();
    let line_origins = provenance.line_origin_set_indexes().unwrap();
    let principals_at = |line_idx: usize| -> Vec<String> {
        let set_idx = line_origins[line_idx];
        provenance.origin_sets[set_idx as usize]
            .origin_indexes
            .iter()
            .map(|i| {
                provenance.origins[*i as usize]
                    .attribution
                    .principal
                    .name
                    .clone()
            })
            .collect()
    };

    assert!(principals_at(0).contains(&"alice".to_string()));
    assert!(principals_at(1).contains(&"bob".to_string()));
    assert!(principals_at(2).contains(&"carol".to_string()));
}

#[test]
fn diamond_merge_provenance_walks_each_ancestor_once() {
    // Memoization regression: a "diamond" topology where two parents
    // both descend from the same grandparent is the canonical case
    // where uncached recursion does duplicate work — the grandparent
    // gets walked twice (once per side branch). With the
    // ProvenanceCache the recursion sees the grandparent's result on
    // its second visit and short-circuits.
    //
    //          base (alice)
    //          /     \
    //   left (bob)   right (carol)
    //          \     /
    //         merge (dave) — content union of both sides
    //
    // The contract we're asserting: blame at `merge` returns the
    // correct line attribution for the base-introduced line. The
    // *speed* property is what motivates the cache; the *correctness*
    // property is that adding the cache didn't change the answer.
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let base = put_state_with_file(store, "lib.rs", b"fn shared() {}\n", Vec::new(), "alice");
    let left = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn left() {}\n",
        vec![base.change_id],
        "bob",
    );
    let right = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn right() {}\n",
        vec![base.change_id],
        "carol",
    );
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn left() {}\nfn right() {}\n",
        vec![left.change_id, right.change_id],
        "dave",
    );

    let provenance = repo
        .get_file_provenance_for_state(&merge, Path::new("lib.rs"))
        .unwrap()
        .unwrap();
    let line_origins = provenance.line_origin_set_indexes().unwrap();
    let principals_at = |line_idx: usize| -> Vec<String> {
        let set_idx = line_origins[line_idx];
        provenance.origin_sets[set_idx as usize]
            .origin_indexes
            .iter()
            .map(|i| {
                provenance.origins[*i as usize]
                    .attribution
                    .principal
                    .name
                    .clone()
            })
            .collect()
    };

    // The shared line came from base (alice) and propagated through
    // both left and right. The N-parent walk should find alice as one
    // of its origins regardless of which parent path the cache visited
    // first.
    assert!(
        principals_at(0).contains(&"alice".to_string()),
        "diamond ancestor's line should still credit alice via the cached walk; got {:?}",
        principals_at(0)
    );
    assert!(principals_at(1).contains(&"bob".to_string()));
    assert!(principals_at(2).contains(&"carol".to_string()));
}

#[test]
fn file_provenance_validates_coverage() {
    let origin = Origin {
        state_id: ChangeId::generate(),
        attribution: Attribution::human(Principal::new("Test", "test@example.com")),
        created_at: chrono::Utc::now(),
        authored_at: None,
    };
    let provenance = build_single_origin_provenance(
        ContentHash::compute(b"hello"),
        &["hello".to_string()],
        origin,
    );
    provenance.validate().unwrap();
}
