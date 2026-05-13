// SPDX-License-Identifier: Apache-2.0
//! Trait + dispatch registry for the five risk-signal modules.

use std::{collections::BTreeMap, path::PathBuf};

use objects::object::{RiskSignal, State};
use semantic::parser::FunctionDef;

use crate::config::ReviewSignalsConfig;

/// Stable identifier for a risk-signal module. Used by the budgeter to
/// group results and by the health surface to track per-module fire
/// rates without depending on the module's runtime version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleId {
    Novelty,
    TestReachability,
    PatternDeviation,
    InvariantAdjacency,
    SelfFlaggedUncertainty,
}

impl ModuleId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Novelty => "novelty.tree_sitter",
            Self::TestReachability => "test_reachability.tree_sitter",
            Self::PatternDeviation => "pattern_deviation.tree_sitter",
            Self::InvariantAdjacency => "invariant_adjacency",
            Self::SelfFlaggedUncertainty => "self_flagged_uncertainty",
        }
    }
}

/// Pure-computation contract every signal module implements. The trait
/// uses a free function rather than `&self` so registries can be `static`
/// arrays of fn pointers — see [`ALL_MODULES`].
pub trait RiskSignalModule {
    /// Stable identifier. Used by budgeting and health surfaces.
    const ID: ModuleId;
    /// Wire version of this module's output schema. Bump on breaking
    /// changes to the [`RiskSignal`] shape this module produces.
    const VERSION: u32;

    /// Compute signals for the given `(prior, new)` state transition.
    /// MUST be pure: no I/O, no clock, no env lookups. The `ctx`
    /// argument carries any parsed-file caches the caller wants to
    /// reuse across modules.
    fn compute(
        prior: &State,
        new: &State,
        cfg: &ReviewSignalsConfig,
        ctx: &SemanticContext,
    ) -> Vec<RiskSignal>;
}

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
    use objects::object::{Attribution, ContentHash, Principal};

    use super::*;

    fn empty_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
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
}