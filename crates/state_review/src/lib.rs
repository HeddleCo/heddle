// SPDX-License-Identifier: Apache-2.0
//! Pure risk-signal computation for the Review epic.
//!
//! Five modules are registered as [`ComputeFn`] fn-pointers in
//! [`ALL_MODULES`]: novelty, test reachability, pattern deviation, invariant
//! adjacency, and self-flagged uncertainty. Each receives the prior state, the
//! new state, and a per-repo config; each returns the signals it fired.
//! Computation is pure — no I/O, no clock, no environment lookups — so the
//! same inputs always produce the same output.
//!
//! Render-time tick budgeting is in [`budget`]. Per-repo health metrics
//! (per-signal fire rates) are in [`health`]. The high-level entry point is
//! [`registry::run_all`].

pub mod budget;
pub mod config;
pub mod health;
pub mod modules;
pub mod payload;
pub mod registry;

pub use budget::{BudgetConfig, BudgetedSignals, budget};
pub use config::{
    InvariantAdjacencyConfig, NoveltyConfig, PatternDeviationConfig, ReviewSignalsConfig,
    SelfFlaggedUncertaintyConfig, TestReachabilityConfig,
};
pub use health::{SignalHealth, StateSignalSnapshot, compute_health};
// Re-export the type the modules produce so consumers don't need to
// reach into `objects` directly.
pub use objects::object::{MAX_REASON_LEN, ProducerId, RiskSignal, RiskSignalKind, SignalAnchor};
pub use payload::{PathSymbol, ReadingOrderPartition, SymbolKind, build_review_payload_partition};
pub use registry::{ALL_MODULES, ComputeFn, SemanticContext, run_all};

pub(crate) fn truncate_reason(reason: &str) -> String {
    if reason.len() <= objects::object::MAX_REASON_LEN {
        reason.to_string()
    } else {
        let take = objects::object::MAX_REASON_LEN.saturating_sub(1);
        let mut out: String = reason.chars().take(take).collect();
        out.push('\u{2026}');
        out
    }
}
