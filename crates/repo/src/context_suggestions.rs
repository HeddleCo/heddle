// SPDX-License-Identifier: Apache-2.0
//! Context rewrite scoring and low-noise suggestion heuristics.

use std::collections::{BTreeMap, BTreeSet};

pub use objects::object::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW,
};
use objects::{
    object::{ContextTarget, State, SuggestionInputs, SuggestionSignal, score_suggestions},
    store::ObjectStore,
};

use crate::{HistoryQuery, Repository, staleness};

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

        Ok(score_suggestions(
            SuggestionInputs {
                signals,
                stale_map,
                active_paths,
                history_len: history.len(),
            },
            limit,
        ))
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
    use std::fs;

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

    #[test]
    fn suggest_context_targets_matches_golden_fixture() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();

        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "one\n").unwrap();
        repo.snapshot(Some("add a".to_string()), None).unwrap();

        fs::write(dir.path().join("src/b.rs"), "one\n").unwrap();
        repo.snapshot(Some("add b".to_string()), None).unwrap();

        fs::write(dir.path().join("src/a.rs"), "two\n").unwrap();
        repo.snapshot(Some("update a".to_string()), None).unwrap();

        fs::write(dir.path().join("src/a.rs"), "three\n").unwrap();
        let head = repo
            .snapshot(Some("update a again".to_string()), None)
            .unwrap();

        let suggestions = repo.suggest_context_targets(&head, 10).unwrap();

        assert_eq!(
            suggestions,
            vec![ContextSuggestion {
                path: "src/a.rs".to_string(),
                score: 84,
                tier: ContextSuggestionTier::High,
                reasons: vec![
                    "3 recent changes across the last 5 states".to_string(),
                    "no active file guidance exists yet".to_string(),
                ],
                recent_changes: 3,
                distinct_states: 3,
                distinct_agents: 0,
                has_context: false,
                stale_annotations: 0,
            }]
        );
    }
}
