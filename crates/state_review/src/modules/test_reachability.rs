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
    path::Path,
};

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};
use semantic::{parser::FunctionDef, Language};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "test_reachability.tree_sitter";
const REASON_TEXT: &str =
    "no test reaches this symbol via static reachability via tree-sitter call graph; \
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

    let mut test_set: HashSet<(&Path, &str)> = HashSet::new();
    let mut all_fns: HashMap<(&Path, &str), &FunctionDef> = HashMap::new();
    for (path, fns) in &ctx.new_functions {
        let lang = Language::from_path(path);
        for fn_def in fns {
            let key = (path.as_path(), fn_def.name.as_str());
            all_fns.insert(key, fn_def);
            if is_test_function(&fn_def.name, lang) {
                test_set.insert(key);
            }
        }
    }

    if test_set.len() < cfg.test_reachability.min_test_functions_in_repo as usize {
        return Vec::new();
    }

    let mut callers_of: HashMap<&str, Vec<(&Path, &str)>> = HashMap::new();
    let names: Vec<&str> = all_fns.keys().map(|(_, n)| *n).collect();
    for (&(caller_path, caller_name), caller_def) in &all_fns {
        for &callee_name in &names {
            if callee_name == caller_name {
                continue;
            }
            if body_mentions(&caller_def.content, callee_name) {
                callers_of
                    .entry(callee_name)
                    .or_default()
                    .push((caller_path, caller_name));
            }
        }
    }

    let mut out = Vec::new();
    for (path, fns) in &ctx.new_functions {
        for fn_def in fns {
            let key = (path.as_path(), fn_def.name.as_str());
            if test_set.contains(&key) {
                continue;
            }
            if !reaches_test(key, &callers_of, &test_set) {
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
    }
    out
}

fn reaches_test<'a>(
    start: (&'a Path, &'a str),
    callers_of: &HashMap<&str, Vec<(&'a Path, &'a str)>>,
    test_set: &HashSet<(&Path, &str)>,
) -> bool {
    let mut visited: HashSet<(&Path, &str)> = HashSet::new();
    let mut queue: VecDeque<(&Path, &str)> = VecDeque::new();
    queue.push_back(start);
    visited.insert(start);
    while let Some(node) = queue.pop_front() {
        if test_set.contains(&node) {
            return true;
        }
        if let Some(callers) = callers_of.get(node.1) {
            for &caller in callers {
                if visited.insert(caller) {
                    queue.push_back(caller);
                }
            }
        }
    }
    false
}

fn is_test_function(name: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => name.starts_with("test_") || name.ends_with("_test"),
        Language::Python => name.starts_with("test_") || name == "setUp",
        Language::JavaScript | Language::TypeScript => {
            name.starts_with("test") || name.starts_with("it") || name.starts_with("describe")
        }
        Language::Go => name.starts_with("Test"),
        _ => false,
    }
}

fn body_mentions(body: &str, name: &str) -> bool {
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
