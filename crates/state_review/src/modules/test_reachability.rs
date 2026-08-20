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
use semantic::{Language, parser::FunctionDef};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "test_reachability.tree_sitter";
const REASON_TEXT: &str = "no test reaches this symbol via static reachability via tree-sitter call graph; \
     this is not runtime coverage";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.test_reachability.enabled || !ctx.corpus_complete {
        return Vec::new();
    }
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());

    let catalog: Vec<CatalogEntry<'_>> = ctx
        .new_functions
        .iter()
        .flat_map(|(path, fns)| {
            fns.iter().map(|def| CatalogEntry {
                path: path.as_path(),
                identity: def.symbol_identity(),
                def,
            })
        })
        .collect();
    let all_fns: HashMap<(&Path, &str), &FunctionDef> = catalog
        .iter()
        .map(|entry| ((entry.path, entry.identity.as_str()), entry.def))
        .collect();

    let mut test_set: HashSet<(&Path, &str)> = HashSet::new();
    for (&key, def) in &all_fns {
        if is_test_function(&def.name, Language::from_path(key.0)) {
            test_set.insert(key);
        }
    }

    if test_set.len() < cfg.test_reachability.min_test_functions_in_repo as usize {
        return Vec::new();
    }

    let mut callers_of: HashMap<(&Path, &str), Vec<(&Path, &str)>> = HashMap::new();
    for caller in &catalog {
        for callee in &catalog {
            if caller.path == callee.path && caller.identity == callee.identity {
                continue;
            }
            if mentions_callee(&caller.def.content, callee.def) {
                callers_of
                    .entry((callee.path, callee.identity.as_str()))
                    .or_default()
                    .push((caller.path, caller.identity.as_str()));
            }
        }
    }

    let mut out = Vec::new();
    for entry in &catalog {
        let key = (entry.path, entry.identity.as_str());
        if !ctx.is_emit_target(entry.path, entry.def) {
            continue;
        }
        if test_set.contains(&key) {
            continue;
        }
        if !reaches_test(key, &callers_of, &test_set) {
            out.push(RiskSignal {
                kind: RiskSignalKind::TestReachability,
                anchor: SignalAnchor::symbol(entry.path.to_string_lossy(), &entry.def.name),
                reason: REASON_TEXT.to_string(),
                producer: ProducerId::new(MODULE_ID, VERSION),
                computed_at,
                computed_against: Some(new.state_id),
            });
        }
    }
    out
}

struct CatalogEntry<'a> {
    path: &'a Path,
    identity: String,
    def: &'a FunctionDef,
}

fn reaches_test<'a>(
    start: (&'a Path, &'a str),
    callers_of: &HashMap<(&'a Path, &'a str), Vec<(&'a Path, &'a str)>>,
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
        if let Some(callers) = callers_of.get(&node) {
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
        // Zig `test "…"` / `test Name` blocks are extracted as functions named
        // `test:"…"` / `test:Name` by the symbol resolver.
        Language::Zig => name.starts_with("test:"),
        _ => false,
    }
}

fn mentions_callee(body: &str, callee: &FunctionDef) -> bool {
    if callee.container.is_empty() {
        return body_mentions(body, &callee.name);
    }
    body_mentions(body, &callee.qualified_name())
        || body_mentions(body, &format!("{}.{}", callee.container, callee.name))
}

