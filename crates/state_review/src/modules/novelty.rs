// SPDX-License-Identifier: Apache-2.0
//! Novelty: fires when a changed symbol's structural shape doesn't appear
//! elsewhere in the repo's code base. Pure: operates on the parsed-file
//! map carried by [`SemanticContext`] and uses the function-body
//! similarity helper from `crates/semantic`.
//!
//! For first ship, "novel" = new function body whose maximum similarity
//! to any other function body in the new state is below `1 - tolerance`.
//! When the corpus is too small (1 file, 1 function), we stay quiet
//! rather than firing on every change.

use std::path::PathBuf;

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};
use semantic::{
    analysis::{SimilarityMethod, compute_similarity},
    parser::FunctionDef,
};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "novelty.tree_sitter";
const MIN_CORPUS_FUNCTIONS: usize = 4;

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.novelty.enabled {
        return Vec::new();
    }
    let tolerance = cfg.novelty.tolerance.clamp(0.0, 1.0);
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());

    // Build the corpus: every function body across every file in `new_functions`.
    let corpus: Vec<(PathBuf, FunctionDef)> = ctx
        .new_functions
        .iter()
        .flat_map(|(path, fns)| {
            fns.iter()
                .map(|f| (path.clone(), f.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    if corpus.len() < MIN_CORPUS_FUNCTIONS {
        return Vec::new();
    }

    // For each function in the changed-files set (here: every function
    // since we don't compute the diff). Compare to the rest of the
    // corpus. Fire when max similarity is below `1 - tolerance`.
    let novelty_threshold = 1.0 - tolerance;
    let mut out = Vec::new();
    for (path, fn_def) in &corpus {
        let max_sim = corpus
            .iter()
            .filter(|(p, f)| !(p == path && f.name == fn_def.name))
            .map(|(_, other)| {
                compute_similarity(&other.content, &fn_def.content, SimilarityMethod::Tokens) as f32
            })
            .fold(0.0_f32, f32::max);
        if max_sim < novelty_threshold {
            let reason = format!(
                "function shape unique in repo (max sibling similarity {:.0}%)",
                max_sim * 100.0
            );
            out.push(RiskSignal {
                kind: RiskSignalKind::Novelty,
                anchor: SignalAnchor::symbol(path.to_string_lossy(), &fn_def.name),
                reason: truncate_reason(&reason),
                producer: ProducerId::new(MODULE_ID, VERSION),
                computed_at,
                computed_against: Some(new.change_id),
            });
        }
    }
    out
}

fn truncate_reason(reason: &str) -> String {
    if reason.len() <= objects::object::MAX_REASON_LEN {
        reason.to_string()
    } else {
        let take = objects::object::MAX_REASON_LEN.saturating_sub(1);
        let mut out: String = reason.chars().take(take).collect();
        out.push('…');
        out
    }
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
    fn quiet_with_small_corpus() {
        // Empty SemanticContext = corpus of zero. Stays quiet.
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn quiet_when_disabled() {
        let mut cfg = ReviewSignalsConfig::default();
        cfg.novelty.enabled = false;
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }
}
