// SPDX-License-Identifier: Apache-2.0
//! Context rewrite scoring and low-noise suggestion heuristics.

use std::collections::{BTreeMap, BTreeSet};

use objects::{
    object::{ContextTarget, State},
    store::ObjectStore,
};

use crate::{HistoryQuery, Repository, staleness};

pub const SUGGESTION_WINDOW: usize = 24;
pub const MEDIUM_SUGGESTION_THRESHOLD: u32 = 45;
pub const HIGH_SUGGESTION_THRESHOLD: u32 = 70;
pub const MAJOR_REWRITE_THRESHOLD_PCT: u32 = 50;

const CHANGE_WEIGHT: u32 = 16;
const DISTINCT_STATE_WEIGHT: u32 = 8;
const DISTINCT_AGENT_WEIGHT: u32 = 10;
const RECENCY_WEIGHT: u32 = 12;
const STALE_WEIGHT: u32 = 18;
const HAS_CONTEXT_PENALTY: u32 = 35;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextSuggestionTier {
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextSuggestion {
    pub path: String,
    pub score: u32,
    pub tier: ContextSuggestionTier,
    pub reasons: Vec<String>,
    pub recent_changes: u32,
    pub distinct_states: u32,
    pub distinct_agents: u32,
    pub has_context: bool,
    pub stale_annotations: u32,
}

#[derive(Default)]
struct SuggestionSignal {
    recent_changes: u32,
    distinct_states: BTreeSet<String>,
    distinct_agents: BTreeSet<String>,
    latest_seen_index: Option<usize>,
}

impl Repository {
    pub fn suggest_context_targets(
        &self,
        state: &State,
        limit: usize,
    ) -> Result<Vec<ContextSuggestion>, anyhow::Error> {
        let history = self.collect_state_window(state, SUGGESTION_WINDOW)?;
        let mut signals: BTreeMap<String, SuggestionSignal> = BTreeMap::new();

        for (index, candidate) in history.iter().enumerate() {
            let parent_tree = candidate
                .first_parent()
                .and_then(|parent_id| self.store().get_state(parent_id).ok().flatten())
                .map(|parent| parent.tree);

            let changes = if let Some(parent_tree) = parent_tree {
                self.diff_trees(&parent_tree, &candidate.tree)?
            } else {
                self.diff_trees(&objects::object::Tree::new().hash(), &candidate.tree)?
            };

            for change in changes {
                let signal = signals.entry(change.path).or_default();
                signal.recent_changes += 1;
                signal
                    .distinct_states
                    .insert(candidate.change_id.to_string_full());
                if let Some(agent) = &candidate.attribution.agent {
                    signal
                        .distinct_agents
                        .insert(format!("{}/{}", agent.provider, agent.model));
                }
                signal.latest_seen_index = Some(
                    signal
                        .latest_seen_index
                        .map_or(index, |current| current.min(index)),
                );
            }
        }

        let stale_map = staleness::check_context_staleness(self, state)?;
        let active_context = match &state.context {
            Some(root) => self.list_context_entries(root, None)?,
            None => Vec::new(),
        };

        let active_paths: BTreeSet<String> = active_context
            .iter()
            .filter_map(|entry| match &entry.target {
                ContextTarget::File { path } => Some(path.clone()),
                ContextTarget::State { .. } => None,
            })
            .collect();

        let mut suggestions = Vec::new();
        for (path, signal) in signals {
            let has_context = active_paths.contains(&path);
            let stale_annotations = stale_map
                .iter()
                .filter(|(key, status)| {
                    key.starts_with(&format!("{path}:"))
                        && !matches!(status, staleness::StalenessStatus::Fresh)
                })
                .count() as u32;

            let mut score = signal.recent_changes.saturating_mul(CHANGE_WEIGHT);
            score += (signal.distinct_states.len() as u32).saturating_mul(DISTINCT_STATE_WEIGHT);
            score += (signal.distinct_agents.len() as u32).saturating_mul(DISTINCT_AGENT_WEIGHT);
            if signal.latest_seen_index.unwrap_or(usize::MAX) <= 3 {
                score += RECENCY_WEIGHT;
            }
            if stale_annotations > 0 {
                score += stale_annotations.saturating_mul(STALE_WEIGHT);
            }
            if has_context && stale_annotations == 0 {
                score = score.saturating_sub(HAS_CONTEXT_PENALTY);
            }

            let tier = if score >= HIGH_SUGGESTION_THRESHOLD {
                Some(ContextSuggestionTier::High)
            } else if score >= MEDIUM_SUGGESTION_THRESHOLD {
                Some(ContextSuggestionTier::Medium)
            } else {
                None
            };

            let Some(tier) = tier else {
                continue;
            };

            let mut reasons = Vec::new();
            if signal.recent_changes >= 3 {
                reasons.push(format!(
                    "{} recent changes across the last {} states",
                    signal.recent_changes,
                    history.len()
                ));
            }
            if signal.distinct_agents.len() >= 2 {
                reasons.push(format!(
                    "{} distinct agents touched this file",
                    signal.distinct_agents.len()
                ));
            }
            if stale_annotations > 0 {
                reasons.push(format!("{stale_annotations} annotation(s) may be stale"));
            }
            if !has_context {
                reasons.push("no active file guidance exists yet".to_string());
            }

            suggestions.push(ContextSuggestion {
                path,
                score,
                tier,
                reasons,
                recent_changes: signal.recent_changes,
                distinct_states: signal.distinct_states.len() as u32,
                distinct_agents: signal.distinct_agents.len() as u32,
                has_context,
                stale_annotations,
            });
        }

        suggestions.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        suggestions.truncate(limit);
        Ok(suggestions)
    }

    fn collect_state_window(
        &self,
        state: &State,
        limit: usize,
    ) -> Result<Vec<State>, anyhow::Error> {
        let query = HistoryQuery::new(Some(state.change_id)).with_limit(limit);
        Ok(self.query_history(&query)?)
    }
}

pub fn compute_rewrite_pct(previous: &str, next: &str) -> u32 {
    let prev_tokens = normalize_tokens(previous);
    let next_tokens = normalize_tokens(next);

    if prev_tokens.is_empty() && next_tokens.is_empty() {
        return 0;
    }
    if prev_tokens.is_empty() || next_tokens.is_empty() {
        return 100;
    }

    let prev_set: BTreeSet<_> = prev_tokens.iter().cloned().collect();
    let next_set: BTreeSet<_> = next_tokens.iter().cloned().collect();
    let intersection = prev_set.intersection(&next_set).count() as f64;
    let union = prev_set.union(&next_set).count() as f64;
    let similarity = if union == 0.0 {
        1.0
    } else {
        intersection / union
    };
    ((1.0 - similarity) * 100.0).round() as u32
}

pub fn is_major_rewrite(rewrite_pct: u32) -> bool {
    rewrite_pct >= MAJOR_REWRITE_THRESHOLD_PCT
}

fn normalize_tokens(input: &str) -> Vec<String> {
    input
        .lines()
        .flat_map(|line| {
            line.to_lowercase()
                .split(|ch: char| !ch.is_alphanumeric())
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_pct_is_zero_for_identical_content() {
        assert_eq!(compute_rewrite_pct("same tokens", "same tokens"), 0);
    }

    #[test]
    fn rewrite_pct_detects_major_changes() {
        let pct = compute_rewrite_pct("alpha beta gamma", "delta epsilon zeta");
        assert!(pct >= MAJOR_REWRITE_THRESHOLD_PCT);
        assert!(is_major_rewrite(pct));
    }
}
