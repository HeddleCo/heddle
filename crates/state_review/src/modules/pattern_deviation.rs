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

use crate::truncate_reason;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use objects::object::{Attribution, ChangeId, ContentHash, Principal, RiskSignalKind};

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

    #[test]
    fn fires_when_function_diverges_from_prior_version() {
        let path = PathBuf::from("src/scoring.rs");
        let mut cfg = ReviewSignalsConfig::default();
        cfg.pattern_deviation.threshold = 0.2;
        let mut ctx = SemanticContext::new();
        ctx.prior_functions.insert(
            path.clone(),
            vec![fdef(
                "score",
                "fn score(value: usize) -> usize { value + 1 }",
            )],
        );
        ctx.new_functions.insert(
            path,
            vec![fdef(
                "score",
                "fn score(socket: &mut Socket) -> usize { while socket.poll() { rotate_key(); } 0 }",
            )],
        );
        let new = state_with_change_id(9);

        let signals = run(&empty_state(), &new, &cfg, &ctx);

        assert!(
            signals.iter().any(|signal| {
                signal.kind == RiskSignalKind::PatternDeviation
                    && signal.anchor.file == "src/scoring.rs"
                    && signal.anchor.symbol.as_deref() == Some("score")
                    && signal.reason.contains("prior version")
                    && signal.computed_against == Some(new.change_id)
            }),
            "expected prior-version pattern deviation, got: {signals:?}"
        );
    }

    #[test]
    fn fires_when_new_function_diverges_from_sibling_exemplars() {
        let path = PathBuf::from("src/workers.rs");
        let mut cfg = ReviewSignalsConfig::default();
        cfg.pattern_deviation.threshold = 0.5;
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            path,
            vec![
                fdef(
                    "load_user",
                    "fn load_user() { let record = fetch_user(); validate(record); save(record); }",
                ),
                fdef(
                    "load_order",
                    "fn load_order() { let record = fetch_order(); validate(record); save(record); }",
                ),
                fdef(
                    "decode_wire",
                    "fn decode_wire() { while socket.poll() { rotate_key(); decode_frame(); } }",
                ),
            ],
        );

        let signals = run(&empty_state(), &state_with_change_id(10), &cfg, &ctx);

        assert!(
            signals.iter().any(|signal| {
                signal.kind == RiskSignalKind::PatternDeviation
                    && signal.anchor.file == "src/workers.rs"
                    && signal.anchor.symbol.as_deref() == Some("decode_wire")
                    && signal.reason.contains("sibling exemplars")
            }),
            "expected sibling pattern deviation for decode_wire, got: {signals:?}"
        );
    }
}
