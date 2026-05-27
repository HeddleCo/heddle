// SPDX-License-Identifier: Apache-2.0
//! Risk-signal computation hookpoint, called from the snapshot path.
//!
//! Lives in its own file so the `cfg(feature = "tree-sitter-symbols")` guard
//! sits at the module boundary rather than scattered across `repository.rs`.
//! The actual signal modules live in `crates/state_review/`; this layer
//! mediates between `Repository`'s already-built `State` and the registry,
//! persisting any fired signals as a `RiskSignalBlob` and returning the
//! attached hash for `state.with_risk_signals(...)`.
//!
//! Errors are intentionally swallowed (with a `tracing::warn`) — capture must
//! never fail because of a signal hiccup.

#![cfg(feature = "tree-sitter-symbols")]

use objects::object::{Blob, ContentHash, RiskSignalBlob, State};
use state_review::{
    ReviewSignalsConfig, SemanticContext,
    config::{
        InvariantAdjacencyConfig, NoveltyConfig, PatternDeviationConfig,
        SelfFlaggedUncertaintyConfig, TestReachabilityConfig,
    },
    registry::run_all,
};
use tracing::warn;

use crate::{Repository, Result, repository::ReviewSignalsToml};

impl Repository {
    /// Run the signal registry against a freshly-built `(prior, new)`
    /// pair, encode any fired signals as a `RiskSignalBlob`, and return
    /// the persisted hash so the snapshot path can chain
    /// `state.with_risk_signals(...)` before `put_state`.
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
    ) -> Result<Option<ContentHash>> {
        let cfg = signals_config_from_repo(&self.config().review.signals);
        // The default empty `SemanticContext` covers self-flagged-uncertainty
        // and invariant-adjacency, both of which only need state-level
        // metadata. Tree-sitter-driven modules (novelty, test-reachability,
        // pattern-deviation) stay quiet rather than failing — that's a
        // first-ship trade-off, not a permanent one. Follow-on work
        // populates `SemanticContext` from the snapshot's tree.
        let ctx = SemanticContext::new();
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

        let hash = state
            .risk_signals
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
            state.risk_signals.is_none(),
            "no signals fire on a quiet capture; expected None, got {:?}",
            state.risk_signals
        );
    }
}
