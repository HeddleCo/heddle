// SPDX-License-Identifier: Apache-2.0
//! Repo-level signal health: per-module fire rate over a rolling window.
//!
//! Pure: takes a slice of [`StateSignalSnapshot`] (the caller is
//! responsible for fetching them from the oplog/object store) and returns
//! one [`SignalHealth`] per producer module that fired at least once.
//!
//! "Fire rate" = states-where-this-module-fired / states-considered. A
//! module that fires more than [`compute_health`]'s `warn_threshold` of
//! the time is flagged for tuning.

use std::collections::{BTreeMap, BTreeSet};

/// One state's signal payload as seen by the health module. Just the
/// producer module ids that fired — no anchor or reason needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSignalSnapshot {
    pub state_id: String,
    pub modules_fired: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignalHealth {
    pub module_id: String,
    pub fire_rate: f32,
    pub window_states: u32,
    pub warn: bool,
}

/// Compute per-module fire rate. `warn_threshold` is in `0.0..=1.0`.
pub fn compute_health(
    history: &[StateSignalSnapshot],
    window: usize,
    warn_threshold: f32,
) -> Vec<SignalHealth> {
    let slice: &[StateSignalSnapshot] = if history.len() > window {
        &history[history.len() - window..]
    } else {
        history
    };
    let visited = slice.len() as u32;
    let mut hit_count: BTreeMap<String, u32> = BTreeMap::new();
    for snapshot in slice {
        for module_id in &snapshot.modules_fired {
            *hit_count.entry(module_id.clone()).or_insert(0) += 1;
        }
    }
    hit_count
        .into_iter()
        .map(|(module_id, hits)| {
            let fire_rate = if visited == 0 {
                0.0
            } else {
                hits as f32 / visited as f32
            };
            SignalHealth {
                module_id,
                fire_rate,
                window_states: visited,
                warn: fire_rate > warn_threshold,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(state_id: &str, modules: &[&str]) -> StateSignalSnapshot {
        StateSignalSnapshot {
            state_id: state_id.to_string(),
            modules_fired: modules.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn fire_rate_above_threshold_warns() {
        let history = vec![
            snap("a", &["novelty"]),
            snap("b", &["novelty"]),
            snap("c", &["novelty"]),
            snap("d", &[]),
        ];
        let report = compute_health(&history, 100, 0.5);
        let novelty = report.iter().find(|e| e.module_id == "novelty").unwrap();
        assert_eq!(novelty.fire_rate, 0.75);
        assert!(novelty.warn);
    }

    #[test]
    fn empty_history_yields_no_entries() {
        let report = compute_health(&[], 100, 0.5);
        assert!(report.is_empty());
    }

    #[test]
    fn window_truncates_to_most_recent() {
        let history = vec![
            snap("a", &["novelty"]),
            snap("b", &["novelty"]),
            snap("c", &["novelty"]),
            snap("d", &["test_reachability"]),
            snap("e", &["test_reachability"]),
        ];
        let report = compute_health(&history, 2, 0.5);
        // Only the last two states are considered.
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].module_id, "test_reachability");
        assert_eq!(report[0].fire_rate, 1.0);
    }

    #[test]
    fn fire_rate_at_or_below_threshold_does_not_warn() {
        let history = vec![snap("a", &["novelty"]), snap("b", &[])];
        let report = compute_health(&history, 100, 0.5);
        let novelty = report.iter().find(|e| e.module_id == "novelty").unwrap();
        assert_eq!(novelty.fire_rate, 0.5);
        assert!(!novelty.warn, "0.5 is not > 0.5");
    }
}