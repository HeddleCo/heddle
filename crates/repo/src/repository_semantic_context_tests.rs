// SPDX-License-Identifier: Apache-2.0
//! Capture-time SemanticContext tests (heddle#1069).

use std::{collections::BTreeSet, path::PathBuf};

use objects::{
    object::{Attribution, ContentHash, Principal, RiskSignalKind, State},
    store::ObjectStore,
};
use tempfile::TempDir;

use super::{Repository, build_semantic_context};
use crate::StateAttachmentKind;

fn author() -> Attribution {
    Attribution::human(Principal::new("Test", "test@example.com"))
}

fn snapshot(repo: &Repository, root: &std::path::Path, path: &str, content: &str) -> State {
    std::fs::write(root.join(path), content).unwrap();
    repo.snapshot_with_attribution(Some("capture".to_string()), None, author())
        .unwrap()
}

fn attachment_hash(
    repo: &Repository,
    state: &State,
    kind: StateAttachmentKind,
) -> Option<ContentHash> {
    repo.latest_state_attachment(&state.id(), kind)
        .unwrap()
        .and_then(|attachment| match attachment.body {
            objects::object::StateAttachmentBody::RiskSignals(hash)
            | objects::object::StateAttachmentBody::SemanticIndex(hash) => Some(hash),
            _ => None,
        })
}

fn load_risk_kinds(repo: &Repository, state: &State) -> Vec<RiskSignalKind> {
    let hash = match attachment_hash(repo, state, StateAttachmentKind::RiskSignals) {
        Some(hash) => hash,
        None => return Vec::new(),
    };
    let blob = repo.store().get_blob(&hash).unwrap().unwrap();
    objects::object::RiskSignalBlob::decode(blob.content())
        .unwrap()
        .signals
        .into_iter()
        .map(|signal| signal.kind)
        .collect()
}

const CORPUS: &str = "\
fn alpha() { let total = first + second + third + fourth; }
fn beta() { for widget in inventory { ship(widget); } }
fn gamma() { match colour { Red => stop(), Green => go() } }
fn delta() { while pending { dequeue().handle(); } flush(); }
";

#[test]
fn fmt_sweep_prunes_changed_paths_and_persists_no_tree_sitter_signals() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let first = snapshot(&repo, temp.path(), "hello.rs", "fn foo() -> i32 { 1 }\n");
    let reformatted = snapshot(
        &repo,
        temp.path(),
        "hello.rs",
        "fn foo() -> i32 {\n    1\n}\n",
    );

    let new_index = attachment_hash(&repo, &reformatted, StateAttachmentKind::SemanticIndex);
    let ctx = build_semantic_context(
        &repo,
        Some(&first),
        &reformatted,
        new_index.as_ref(),
        None,
        None,
    )
    .unwrap();
    assert!(
        ctx.changed_paths.is_empty(),
        "fmt-sweep must prune semantic changed_paths: {ctx:?}"
    );
    assert!(
        ctx.changed_symbols.is_empty(),
        "fmt-sweep must emit no changed symbols: {ctx:?}"
    );
    assert!(
        load_risk_kinds(&repo, &reformatted)
            .iter()
            .all(|kind| !matches!(
                kind,
                RiskSignalKind::Novelty
                    | RiskSignalKind::PatternDeviation
                    | RiskSignalKind::TestReachability
            )),
        "fmt-sweep must persist zero tree-sitter signals"
    );
}

#[test]
fn novel_shape_populates_context_and_fires_novelty() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let state = snapshot(&repo, temp.path(), "changed.rs", CORPUS);

    let kinds = load_risk_kinds(&repo, &state);
    assert!(
        kinds.contains(&RiskSignalKind::Novelty),
        "novel-shape capture must persist novelty, got {kinds:?}"
    );

    let index_hash = attachment_hash(&repo, &state, StateAttachmentKind::SemanticIndex);
    let ctx = build_semantic_context(&repo, None, &state, index_hash.as_ref(), None, None).unwrap();
    assert!(ctx.changed_paths.contains(&PathBuf::from("changed.rs")));
    assert!(
        ctx.corpus_complete,
        "single-file repo must finish the corpus walk"
    );
    let fns = ctx
        .new_functions
        .get(&PathBuf::from("changed.rs"))
        .expect("changed.rs must be parsed");
    assert_eq!(fns.len(), 4, "changed.rs functions: {fns:?}");
    assert_eq!(
        ctx.changed_symbols.len(),
        4,
        "first capture marks every new symbol"
    );
}

#[test]
fn one_function_edit_does_not_mark_siblings_changed() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let seed = snapshot(&repo, temp.path(), "changed.rs", CORPUS);
    let edited = snapshot(
        &repo,
        temp.path(),
        "changed.rs",
        "\
fn alpha() { let total = first + second + third + fourth; }
fn beta() { for widget in inventory { ship(widget); } }
fn gamma() { match colour { Red => stop(), Green => go() } }
fn delta() { if ready { launch(); } else { wait(); } abort(); }
",
    );

    let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
    let ctx = build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
        .unwrap();
    assert!(ctx.corpus_complete);
    assert_eq!(
        ctx.changed_symbols,
        BTreeSet::from([(PathBuf::from("changed.rs"), "delta".to_string())])
    );

    let novelty_symbols: BTreeSet<String> = load_risk_signals(&repo, &edited)
        .into_iter()
        .filter(|signal| signal.kind == RiskSignalKind::Novelty)
        .filter_map(|signal| signal.anchor.symbol)
        .collect();
    assert!(
        novelty_symbols.iter().all(|name| name == "delta"),
        "untouched siblings must not persist novelty: {novelty_symbols:?}"
    );
}

const TESTS: &str = "\
fn test_one() { assert!(true); }
fn test_two() { assert!(true); }
fn test_three() { assert!(true); }
";

#[test]
fn unchanged_tests_join_corpus_and_fire_reachability() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("lib.rs"), "fn orphan() { do_work(); }\n").unwrap();
    std::fs::write(temp.path().join("tests.rs"), TESTS).unwrap();
    let seed = repo
        .snapshot_with_attribution(Some("seed".to_string()), None, author())
        .unwrap();
    let edited = snapshot(
        &repo,
        temp.path(),
        "lib.rs",
        "fn orphan() { do_other(); }\n",
    );

    let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
    let ctx = build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
        .unwrap();
    assert!(
        ctx.corpus_complete,
        "small repo must finish the corpus walk"
    );
    assert_eq!(ctx.changed_paths, BTreeSet::from([PathBuf::from("lib.rs")]));
    assert!(
        ctx.new_functions.contains_key(&PathBuf::from("tests.rs")),
        "unchanged tests must join the new-state corpus: {:?}",
        ctx.new_functions.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        ctx.changed_symbols,
        BTreeSet::from([(PathBuf::from("lib.rs"), "orphan".to_string())])
    );
    assert!(
        load_risk_kinds(&repo, &edited).contains(&RiskSignalKind::TestReachability),
        "corpus must see unchanged tests so reachability can fire"
    );
}

fn load_risk_signals(repo: &Repository, state: &State) -> Vec<objects::object::RiskSignal> {
    let hash = match attachment_hash(repo, state, StateAttachmentKind::RiskSignals) {
        Some(hash) => hash,
        None => return Vec::new(),
    };
    let blob = repo.store().get_blob(&hash).unwrap().unwrap();
    objects::object::RiskSignalBlob::decode(blob.content())
        .unwrap()
        .signals
}
