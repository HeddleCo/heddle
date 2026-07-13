// SPDX-License-Identifier: Apache-2.0
//! Trait + dispatch registry for the five risk-signal modules.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use objects::object::{RiskSignal, State};
use semantic::parser::FunctionDef;

use crate::config::ReviewSignalsConfig;

/// Bundle of pre-extracted function lists per file, keyed by repo-relative
/// path. The caller parses + extracts once per review pass and shares this
/// across modules so tree-sitter work is amortised. An empty context is
/// valid — a module that needs parsed sources but finds none MUST stay
/// quiet rather than failing.
///
/// We hold extracted [`FunctionDef`]s rather than the parser's `ParsedFile`
/// because `ParsedFile` owns a `TSTree` which isn't `Clone`/`Send`-friendly
/// for sharing across modules.
#[derive(Debug, Default, Clone)]
pub struct SemanticContext {
    pub prior_functions: BTreeMap<PathBuf, Vec<FunctionDef>>,
    pub new_functions: BTreeMap<PathBuf, Vec<FunctionDef>>,
    /// Repo-relative paths that actually changed in this review pass. Modules
    /// that should only report on the diff (e.g. novelty) scope their emitted
    /// signals to this set while still comparing against the full
    /// `new_functions` corpus. Empty means "no changed files known" — such a
    /// module stays quiet rather than scanning the whole repo.
    pub changed_paths: BTreeSet<PathBuf>,
}

impl SemanticContext {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Module dispatcher. Static array so we can iterate modules in priority
/// order without virtual dispatch.
pub const ALL_MODULES: &[ComputeFn] = &[
    crate::modules::invariant_adjacency::run,
    crate::modules::self_flagged_uncertainty::run,
    crate::modules::pattern_deviation::run,
    crate::modules::novelty::run,
    crate::modules::test_reachability::run,
];

pub type ComputeFn = fn(&State, &State, &ReviewSignalsConfig, &SemanticContext) -> Vec<RiskSignal>;

/// Run every registered module and return the union of fired signals.
/// Output is sorted by `(producer.module, anchor.canonical())` for
/// deterministic ordering — important for golden-file tests in R7.
pub fn run_all(
    prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    let mut signals = Vec::new();
    for module in ALL_MODULES {
        signals.extend(module(prior, new, cfg, ctx));
    }
    signals.sort_by(|a, b| {
        a.producer
            .module
            .cmp(&b.producer.module)
            .then_with(|| a.anchor.canonical().cmp(&b.anchor.canonical()))
    });
    signals
}

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, ChangeId, ContentHash, Principal};

    use super::*;

    fn empty_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
    }

    fn state_with_change_id(byte: u8) -> State {
        empty_state().with_change_id(ChangeId::from_bytes([byte; 16]))
    }

    fn fdef(name: &str, content: &str) -> FunctionDef {
        FunctionDef {
            name: name.to_string(),
            signature: format!("fn {name}()"),
            start_line: 1,
            end_line: 3,
            content: content.to_string(),
        }
    }

    #[test]
    fn run_all_with_no_signals_returns_empty() {
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run_all(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn module_count_matches_priority_order() {
        // Five modules, fixed: invariant > self-flagged > pattern > novelty > tests.
        // Adding a sixth module must update budgeting's priority array
        // simultaneously; the const_assert in `budget.rs` enforces that.
        assert_eq!(ALL_MODULES.len(), 5);
    }

    #[test]
    fn run_all_returns_deterministic_order_with_computed_against() {
        let prior = state_with_change_id(1);
        let new = state_with_change_id(2)
            .with_intent("self-flag:[z.rs:zeta] late anchor\nself-flag:[a.rs:alpha] early anchor");
        let mut cfg = ReviewSignalsConfig::default();
        cfg.novelty.enabled = false;
        cfg.test_reachability.enabled = false;
        cfg.pattern_deviation.threshold = 0.2;

        let mut ctx = SemanticContext::new();
        ctx.prior_functions.insert(
            PathBuf::from("src/scoring.rs"),
            vec![fdef(
                "score",
                "fn score(value: usize) -> usize { value + 1 }",
            )],
        );
        ctx.new_functions.insert(
            PathBuf::from("src/scoring.rs"),
            vec![fdef(
                "score",
                "fn score(socket: &mut Socket) -> usize { while socket.poll() { rotate_key(); } 0 }",
            )],
        );

        let signals = run_all(&prior, &new, &cfg, &ctx);
        let ordering: Vec<(String, String)> = signals
            .iter()
            .map(|signal| (signal.producer.module.clone(), signal.anchor.canonical()))
            .collect();

        assert_eq!(
            ordering,
            vec![
                (
                    "pattern_deviation.tree_sitter".to_string(),
                    "src/scoring.rs:score".to_string(),
                ),
                (
                    "self_flagged_uncertainty".to_string(),
                    "a.rs:alpha".to_string(),
                ),
                (
                    "self_flagged_uncertainty".to_string(),
                    "z.rs:zeta".to_string(),
                ),
            ],
            "run_all should sort by producer module then canonical anchor"
        );
        assert!(
            signals
                .iter()
                .all(|signal| signal.computed_against == Some(new.state_id)),
            "all registered modules should stamp computed_against on emitted signals: {signals:?}"
        );
    }
}
