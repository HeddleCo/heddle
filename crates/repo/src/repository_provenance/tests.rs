// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use objects::{
    object::{
        Attribution, Blob, ContentHash, FileProvenance, LineSpan, Origin, OriginSet, Principal,
        State, StateId, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use tempfile::TempDir;

use super::helpers::{build_single_origin_provenance, lcs_line_matches, lookup_tree_entry};
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
    store: &impl ObjectStore,
    file: &str,
    content: &[u8],
    parents: Vec<StateId>,
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
        vec![base.id()],
        "bob",
    );
    let theirs = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn from_carol() {}\n",
        vec![base.id()],
        "carol",
    );
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn from_bob() {}\nfn from_carol() {}\n",
        vec![ours.id(), theirs.id()],
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
    let ours = put_state_with_file(store, "lib.rs", b"a\nb\n", vec![base.id()], "bob");
    let theirs = put_state_with_file(store, "lib.rs", b"a\nc\n", vec![base.id()], "carol");
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"a\nb\nc\nNOVEL\n",
        vec![ours.id(), theirs.id()],
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
    let s2 = put_state_with_file(store, "lib.rs", b"a\nb\n", vec![s1.id()], "bob");
    let s3 = put_state_with_file(store, "lib.rs", b"a\nb\nc\n", vec![s2.id()], "carol");

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
        vec![base.id()],
        "bob",
    );
    let right = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn right() {}\n",
        vec![base.id()],
        "carol",
    );
    let merge = put_state_with_file(
        store,
        "lib.rs",
        b"fn shared() {}\nfn left() {}\nfn right() {}\n",
        vec![left.id(), right.id()],
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
        state_id: crate::test_state_id(),
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

#[test]
fn provenance_merge_unions_origin_sets_not_set_indexes() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let parent_blob_hash = store.put_blob(&Blob::from("kept\n")).unwrap();
    let parent_tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("lib.rs", parent_blob_hash, false).unwrap(),
        ]))
        .unwrap();
    let parent_origin = Origin {
        state_id: crate::test_state_id(),
        attribution: Attribution::human(Principal::new("Parent", "parent@example.com")),
        created_at: chrono::Utc::now(),
        authored_at: None,
    };
    let parent_provenance = FileProvenance::new(
        parent_blob_hash,
        1,
        vec![LineSpan {
            start_line: 0,
            line_len: 1,
            origin_set_index: 11,
        }],
        vec![parent_origin],
        (0..12)
            .map(|_| OriginSet {
                origin_indexes: vec![0],
            })
            .collect(),
    );
    parent_provenance
        .validate()
        .expect("fixture provenance should be valid");
    let parent_provenance_blob = store
        .put_blob(&Blob::from_slice(
            &rmp_serde::to_vec(&parent_provenance).unwrap(),
        ))
        .unwrap();
    let parent_provenance_root = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("lib.rs", parent_provenance_blob, false).unwrap(),
        ]))
        .unwrap();
    let parent = State::new(
        parent_tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Parent", "parent@example.com")),
    )
    .with_provenance(parent_provenance_root);
    store.put_state(&parent).unwrap();

    let child_blob_hash = store.put_blob(&Blob::from("kept\nadded\n")).unwrap();
    let child_tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("lib.rs", child_blob_hash, false).unwrap(),
        ]))
        .unwrap();
    let child = State::new(
        child_tree_hash,
        vec![parent.id()],
        Attribution::human(Principal::new("Child", "child@example.com")),
    );
    store.put_state(&child).unwrap();

    let provenance = repo
        .get_file_provenance_for_state(&child, Path::new("lib.rs"))
        .expect("provenance lookup should succeed")
        .expect("child provenance should be synthesized");
    provenance
        .validate()
        .expect("translated provenance must remain valid");
    let line_sets = provenance.line_origin_set_indexes().unwrap();
    let first_line_origins = &provenance.origin_sets[line_sets[0] as usize].origin_indexes;
    assert_eq!(first_line_origins, &[0]);
}

#[test]
fn lookup_tree_entry_characterizes_entry_policy_paths() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    let store = repo.store();

    let blob_hash = store.put_blob(&Blob::from_slice(b"blob content")).unwrap();
    let symlink_hash = store.put_blob(&Blob::from_slice(b"target.txt")).unwrap();
    let nested_blob_hash = store
        .put_blob(&Blob::from_slice(b"nested content"))
        .unwrap();
    let missing_subtree_hash = ContentHash::compute(b"not-in-store");

    let nested_tree = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("inner.txt", nested_blob_hash, false).unwrap(),
        ]))
        .unwrap();
    let missing_parent = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::directory("ghost".to_string(), missing_subtree_hash).unwrap(),
        ]))
        .unwrap();
    let root_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt", blob_hash, false).unwrap(),
            TreeEntry::symlink("link".to_string(), symlink_hash).unwrap(),
            TreeEntry::directory("dir".to_string(), nested_tree).unwrap(),
            TreeEntry::directory("missing".to_string(), missing_parent).unwrap(),
        ]))
        .unwrap();
    let tree = store.get_tree(&root_hash).unwrap().unwrap();

    let file = lookup_tree_entry(&repo, &tree, Path::new("file.txt")).unwrap();
    assert_eq!(file.blob_hash(), Some(blob_hash));

    let link = lookup_tree_entry(&repo, &tree, Path::new("link")).unwrap();
    assert!(link.is_symlink());

    let dir = lookup_tree_entry(&repo, &tree, Path::new("dir")).unwrap();
    assert!(dir.is_tree());

    let nested = lookup_tree_entry(&repo, &tree, Path::new("dir/inner.txt")).unwrap();
    assert_eq!(nested.blob_hash(), Some(nested_blob_hash));

    assert!(lookup_tree_entry(&repo, &tree, Path::new("nope.txt")).is_none());
    assert!(lookup_tree_entry(&repo, &tree, Path::new("dir/missing.txt")).is_none());
    assert!(lookup_tree_entry(&repo, &tree, Path::new("missing/ghost/inner.txt")).is_none());
}
