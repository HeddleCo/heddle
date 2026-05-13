// SPDX-License-Identifier: Apache-2.0
//! Pattern deviation: fires when a changed symbol's structural shape
//! diverges substantially from local exemplars (sibling functions in the
//! same file/module, and the prior version of the same symbol).
//!
//! Pure: it operates on the `SemanticContext`'s parsed-file map and
//! compares structure-similarity scores against the configured threshold.

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};
use semantic::{
    analysis::{SimilarityMethod, compute_similarity},
    parser::FunctionDef,
};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "pattern_deviation.tree_sitter";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.pattern_deviation.enabled {
        return Vec::new();
    }
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());

    let mut out = Vec::new();
    let threshold = cfg.pattern_deviation.threshold;

    for (path, new_fns) in &ctx.new_functions {
        let prior_fns: Option<&Vec<FunctionDef>> = ctx.prior_functions.get(path);
        for fn_def in new_fns {
            let prior_same = prior_fns.and_then(|fns| fns.iter().find(|f| f.name == fn_def.name));
            // 1. Compare against the prior version of this same symbol.
            if let Some(prior_fn) = prior_same {
                let similarity = compute_similarity(
                    &prior_fn.content,
                    &fn_def.content,
                    SimilarityMethod::Tokens,
                ) as f32;
                let divergence = 1.0 - similarity;
                if divergence >= threshold {
                    out.push(make_signal(
                        path,
                        &fn_def.name,
                        format!(
                            "function body diverges {:.0}% from prior version",
                            divergence * 100.0
                        ),
                        new.change_id,
                        computed_at,
                    ));
                    continue;
                }
            } else {
                // 2. New symbol — compare against siblings (other functions
                // in the same file). If max similarity to any sibling is
                // below 1 - threshold, it diverges from the local style.
                let siblings: Vec<&FunctionDef> =
                    new_fns.iter().filter(|f| f.name != fn_def.name).collect();
                if siblings.is_empty() {
                    continue;
                }
                let max_sim = siblings
                    .iter()
                    .map(|s| {
                        compute_similarity(&s.content, &fn_def.content, SimilarityMethod::Tokens)
                            as f32
                    })
                    .fold(0.0_f32, f32::max);
                let divergence = 1.0 - max_sim;
                if divergence >= threshold {
                    out.push(make_signal(
                        path,
                        &fn_def.name,
                        format!(
                            "new function shape diverges {:.0}% from sibling exemplars",
                            divergence * 100.0
                        ),
                        new.change_id,
                        computed_at,
                    ));
                }
            }
        }
    }
    out
}

fn make_signal(
    path: &std::path::Path,
    symbol: &str,
    reason: String,
    against: objects::object::ChangeId,
    ts: i64,
) -> RiskSignal {
    RiskSignal {
        kind: RiskSignalKind::PatternDeviation,
        anchor: SignalAnchor::symbol(path.to_string_lossy(), symbol),
        reason: truncate_reason(&reason),
        producer: ProducerId::new(MODULE_ID, VERSION),
        computed_at: ts,
        computed_against: Some(against),
    }
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
    fn quiet_when_disabled() {
        let mut cfg = ReviewSignalsConfig::default();
        cfg.pattern_deviation.enabled = false;
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn quiet_with_empty_context() {
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }
}