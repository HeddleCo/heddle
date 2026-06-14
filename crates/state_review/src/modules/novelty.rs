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

    // For each function in the changed-files set, compare to the rest of the
    // full-repo corpus. Fire when max similarity is below `1 - tolerance`.
    // The corpus stays whole (so "unique in the repo" is measured against
    // every function), but we only *evaluate and report* functions that live
    // in a changed file — novelty is a diff-scoped signal, not a repo scan.
    let novelty_threshold = 1.0 - tolerance;
    let mut out = Vec::new();
    for (path, fn_def) in corpus
        .iter()
        .filter(|(path, _)| ctx.changed_paths.contains(path))
    {
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

use crate::truncate_reason;

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

    fn fdef(name: &str, body: &str) -> FunctionDef {
        FunctionDef {
            name: name.to_string(),
            signature: format!("fn {name}()"),
            start_line: 1,
            end_line: 3,
            content: body.to_string(),
        }
    }

    #[test]
    fn novelty_scoped_to_changed_files() {
        // Corpus of four files, each with a structurally distinct function, so
        // every function is "novel" against the rest of the repo. Only
        // `changed.rs` is in the changed set, so novelty must report exactly
        // one signal — for that file's function — not one per corpus function.
        let cfg = ReviewSignalsConfig::default();
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("a.rs"),
            vec![fdef(
                "alpha",
                "let total = first + second + third + fourth;",
            )],
        );
        ctx.new_functions.insert(
            PathBuf::from("b.rs"),
            vec![fdef("beta", "for widget in inventory { ship(widget); }")],
        );
        ctx.new_functions.insert(
            PathBuf::from("c.rs"),
            vec![fdef(
                "gamma",
                "match colour { Red => stop(), Green => go() }",
            )],
        );
        ctx.new_functions.insert(
            PathBuf::from("changed.rs"),
            vec![fdef(
                "delta",
                "while pending { dequeue().handle(); } flush();",
            )],
        );
        ctx.changed_paths.insert(PathBuf::from("changed.rs"));

        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);

        assert_eq!(
            signals.len(),
            1,
            "novelty should fire only for the changed file, got: {signals:?}"
        );
        assert_eq!(signals[0].anchor.file, "changed.rs");
        assert_eq!(signals[0].anchor.symbol.as_deref(), Some("delta"));
    }
}
