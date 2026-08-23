// SPDX-License-Identifier: Apache-2.0
//! Risk-signal computation hookpoint, called from the snapshot path.
//!
//! Lives in its own file so the `cfg(feature = "tree-sitter-symbols")` guard
//! sits at the module boundary rather than scattered across `repository.rs`.
//! The actual signal modules live in `crates/state_review/`; this layer
//! mediates between `Repository`'s already-built `State` and the registry,
//! persisting any fired signals as a `RiskSignalBlob` for attachment after
//! the immutable state is stored.
//!
//! Errors are intentionally swallowed (with a `tracing::warn`) — capture must
//! never fail because of a signal hiccup.

#![cfg(feature = "tree-sitter-symbols")]

use objects::{
    object::{Blob, ContentHash, RiskSignalBlob, State},
    store::ObjectStore,
};
use state_review::{
    ReviewSignalsConfig,
    config::{
        InvariantAdjacencyConfig, NoveltyConfig, PatternDeviationConfig,
        SelfFlaggedUncertaintyConfig, TestReachabilityConfig,
    },
    registry::run_all,
};
use tracing::warn;

use crate::repository_semantic_context::build_semantic_context;

use crate::{Repository, Result, repository::ReviewSignalsToml};

impl Repository {
    /// Run the signal registry against a freshly-built `(prior, new)`
    /// pair, encode any fired signals as a `RiskSignalBlob`, and return
    /// the persisted hash so the snapshot path can attach it to the state.
    ///
    /// `Ok(None)` covers the two should-skip cases:
    /// - Registry fired no signals (avoid an empty blob — keeps the on-disk
    ///   shape identical to "feature off" for unaffected captures).
    /// - Anything went wrong encoding/persisting the blob (logged, never
    ///   propagated — capture wins).
    pub(crate) fn compute_and_persist_signals(
        &self,
        prior: Option<&State>,
        new: &State,
        new_index: Option<&ContentHash>,
        source_blobs: Option<&std::collections::HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&std::collections::HashMap<ContentHash, &objects::object::Tree>>,
    ) -> Result<Option<ContentHash>> {
        let cfg = signals_config_from_repo(&self.config().review.signals);
        let ctx = if tree_sitter_producers_enabled(&cfg) {
            match build_semantic_context(self, prior, new, new_index, source_blobs, source_trees) {
                Ok(ctx) => ctx,
                Err(err) => {
                    warn!(
                        error = %err,
                        "failed to build SemanticContext; running state-only signals"
                    );
                    state_review::SemanticContext::new()
                }
            }
        } else {
            state_review::SemanticContext::new()
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
            Ok(bytes) => match self.store().put_blob(&Blob::new(bytes)) {
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

/// Map the TOML-shaped repo config into the `state_review` crate's typed
/// config. Kept as a free function so tests can exercise it without spinning
/// up a `Repository`.
pub(crate) fn signals_config_from_repo(t: &ReviewSignalsToml) -> ReviewSignalsConfig {
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

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, Principal, RiskSignalBlob};
    use state_review::ReviewSignalsConfig;
    use tempfile::TempDir;

    use super::*;

    /// Snapshotting a state whose intent carries a `self-flag:` line should
    /// land a `RiskSignalBlob` on the resulting state. Picks the
    /// self-flagged-uncertainty module specifically because it's the one
    /// signal that fires from state-only metadata (no parsed-file context),
    /// so this test exercises the wiring without requiring a tree-sitter
    /// fixture corpus.
    #[test]
    fn snapshot_attaches_risk_signals_when_signal_fires() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        // Write a tiny file so the snapshot has something to track and
        // mirrors a realistic capture.
        std::fs::write(temp.path().join("hello.txt"), b"hi").unwrap();

        let attribution = Attribution::human(Principal::new("Alice", "alice@example.com"));
        let intent = "feat: rewrote auth\nself-flag:[src/auth.rs:verify] uncertain about edge case";
        let state = repo
            .snapshot_with_attribution(Some(intent.to_string()), None, attribution)
            .unwrap();

        let hash = repo
            .latest_state_attachment(&state.id(), crate::StateAttachmentKind::RiskSignals)
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
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("hello.txt"), b"hi").unwrap();

        let attribution = Attribution::human(Principal::new("Bob", "bob@example.com"));
        let state = repo
            .snapshot_with_attribution(Some("plain capture".to_string()), None, attribution)
            .unwrap();

        assert!(
            repo.latest_state_attachment(&state.id(), crate::StateAttachmentKind::RiskSignals)
                .unwrap()
                .is_none()
        );
    }

    fn attached_risk_modules(repo: &Repository, state: &objects::object::State) -> Vec<String> {
        let Some(attachment) = repo
            .latest_state_attachment(&state.id(), crate::StateAttachmentKind::RiskSignals)
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
        let repo = Repository::init_default(temp.path()).unwrap();
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
        let repo = Repository::init_default(temp.path()).unwrap();
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
        let mut cfg = ReviewSignalsConfig::default();
        cfg.novelty.enabled = false;
        cfg.test_reachability.enabled = false;
        cfg.pattern_deviation.enabled = false;
        assert!(!tree_sitter_producers_enabled(&cfg));
        cfg.novelty.enabled = true;
        assert!(tree_sitter_producers_enabled(&cfg));
    }

    /// When a state has an invariant annotation attached to its context,
    /// `compute_and_persist_signals` must fire an `invariant_adjacency` signal.
    /// This was previously a permanent no-op: `ctx_annotations` returned the
    /// hard-wired `EMPTY` static regardless of what `SemanticContext` carried.
    #[test]
    fn snapshot_with_invariant_annotation_persists_invariant_adjacency_signal() {
        use chrono::Utc;
        use objects::object::{
            Annotation, AnnotationKind, AnnotationScope, ContextBlob,
            ContextTarget, StateAttachment, StateAttachmentBody,
        };

        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("src.rs"), b"fn guarded() {}").unwrap();
        let attribution = Attribution::human(Principal::new("Ann", "ann@example.com"));

        // Take a seed snapshot so there is a prior state.
        let seed = repo
            .snapshot_with_attribution(Some("seed".to_string()), None, attribution.clone())
            .unwrap();

        // Attach an invariant annotation to the seed state, simulating what
        // `heddle context` writes when an agent marks a symbol as invariant.
        let target = ContextTarget::file("src.rs").unwrap();
        let annotation = Annotation::new(
            AnnotationScope::Symbol {
                name: "guarded".to_string(),
                resolved_lines: None,
            },
            AnnotationKind::Invariant,
            "must hold across all operations".to_string(),
            vec![],
            "ann@example.com".to_string(),
            0,
            None,
            None,
        );
        let blob = ContextBlob::new(vec![annotation]);
        let context_root = repo.set_context_blob(None, &target, &blob).unwrap();
        repo.put_state_attachment(&StateAttachment {
            state_id: seed.id(),
            body: StateAttachmentBody::Context(context_root),
            attribution: attribution.clone(),
            created_at: Utc::now(),
            supersedes: None,
        })
        .unwrap();

        // Take a second snapshot — this is the capture that should read the
        // invariant annotation from the seed's context and fire the signal.
        std::fs::write(temp.path().join("src.rs"), b"fn guarded() { changed() }").unwrap();
        let state = repo
            .snapshot_with_attribution(Some("change".to_string()), None, attribution)
            .unwrap();

        let modules = attached_risk_modules(&repo, &state);
        assert!(
            modules.iter().any(|m| m == "invariant_adjacency"),
            "invariant_adjacency must fire when state has an invariant annotation; got {modules:?}"
        );
    }
}
