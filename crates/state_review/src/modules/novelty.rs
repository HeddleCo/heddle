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
    analysis::try_compute_ast_similarity_for_languages,
    parser::{FunctionDef, Language},
};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext, truncate_reason};

const VERSION: u32 = 1;
const MODULE_ID: &str = "novelty.tree_sitter";
const MIN_CORPUS_FUNCTIONS: usize = 4;

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.novelty.enabled || !ctx.corpus_complete {
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
        .filter(|(path, fn_def)| ctx.is_emit_target(path, fn_def))
    {
        let Some(max_sim) = max_structural_similarity(path, fn_def, &corpus) else {
            continue;
        };
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
                computed_against: Some(new.state_id),
            });
        }
    }
    out
}

fn max_structural_similarity(
    path: &std::path::Path,
    fn_def: &FunctionDef,
    corpus: &[(PathBuf, FunctionDef)],
) -> Option<f32> {
    let language = Language::from_path(path);
    if language == Language::Unknown {
        return None;
    }
    let mut best: Option<f32> = None;
    for (other_path, other) in corpus {
        if other_path == path && other.symbol_identity() == fn_def.symbol_identity() {
            continue;
        }
        let other_language = Language::from_path(other_path);
        let score = try_compute_ast_similarity_for_languages(
            &other.content,
            other_language,
            &fn_def.content,
            language,
        )?;
        best = Some(best.map_or(score as f32, |current| current.max(score as f32)));
    }
    best
}

#[cfg(test)]
#[path = "novelty_tests.rs"]
mod tests;
