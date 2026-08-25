// SPDX-License-Identifier: Apache-2.0
//! Capture-time risk-signal computer, wired into `repo` through the
//! [`repo::signals::SignalComputer`] registration seam.
//!
//! Entry points call [`install_capture_signal_computer`] once at startup;
//! unregistered processes and repositories skip signal computation entirely —
//! identical to the historical "feature off" on-disk shape.

use std::sync::Arc;

use objects::{
    object::{Blob, ContentHash, RiskSignalBlob, State},
    store::ObjectStore,
};
use repo::{
    CaptureSemanticContext, Repository, Result, ReviewSignalsToml, build_semantic_context,
    signals::SignalComputer,
};
use tracing::warn;

use crate::{
    ReviewSignalsConfig,
    config::{
        InvariantAdjacencyConfig, NoveltyConfig, PatternDeviationConfig,
        SelfFlaggedUncertaintyConfig, TestReachabilityConfig,
    },
    registry::{SemanticContext, run_all},
};

/// The production signal computer: builds a [`SemanticContext`] from the
/// snapshot diff (tree-sitter-backed when the semantic index allows) and
/// runs the registry, persisting any fired signals as a `RiskSignalBlob`.
pub struct CaptureSignalComputer;

/// Install the production signal computer for this process.
///
/// CLI entry points call this before opening any repository so snapshot paths
/// that do not pass through the save verb (for example `revert`) retain the
/// same capture-time signal behavior.
pub fn install_capture_signal_computer() {
    repo::signals::install_default_computer(Arc::new(CaptureSignalComputer));
}

impl SignalComputer for CaptureSignalComputer {
    fn compute_and_persist(
        &self,
        repo: &Repository,
        prior: Option<&State>,
        new: &State,
        new_index: Option<&ContentHash>,
        source_blobs: Option<&std::collections::HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&std::collections::HashMap<ContentHash, &objects::object::Tree>>,
    ) -> Result<Option<ContentHash>> {
        let cfg = signals_config_from_repo(&repo.config().review.signals);
        let ctx = if tree_sitter_producers_enabled(&cfg) {
            match build_semantic_context(repo, prior, new, new_index, source_blobs, source_trees) {
                Ok(ctx) => semantic_context_from_capture(&ctx),
                Err(err) => {
                    warn!(
                        error = %err,
                        "failed to build SemanticContext; running state-only signals"
                    );
                    SemanticContext::new()
                }
            }
        } else {
            SemanticContext::new()
        };
        // The registry expects a non-Option prior. Use the new state itself
        // when none is available (initial snapshot) — the modules fire on
        // their own diagnostic content, not on diff vs prior, except where
        // they explicitly check parents (which is degraded-but-safe for an
        // identity comparison).
        let prior_owned;
        let prior_ref = match prior {
            Some(p) => p,
            None => {
                prior_owned = new.clone();
                &prior_owned
            }
        };
        let signals = run_all(prior_ref, new, &cfg, &ctx);
        if signals.is_empty() {
            return Ok(None);
        }
        match RiskSignalBlob::new(signals).encode() {
            Ok(bytes) => match repo.store().put_blob(&Blob::new(bytes)) {
                Ok(hash) => Ok(Some(hash)),
                Err(err) => {
                    warn!(error = %err, "failed to persist risk_signals blob; skipping");
                    Ok(None)
                }
            },
            Err(err) => {
                warn!(error = %err, "failed to encode risk_signals blob; skipping");
                Ok(None)
            }
        }
    }
}

fn tree_sitter_producers_enabled(cfg: &ReviewSignalsConfig) -> bool {
    cfg.novelty.enabled || cfg.test_reachability.enabled || cfg.pattern_deviation.enabled
}

/// Map the TOML-shaped repo config into the typed signals config.
fn signals_config_from_repo(t: &ReviewSignalsToml) -> ReviewSignalsConfig {
    ReviewSignalsConfig {
        novelty: NoveltyConfig {
            enabled: t.novelty.enabled,
            tolerance: t.novelty.tolerance,
        },
        test_reachability: TestReachabilityConfig {
            enabled: t.test_reachability.enabled,
            min_test_functions_in_repo: t.test_reachability.min_test_functions_in_repo,
        },
        pattern_deviation: PatternDeviationConfig {
            enabled: t.pattern_deviation.enabled,
            threshold: t.pattern_deviation.threshold,
        },
        invariant_adjacency: InvariantAdjacencyConfig {
            enabled: t.invariant_adjacency.enabled,
        },
        self_flagged_uncertainty: SelfFlaggedUncertaintyConfig {
            enabled: t.self_flagged_uncertainty.enabled,
            max_per_state: t.self_flagged_uncertainty.max_per_state,
        },
    }
}

