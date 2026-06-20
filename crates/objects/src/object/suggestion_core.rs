// SPDX-License-Identifier: Apache-2.0
//! Pure context suggestion scoring.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::StalenessStatus;

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
pub struct SuggestionSignal {
    pub recent_changes: u32,
    pub distinct_states: BTreeSet<String>,
    pub distinct_agents: BTreeSet<String>,
    pub latest_seen_index: Option<usize>,
}

pub struct SuggestionInputs {
    pub signals: BTreeMap<String, SuggestionSignal>,
    pub stale_map: HashMap<String, StalenessStatus>,
    pub active_paths: BTreeSet<String>,
    pub history_len: usize,
}

pub fn score_suggestions(inputs: SuggestionInputs, limit: usize) -> Vec<ContextSuggestion> {
    let SuggestionInputs {
        signals,
        stale_map,
        active_paths,
        history_len,
    } = inputs;
    let mut suggestions = Vec::new();

    for (path, signal) in signals {
        let has_context = active_paths.contains(&path);
        let stale_annotations = stale_map
            .iter()
            .filter(|(key, status)| {
                key.starts_with(&format!("{path}:"))
                    && !matches!(status, StalenessStatus::Fresh)
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
                signal.recent_changes, history_len
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
    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_suggestions_hits_tier_boundaries_and_context_penalty() {
        let mut signals = BTreeMap::new();
        signals.insert(
            "high.rs".to_string(),
            signal(2, [], ["anthropic/claude", "openai/codex"], None),
        );
        signals.insert(
            "penalty.rs".to_string(),
            signal(5, [], [], None),
        );
        signals.insert("low.rs".to_string(), signal(1, ["s1"], [], None));

        let mut stale_map = HashMap::new();
        stale_map.insert("high.rs:File".to_string(), StalenessStatus::Unknown);
        stale_map.insert("penalty.rs:File".to_string(), StalenessStatus::Fresh);

        let inputs = SuggestionInputs {
            signals,
            stale_map,
            active_paths: BTreeSet::from(["penalty.rs".to_string()]),
            history_len: 9,
        };

        let suggestions = score_suggestions(inputs, 10);

        assert_eq!(
            suggestions,
            vec![
                ContextSuggestion {
                    path: "high.rs".to_string(),
                    score: HIGH_SUGGESTION_THRESHOLD,
                    tier: ContextSuggestionTier::High,
                    reasons: vec![
                        "2 distinct agents touched this file".to_string(),
                        "1 annotation(s) may be stale".to_string(),
                        "no active file guidance exists yet".to_string(),
                    ],
                    recent_changes: 2,
                    distinct_states: 0,
                    distinct_agents: 2,
                    has_context: false,
                    stale_annotations: 1,
                },
                ContextSuggestion {
                    path: "penalty.rs".to_string(),
                    score: MEDIUM_SUGGESTION_THRESHOLD,
                    tier: ContextSuggestionTier::Medium,
                    reasons: vec!["5 recent changes across the last 9 states".to_string()],
                    recent_changes: 5,
                    distinct_states: 0,
                    distinct_agents: 0,
                    has_context: true,
                    stale_annotations: 0,
                },
            ]
        );
    }

    fn signal<const S: usize, const A: usize>(
        recent_changes: u32,
        states: [&str; S],
        agents: [&str; A],
        latest_seen_index: Option<usize>,
    ) -> SuggestionSignal {
        SuggestionSignal {
            recent_changes,
            distinct_states: states.into_iter().map(str::to_string).collect(),
            distinct_agents: agents.into_iter().map(str::to_string).collect(),
            latest_seen_index,
        }
    }
}
