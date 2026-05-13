// SPDX-License-Identifier: Apache-2.0
//! Render-time tick budgeting.
//!
//! Reviewers see at most three signals per state by default. The budgeter
//! takes the full fired-signals set and partitions it into a visible head
//! and a hidden tail. Priority order — fixed and load-bearing:
//!
//! 1. invariant_adjacency
//! 2. self_flagged_uncertainty
//! 3. pattern_deviation
//! 4. novelty
//! 5. test_reachability
//!
//! When two signals fire on the same anchor, the highest-priority one
//! wins by default; the others remain in [`BudgetedSignals::hidden`] so a
//! caller passing `--all-signals` can surface them.

use objects::object::{RiskSignal, RiskSignalKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetConfig {
    /// Maximum signals to surface in the visible tier. Default 3.
    pub max_visible: u8,
    /// When false (the default) only the highest-priority signal per
    /// anchor is visible; remaining signals on the same anchor go to
    /// `hidden`.
    pub allow_multiple_per_anchor: bool,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_visible: 3,
            allow_multiple_per_anchor: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetedSignals {
    pub visible: Vec<RiskSignal>,
    pub hidden: Vec<RiskSignal>,
}

/// Pure: takes ownership of `all`, returns the partition. No allocations
/// beyond the two output `Vec`s and the sort buffer.
pub fn budget(mut all: Vec<RiskSignal>, cfg: &BudgetConfig) -> BudgetedSignals {
    // Stable sort by (priority_rank, anchor canonical). Stable sorting
    // means signals from the same module on different anchors keep their
    // input order, which makes the output deterministic for tests.
    all.sort_by(|a, b| {
        priority_rank(a.kind)
            .cmp(&priority_rank(b.kind))
            .then_with(|| a.anchor.canonical().cmp(&b.anchor.canonical()))
    });

    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    let max = cfg.max_visible as usize;
    let mut seen_anchors: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sig in all {
        let key = sig.anchor.canonical();
        let already_filled_anchor = !cfg.allow_multiple_per_anchor && seen_anchors.contains(&key);
        if visible.len() < max && !already_filled_anchor {
            seen_anchors.insert(key);
            visible.push(sig);
        } else {
            hidden.push(sig);
        }
    }
    BudgetedSignals { visible, hidden }
}

/// Render-time priority rank. Mirror of [`RiskSignalKind::priority_rank`]
/// but kept in the budgeter as the source of truth — changing the order
/// here is the canonical place to bump priorities.
fn priority_rank(kind: RiskSignalKind) -> u8 {
    match kind {
        RiskSignalKind::InvariantAdjacency => 0,
        RiskSignalKind::SelfFlaggedUncertainty => 1,
        RiskSignalKind::PatternDeviation => 2,
        RiskSignalKind::Novelty => 3,
        RiskSignalKind::TestReachability => 4,
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{ProducerId, SignalAnchor};

    use super::*;

    fn sig(kind: RiskSignalKind, file: &str, sym: &str) -> RiskSignal {
        RiskSignal {
            kind,
            anchor: SignalAnchor::symbol(file, sym),
            reason: format!("reason for {}", kind.as_str()),
            producer: ProducerId::new(kind.as_str(), 1),
            computed_at: 0,
            computed_against: None,
        }
    }

    #[test]
    fn budget_priority_orders_visible_correctly() {
        let signals = vec![
            sig(RiskSignalKind::TestReachability, "a.rs", "z"),
            sig(RiskSignalKind::InvariantAdjacency, "a.rs", "y"),
            sig(RiskSignalKind::Novelty, "a.rs", "x"),
            sig(RiskSignalKind::PatternDeviation, "a.rs", "w"),
            sig(RiskSignalKind::SelfFlaggedUncertainty, "a.rs", "v"),
        ];
        let result = budget(signals, &BudgetConfig::default());
        assert_eq!(result.visible.len(), 3);
        assert_eq!(result.visible[0].kind, RiskSignalKind::InvariantAdjacency);
        assert_eq!(
            result.visible[1].kind,
            RiskSignalKind::SelfFlaggedUncertainty
        );
        assert_eq!(result.visible[2].kind, RiskSignalKind::PatternDeviation);
        assert_eq!(result.hidden.len(), 2);
        assert_eq!(result.hidden[0].kind, RiskSignalKind::Novelty);
        assert_eq!(result.hidden[1].kind, RiskSignalKind::TestReachability);
    }

    #[test]
    fn budget_collapses_same_anchor_to_highest_priority() {
        let signals = vec![
            sig(RiskSignalKind::Novelty, "a.rs", "foo"),
            sig(RiskSignalKind::InvariantAdjacency, "a.rs", "foo"),
        ];
        let result = budget(signals, &BudgetConfig::default());
        assert_eq!(result.visible.len(), 1);
        assert_eq!(result.visible[0].kind, RiskSignalKind::InvariantAdjacency);
        assert_eq!(result.hidden.len(), 1);
        assert_eq!(result.hidden[0].kind, RiskSignalKind::Novelty);
    }

    #[test]
    fn budget_allow_multiple_per_anchor_keeps_both() {
        let signals = vec![
            sig(RiskSignalKind::Novelty, "a.rs", "foo"),
            sig(RiskSignalKind::InvariantAdjacency, "a.rs", "foo"),
        ];
        let cfg = BudgetConfig {
            max_visible: 3,
            allow_multiple_per_anchor: true,
        };
        let result = budget(signals, &cfg);
        assert_eq!(result.visible.len(), 2);
    }

    #[test]
    fn budget_max_visible_one_truncates_to_one() {
        let signals = vec![
            sig(RiskSignalKind::InvariantAdjacency, "a.rs", "y"),
            sig(RiskSignalKind::SelfFlaggedUncertainty, "b.rs", "y"),
            sig(RiskSignalKind::Novelty, "c.rs", "y"),
        ];
        let cfg = BudgetConfig {
            max_visible: 1,
            allow_multiple_per_anchor: false,
        };
        let result = budget(signals, &cfg);
        assert_eq!(result.visible.len(), 1);
        assert_eq!(result.hidden.len(), 2);
    }

    #[test]
    fn budget_empty_input_returns_empty_partition() {
        let result = budget(Vec::new(), &BudgetConfig::default());
        assert!(result.visible.is_empty());
        assert!(result.hidden.is_empty());
    }
}