/// Convert the repo-side capture context into the registry input.
fn semantic_context_from_capture(ctx: &CaptureSemanticContext) -> SemanticContext {
    SemanticContext {
        prior_functions: ctx.prior_functions.clone(),
        new_functions: ctx.new_functions.clone(),
        changed_paths: ctx.changed_paths.clone(),
        changed_symbols: ctx.changed_symbols.clone(),
        corpus_complete: ctx.corpus_complete,
    }
}

#[cfg(test)]
mod tests {
    // Ported from `repo`'s capture-path tests: the pipeline now lives here,
    // so the end-to-end assertions register the computer explicitly.

    use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

    use objects::{
        object::{Attribution, ContentHash, Principal, RiskSignalBlob, RiskSignalKind, State},
        store::ObjectStore,
    };
    use repo::{
        CORPUS_FILE_BUDGET, CaptureSemanticContext, Repository, StateAttachmentKind,
        build_semantic_context,
    };
    use tempfile::TempDir;

    use super::*;

    fn registered(repo: Repository) -> Repository {
        repo.set_signal_computer(Arc::new(CaptureSignalComputer));
        repo
    }

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
        let repo = registered(Repository::init_default(temp.path()).unwrap());
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
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let state = snapshot(&repo, temp.path(), "changed.rs", CORPUS);

        let kinds = load_risk_kinds(&repo, &state);
        assert!(
            kinds.contains(&RiskSignalKind::Novelty),
            "novel-shape capture must persist novelty, got {kinds:?}"
        );

        let index_hash = attachment_hash(&repo, &state, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, None, &state, index_hash.as_ref(), None, None).unwrap();
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
        let repo = registered(Repository::init_default(temp.path()).unwrap());
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
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        assert!(ctx.corpus_complete);
        assert_eq!(ctx.changed_symbols.len(), 1);
        assert!(
            changed_identities(&ctx, "changed.rs")
                .iter()
                .any(|id| id.contains("delta")),
            "only delta should change: {:?}",
            ctx.changed_symbols
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

    /// Merkle digest moves (new type / const / use) but no function body or
    /// name changes. Emit-scope is `changed_symbols`; empty means persist
    /// zero sibling tree-sitter signals.
    #[test]
    fn non_function_edit_persists_no_sibling_tree_sitter_signals() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let seed = snapshot(&repo, temp.path(), "changed.rs", CORPUS);
        let edited = snapshot(
            &repo,
            temp.path(),
            "changed.rs",
            "\
    use std::fmt;
    const MARKER: i32 = 0;
    struct Marker;
    fn alpha() { let total = first + second + third + fourth; }
    fn beta() { for widget in inventory { ship(widget); } }
    fn gamma() { match colour { Red => stop(), Green => go() } }
    fn delta() { while pending { dequeue().handle(); } flush(); }
    ",
        );

        let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        assert!(
            ctx.changed_paths.contains(&PathBuf::from("changed.rs")),
            "non-function edit must remain in changed_paths: {ctx:?}"
        );
        assert!(
            ctx.changed_symbols.is_empty(),
            "function bodies unchanged so changed_symbols must be empty: {ctx:?}"
        );
        assert!(
            load_risk_kinds(&repo, &edited).iter().all(|kind| !matches!(
                kind,
                RiskSignalKind::Novelty
                    | RiskSignalKind::PatternDeviation
                    | RiskSignalKind::TestReachability
            )),
            "non-function edit must persist zero sibling tree-sitter signals"
        );
    }

    const TESTS: &str = "\
    fn test_one() { assert!(true); }
    fn test_two() { assert!(true); }
    fn test_three() { assert!(true); }
    ";

