// SPDX-License-Identifier: Apache-2.0
//! Test reachability: fires when no test in the repo statically reaches
//! the changed symbol via tree-sitter call-graph traversal.
//!
//! Pure: walks the `SemanticContext`'s parsed-file map, identifies test
//! functions by language-specific naming heuristics, and BFS through
//! callers. The reason text MUST clarify this is **static reachability via
//! tree-sitter call graph; this is not runtime coverage** — that phrase
//! is asserted in tests.
//!
//! Skips silently when the repo has fewer than the configured minimum
//! number of test functions, since firing on every greenfield repo would
//! be noise.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
};

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};
use semantic::{Language, parser::FunctionDef};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "test_reachability.tree_sitter";
/// Required reason-text marker. Asserted by tests so the rendering stays
/// honest about what the signal is measuring.
const REASON_TEXT: &str = "no test reaches this symbol via static reachability via tree-sitter call graph; \
     this is not runtime coverage";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.test_reachability.enabled {
        return Vec::new();
    }
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());

    // Identify all test functions across the corpus.
    let mut test_set: HashSet<(PathBuf, String)> = HashSet::new();
    let mut all_fns: HashMap<(PathBuf, String), FunctionDef> = HashMap::new();
    for (path, fns) in &ctx.new_functions {
        let lang = Language::from_path(path);
        for fn_def in fns {
            let key = (path.clone(), fn_def.name.clone());
            all_fns.insert(key.clone(), fn_def.clone());
            if is_test_function(&fn_def.name, lang) {
                test_set.insert(key);
            }
        }
    }

    if test_set.len() < cfg.test_reachability.min_test_functions_in_repo as usize {
        return Vec::new();
    }

    // Build a callee → callers index by scanning every function body for
    // identifiers that match other function names. Cheap-and-good — a
    // real call graph would resolve namespaces; the tradeoff is documented
    // by the module id (`.tree_sitter`).
    let mut callers_of: HashMap<String, Vec<(PathBuf, String)>> = HashMap::new();
    let names: Vec<String> = all_fns.keys().map(|(_, n)| n.clone()).collect();
    for ((caller_path, caller_name), caller_def) in &all_fns {
        for callee_name in &names {
            if callee_name == caller_name {
                continue;
            }
            if body_mentions(&caller_def.content, callee_name) {
                callers_of
                    .entry(callee_name.clone())
                    .or_default()
                    .push((caller_path.clone(), caller_name.clone()));
            }
        }
    }

    // For each non-test function, BFS through callers. If any path lands
    // in a test, the symbol is reachable. Otherwise emit the signal.
    let mut out = Vec::new();
    let flat: Vec<(PathBuf, FunctionDef)> = ctx
        .new_functions
        .iter()
        .flat_map(|(p, fns)| fns.iter().map(move |fd| (p.clone(), fd.clone())))
        .collect();
    for (path, fn_def) in &flat {
        let key = (path.clone(), fn_def.name.clone());
        if test_set.contains(&key) {
            continue;
        }
        if !reaches_test(&key, &callers_of, &test_set) {
            out.push(RiskSignal {
                kind: RiskSignalKind::TestReachability,
                anchor: SignalAnchor::symbol(path.to_string_lossy(), &fn_def.name),
                reason: REASON_TEXT.to_string(),
                producer: ProducerId::new(MODULE_ID, VERSION),
                computed_at,
                computed_against: Some(new.change_id),
            });
        }
    }
    out
}

fn reaches_test(
    start: &(PathBuf, String),
    callers_of: &HashMap<String, Vec<(PathBuf, String)>>,
    test_set: &HashSet<(PathBuf, String)>,
) -> bool {
    let mut visited: HashSet<(PathBuf, String)> = HashSet::new();
    let mut queue: VecDeque<(PathBuf, String)> = VecDeque::new();
    queue.push_back(start.clone());
    visited.insert(start.clone());
    while let Some(node) = queue.pop_front() {
        if test_set.contains(&node) {
            return true;
        }
        if let Some(callers) = callers_of.get(&node.1) {
            for caller in callers {
                if visited.insert(caller.clone()) {
                    queue.push_back(caller.clone());
                }
            }
        }
    }
    false
}

fn is_test_function(name: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => {
            // We don't see attribute macros from tree-sitter's function
            // body alone, so use the conventional `_test` suffix or
            // `test_` prefix. The `#[test]` attribute case lands when
            // semantic exposes attribute info.
            name.starts_with("test_") || name.ends_with("_test")
        }
        Language::Python => name.starts_with("test_") || name == "setUp",
        Language::JavaScript | Language::TypeScript => {
            name.starts_with("test") || name.starts_with("it") || name.starts_with("describe")
        }
        Language::Go => name.starts_with("Test"),
        _ => false,
    }
}

fn body_mentions(body: &str, name: &str) -> bool {
    // Identifier-boundary check: we want `foo(` not `foobar(`.
    let needle = format!("{name}(");
    body.contains(&needle)
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
        cfg.test_reachability.enabled = false;
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn quiet_with_no_tests_in_corpus() {
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn reason_text_marks_static_reachability() {
        // The contract: when this module fires, the reason text MUST
        // clarify it's static-reachability via tree-sitter, not runtime
        // coverage. We assert against the constant directly because
        // building a SemanticContext with parsed files would require
        // tree-sitter setup; the constant guards the wire-level promise.
        assert!(REASON_TEXT.contains("static reachability"));
        assert!(REASON_TEXT.contains("not runtime coverage"));
    }

    #[test]
    fn rust_test_naming_heuristic_recognises_underscore_prefixes() {
        assert!(is_test_function("test_main_branch", Language::Rust));
        assert!(is_test_function("login_test", Language::Rust));
        assert!(!is_test_function("login", Language::Rust));
    }

    #[test]
    fn python_test_naming_heuristic() {
        assert!(is_test_function("test_endpoint", Language::Python));
        assert!(is_test_function("setUp", Language::Python));
        assert!(!is_test_function("endpoint", Language::Python));
    }

    #[test]
    fn go_test_naming_heuristic() {
        assert!(is_test_function("TestRouter", Language::Go));
        assert!(!is_test_function("Router", Language::Go));
    }
}