fn body_mentions(body: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    body.contains(&needle)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use objects::object::{Attribution, ContentHash, Principal, RiskSignalKind};

    use super::*;

    fn empty_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
    }

    fn cfg_with_one_required_test() -> ReviewSignalsConfig {
        let mut cfg = ReviewSignalsConfig::default();
        cfg.test_reachability.min_test_functions_in_repo = 1;
        cfg
    }

    fn fdef(name: &str, content: &str) -> FunctionDef {
        fdef_in("", name, content)
    }

    fn fdef_in(container: &str, name: &str, content: &str) -> FunctionDef {
        FunctionDef {
            name: name.to_string(),
            container: container.to_string(),
            signature: format!("fn {name}()"),
            start_line: 1,
            end_line: 3,
            content: content.to_string(),
        }
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

    #[test]
    fn direct_test_reachability_stays_quiet() {
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("covered", "fn covered() { do_work(); }"),
                fdef("test_covered", "fn test_covered() { covered(); }"),
            ],
        );
        ctx.changed_paths.insert(PathBuf::from("src/lib.rs"));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert!(
            signals.is_empty(),
            "direct test caller should cover: {signals:?}"
        );
    }

    #[test]
    fn transitive_test_reachability_stays_quiet() {
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("covered", "fn covered() { do_work(); }"),
                fdef("helper", "fn helper() { covered(); }"),
                fdef("test_covered", "fn test_covered() { helper(); }"),
            ],
        );
        ctx.changed_paths.insert(PathBuf::from("src/lib.rs"));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert!(
            signals.is_empty(),
            "transitive test caller should cover all callees: {signals:?}"
        );
    }

    #[test]
    fn unreachable_symbol_fires() {
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("orphan", "fn orphan() { do_work(); }"),
                fdef("test_irrelevant", "fn test_irrelevant() { assert!(true); }"),
            ],
        );
        ctx.changed_paths.insert(PathBuf::from("src/lib.rs"));
        ctx.changed_symbols.insert((
            PathBuf::from("src/lib.rs"),
            fdef("orphan", "").symbol_identity(),
        ));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert_eq!(signal_symbols(&signals), BTreeSet::from(["orphan"]));
        assert_eq!(signals[0].kind, RiskSignalKind::TestReachability);
    }

    #[test]
    fn unreachable_cycle_fires_once_per_cycled_symbol() {
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("alpha", "fn alpha() { beta(); }"),
                fdef("beta", "fn beta() { alpha(); }"),
                fdef("test_irrelevant", "fn test_irrelevant() { assert!(true); }"),
            ],
        );
        ctx.changed_paths.insert(PathBuf::from("src/lib.rs"));
        ctx.changed_symbols.insert((
            PathBuf::from("src/lib.rs"),
            fdef("alpha", "").symbol_identity(),
        ));
        ctx.changed_symbols.insert((
            PathBuf::from("src/lib.rs"),
            fdef("beta", "").symbol_identity(),
        ));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert_eq!(signal_symbols(&signals), BTreeSet::from(["alpha", "beta"]));
    }

    #[test]
    fn unreachable_scoped_to_changed_symbols() {
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("orphan", "fn orphan() { do_work(); }"),
                fdef("sibling", "fn sibling() { do_other(); }"),
                fdef("test_irrelevant", "fn test_irrelevant() { assert!(true); }"),
            ],
        );
        ctx.changed_symbols.insert((
            PathBuf::from("src/lib.rs"),
            fdef("orphan", "").symbol_identity(),
        ));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert_eq!(signal_symbols(&signals), BTreeSet::from(["orphan"]));
    }

    #[test]
    fn unreachable_stays_quiet_when_corpus_incomplete() {
        let mut ctx = SemanticContext::new();
        ctx.corpus_complete = false;
        ctx.new_functions.insert(
            PathBuf::from("src/lib.rs"),
            vec![
                fdef("orphan", "fn orphan() { do_work(); }"),
                fdef("test_irrelevant", "fn test_irrelevant() { assert!(true); }"),
            ],
        );
        ctx.changed_symbols.insert((
            PathBuf::from("src/lib.rs"),
            fdef("orphan", "").symbol_identity(),
        ));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );
        assert!(
            signals.is_empty(),
            "incomplete corpus must fail-closed: {signals:?}"
        );
    }

    fn signal_symbols(signals: &[RiskSignal]) -> BTreeSet<&str> {
        signals
            .iter()
            .filter_map(|signal| signal.anchor.symbol.as_deref())
            .collect()
    }

    #[test]
    fn qualified_call_does_not_cover_same_bare_name() {
        let path = PathBuf::from("src/lib.rs");
        let foo_run = fdef_in("Foo", "run", "fn run() { do_foo(); }");
        let bar_run = fdef_in("Bar", "run", "fn run() { do_bar(); }");
        let mut ctx = SemanticContext::new();
        ctx.new_functions.insert(
            path.clone(),
            vec![
                foo_run.clone(),
                bar_run.clone(),
                fdef("test_bar", "fn test_bar() { Bar::run(); }"),
            ],
        );
        ctx.changed_symbols
            .insert((path.clone(), foo_run.symbol_identity()));
        ctx.changed_symbols
            .insert((path, bar_run.symbol_identity()));

        let signals = run(
            &empty_state(),
            &empty_state(),
            &cfg_with_one_required_test(),
            &ctx,
        );

        assert_eq!(
            signals.len(),
            1,
            "Bar::run is tested; Foo::run must stay unreachable: {signals:?}"
        );
        assert_eq!(signal_symbols(&signals), BTreeSet::from(["run"]));
    }
}