    #[test]
    fn object_literal_methods_do_not_poison_the_corpus() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        std::fs::write(
            temp.path().join("handlers.js"),
            "export const handlers = {\n  save: async () => { persist(); },\n};\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("changed.rs"), CORPUS).unwrap();
        let seed = repo
            .snapshot_with_attribution(Some("seed".to_string()), None, author())
            .unwrap();
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
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        assert!(
            ctx.corpus_complete,
            "object-literal methods must not fail-close the corpus"
        );
        let js_fns = ctx
            .new_functions
            .get(&PathBuf::from("handlers.js"))
            .expect("handlers.js must join the new-state corpus");
        assert!(
            js_fns.iter().any(|f| f.name == "save"),
            "object-literal save must be extracted: {js_fns:?}"
        );
        assert!(
            load_risk_kinds(&repo, &edited).contains(&RiskSignalKind::Novelty),
            "complete corpus must still allow novelty on a novel rust shape"
        );
    }

    #[test]
    fn unchanged_tests_join_corpus_and_fire_reachability() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
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
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
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
        assert_eq!(ctx.changed_symbols.len(), 1);
        assert!(
            changed_identities(&ctx, "lib.rs")
                .iter()
                .any(|id| id.contains("orphan")),
            "only orphan should change: {:?}",
            ctx.changed_symbols
        );
        assert!(
            load_risk_kinds(&repo, &edited).contains(&RiskSignalKind::TestReachability),
            "corpus must see unchanged tests so reachability can fire"
        );
    }

    #[test]
    fn more_than_thirty_two_function_files_can_complete_the_corpus() {
        let count = 40;
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        for index in 0..count {
            std::fs::write(
                temp.path().join(format!("f{index}.rs")),
                format!("fn f{index}() {{ {index} }}\n"),
            )
            .unwrap();
        }
        let state = repo
            .snapshot_with_attribution(Some("wide-ok".to_string()), None, author())
            .unwrap();
        let index_hash = attachment_hash(&repo, &state, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, None, &state, index_hash.as_ref(), None, None).unwrap();
        assert!(
            ctx.corpus_complete,
            "40 function files are inside the shared page and must complete"
        );
        assert_eq!(
            ctx.new_functions.len(),
            count,
            "all function files must join the corpus: {}",
            ctx.new_functions.len()
        );
    }

    #[test]
    fn wide_changed_path_parse_marks_corpus_incomplete_when_file_budget_exceeded() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        for index in 0..=CORPUS_FILE_BUDGET {
            std::fs::write(
                temp.path().join(format!("f{index}.rs")),
                format!("fn f{index}() {{ {index} }}\n"),
            )
            .unwrap();
        }
        let state = repo
            .snapshot_with_attribution(Some("wide".to_string()), None, author())
            .unwrap();
        let index_hash = attachment_hash(&repo, &state, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, None, &state, index_hash.as_ref(), None, None).unwrap();
        assert!(
            !ctx.corpus_complete,
            "parsing more than {CORPUS_FILE_BUDGET} changed files must mark the corpus incomplete"
        );
        assert!(
            ctx.new_functions.len() <= CORPUS_FILE_BUDGET,
            "budget must cap the new-state corpus: {}",
            ctx.new_functions.len()
        );
    }

    #[test]
    fn duplicate_name_edit_marks_only_the_qualified_identity() {
        let seed_src = "\
    impl Foo {
        fn run() { let total = first + second + third + fourth; }
    }
    impl Bar {
        fn run() { for widget in inventory { ship(widget); } }
    }
    fn gamma() { match colour { Red => stop(), Green => go() } }
    fn delta() { while pending { dequeue().handle(); } flush(); }
    ";
        let edited_src = "\
    impl Foo {
        fn run() { if ready { launch(); } else { wait(); } abort(); }
    }
    impl Bar {
        fn run() { for widget in inventory { ship(widget); } }
    }
    fn gamma() { match colour { Red => stop(), Green => go() } }
    fn delta() { while pending { dequeue().handle(); } flush(); }
    ";
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let seed = snapshot(&repo, temp.path(), "dup.rs", seed_src);
        let edited = snapshot(&repo, temp.path(), "dup.rs", edited_src);
        let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        let identities = changed_identities(&ctx, "dup.rs");
        assert_eq!(
            identities.len(),
            1,
            "expected one changed symbol: {identities:?}"
        );
        assert!(
            identities
                .iter()
                .any(|id| id.contains("Foo") && id.contains("run")),
            "edited Foo::run must be the emit target: {identities:?}"
        );
        assert!(
            identities.iter().all(|id| !id.contains("Bar")),
            "untouched Bar::run must not be an emit target: {identities:?}"
        );
    }

    #[test]
    fn mass_delete_skips_prior_parse_and_does_not_blow_the_budget() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        for index in 0..=CORPUS_FILE_BUDGET {
            std::fs::write(
                temp.path().join(format!("gone{index}.rs")),
                format!("fn gone{index}() {{ {index} }}\n"),
            )
            .unwrap();
        }
        std::fs::write(temp.path().join("keep.rs"), "fn keep() { 0 }\n").unwrap();
        let seed = repo
            .snapshot_with_attribution(Some("seed".to_string()), None, author())
            .unwrap();
        for index in 0..=CORPUS_FILE_BUDGET {
            std::fs::remove_file(temp.path().join(format!("gone{index}.rs"))).unwrap();
        }
        let edited = repo
            .snapshot_with_attribution(Some("delete".to_string()), None, author())
            .unwrap();
        let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        assert!(
            ctx.prior_functions
                .keys()
                .all(|path| path == &PathBuf::from("keep.rs")),
            "deleted paths must not be prior-parsed: {:?}",
            ctx.prior_functions.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.corpus_complete,
            "deletes must not exhaust the new-state corpus budget"
        );
    }

    #[test]
    fn content_preserving_rename_marks_no_changed_symbols() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let seed = snapshot(&repo, temp.path(), "old.rs", CORPUS);
        std::fs::write(temp.path().join("new.rs"), CORPUS).unwrap();
        std::fs::remove_file(temp.path().join("old.rs")).unwrap();
        let edited = repo
            .snapshot_with_attribution(Some("rename".to_string()), None, author())
            .unwrap();
        let index_hash = attachment_hash(&repo, &edited, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, Some(&seed), &edited, index_hash.as_ref(), None, None)
                .unwrap();
        assert!(
            ctx.changed_paths.contains(&PathBuf::from("new.rs")),
            "rename destination stays in changed_paths: {ctx:?}"
        );
        assert!(
            changed_identities(&ctx, "new.rs").is_empty(),
            "exact blob rename must not mark destination functions changed: {:?}",
            ctx.changed_symbols
        );
    }

    fn changed_identities(ctx: &CaptureSemanticContext, path: &str) -> BTreeSet<String> {
        ctx.changed_symbols
            .iter()
            .filter(|(changed_path, _)| changed_path == &PathBuf::from(path))
            .map(|(_, identity)| identity.clone())
            .collect()
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

    /// Snapshotting a state whose intent carries a `self-flag:` line should
    /// land a `RiskSignalBlob` on the resulting state. Picks the
    /// self-flagged-uncertainty module specifically because it's the one
    /// signal that fires from state-only metadata (no parsed-file context),
    /// so this test exercises the wiring without requiring a tree-sitter
    /// fixture corpus.
    #[test]
    fn snapshot_attaches_risk_signals_when_signal_fires() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());

        // Write a tiny file so the snapshot has something to track and
        // mirrors a realistic capture.
        std::fs::write(temp.path().join("hello.txt"), b"hi").unwrap();

        let attribution = Attribution::human(Principal::new("Alice", "alice@example.com"));
        let intent = "feat: rewrote auth\nself-flag:[src/auth.rs:verify] uncertain about edge case";
        let state = repo
            .snapshot_with_attribution(Some(intent.to_string()), None, attribution)
            .unwrap();

        let hash = repo
            .latest_state_attachment(&state.id(), StateAttachmentKind::RiskSignals)
            .unwrap()
            .and_then(|attachment| match attachment.body {
                objects::object::StateAttachmentBody::RiskSignals(hash) => Some(hash),
                _ => None,
            })
            .expect("snapshot should attach risk_signals when a self-flag fires");
        let blob = repo
            .store()
            .get_blob(&hash)
            .unwrap()
            .expect("risk signals blob persisted");
        let parsed = RiskSignalBlob::decode(blob.content()).unwrap();
        assert_eq!(parsed.signals.len(), 1, "exactly one self-flag signal");
        let sig = &parsed.signals[0];
        assert_eq!(sig.producer.module, "self_flagged_uncertainty");
        assert_eq!(sig.anchor.file, "src/auth.rs");
        assert_eq!(sig.anchor.symbol.as_deref(), Some("verify"));
    }

    /// A snapshot whose intent has no flags and whose tree is too tiny to
    /// trip novelty/pattern-deviation should leave `risk_signals = None` —
    /// we never write an empty blob.
    #[test]
    fn snapshot_leaves_risk_signals_none_when_quiet() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        std::fs::write(temp.path().join("hello.txt"), b"hi").unwrap();

        let attribution = Attribution::human(Principal::new("Bob", "bob@example.com"));
        let state = repo
            .snapshot_with_attribution(Some("plain capture".to_string()), None, attribution)
            .unwrap();

        assert!(
            repo.latest_state_attachment(&state.id(), StateAttachmentKind::RiskSignals)
                .unwrap()
                .is_none()
        );
    }

    fn attached_risk_modules(repo: &Repository, state: &objects::object::State) -> Vec<String> {
        let Some(attachment) = repo
            .latest_state_attachment(&state.id(), StateAttachmentKind::RiskSignals)
            .unwrap()
        else {
            return Vec::new();
        };
        let objects::object::StateAttachmentBody::RiskSignals(hash) = attachment.body else {
            panic!("expected risk-signals attachment");
        };
        let blob = repo.store().get_blob(&hash).unwrap().unwrap();
        RiskSignalBlob::decode(blob.content())
            .unwrap()
            .signals
            .into_iter()
            .map(|signal| signal.producer.module)
            .collect()
    }

    #[test]
    fn snapshot_fmt_sweep_persists_no_tree_sitter_signals() {
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let attribution = Attribution::human(Principal::new("Fmt", "fmt@example.com"));
        std::fs::write(temp.path().join("hello.rs"), "fn foo() -> i32 { 1 }\n").unwrap();
        repo.snapshot_with_attribution(Some("seed".to_string()), None, attribution.clone())
            .unwrap();
        std::fs::write(
            temp.path().join("hello.rs"),
            "fn foo() -> i32 {\n    1\n}\n",
        )
        .unwrap();
        let reformatted = repo
            .snapshot_with_attribution(Some("fmt".to_string()), None, attribution)
            .unwrap();

        assert!(
            attached_risk_modules(&repo, &reformatted)
                .iter()
                .all(|module| !module.contains("tree_sitter")),
            "fmt-sweep must not persist tree-sitter signals: {:?}",
            attached_risk_modules(&repo, &reformatted)
        );
    }

    #[test]
    fn snapshot_novel_shape_persists_novelty() {
        // Native SnapshotSource::Worktree: new.tree is not in repo.store()
        // until stage_snapshot_objects returns. Overlay maps must resolve it.
        let temp = TempDir::new().unwrap();
        let repo = registered(Repository::init_default(temp.path()).unwrap());
        let attribution = Attribution::human(Principal::new("Nov", "nov@example.com"));
        std::fs::write(
            temp.path().join("changed.rs"),
            "fn alpha() { let total = first + second + third + fourth; }\n\
             fn beta() { for widget in inventory { ship(widget); } }\n\
             fn gamma() { match colour { Red => stop(), Green => go() } }\n\
             fn delta() { while pending { dequeue().handle(); } flush(); }\n",
        )
        .unwrap();
        let state = repo
            .snapshot_with_attribution(Some("novel".to_string()), None, attribution)
            .unwrap();

        let modules = attached_risk_modules(&repo, &state);
        assert!(
            modules.iter().any(|module| module == "novelty.tree_sitter"),
            "worktree capture must persist novelty, got {modules:?}"
        );
    }

    #[test]
    fn tree_sitter_producers_enabled_is_false_when_all_disabled() {
        let mut cfg = crate::ReviewSignalsConfig::default();
        cfg.novelty.enabled = false;
        cfg.test_reachability.enabled = false;
        cfg.pattern_deviation.enabled = false;
        assert!(!tree_sitter_producers_enabled(&cfg));
        cfg.novelty.enabled = true;
        assert!(tree_sitter_producers_enabled(&cfg));
    }
}
