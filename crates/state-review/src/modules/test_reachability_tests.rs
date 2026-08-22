// SPDX-License-Identifier: Apache-2.0
//! Tests for static test-reachability.

use std::{collections::BTreeSet, path::PathBuf};

use objects::object::{Attribution, ContentHash, Principal, RiskSignal, RiskSignalKind};

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

fn signal_symbols(signals: &[RiskSignal]) -> BTreeSet<&str> {
    signals
        .iter()
        .filter_map(|signal| signal.anchor.symbol.as_deref())
        .collect()
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

#[test]
fn module_qualified_call_does_not_cover_other_module() {
    let path = PathBuf::from("src/lib.rs");
    let foo_run = fdef_in("foo", "run", "fn run() { do_foo(); }");
    let bar_run = fdef_in("bar", "run", "fn run() { do_bar(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            foo_run.clone(),
            bar_run.clone(),
            fdef("test_foo", "fn test_foo() { foo::run(); }"),
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
        "foo::run must not mark bar::run reachable: {signals:?}"
    );
}

#[test]
fn unique_receiver_call_covers_the_only_method() {
    let path = PathBuf::from("src/lib.rs");
    let foo_run = fdef_in("Foo", "run", "fn run() { do_foo(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            foo_run.clone(),
            fdef(
                "test_foo",
                "fn test_foo() { let instance = Foo; instance.run(); }",
            ),
        ],
    );
    ctx.changed_symbols
        .insert((path, foo_run.symbol_identity()));

    let signals = run(
        &empty_state(),
        &empty_state(),
        &cfg_with_one_required_test(),
        &ctx,
    );
    assert!(
        signals.is_empty(),
        "unique receiver call must cover Foo::run: {signals:?}"
    );
}

#[test]
fn ambiguous_receiver_call_fail_closes_instead_of_warning() {
    let path = PathBuf::from("src/lib.rs");
    let foo_run = fdef_in("Foo", "run", "fn run() { do_foo(); }");
    let bar_run = fdef_in("Bar", "run", "fn run() { do_bar(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            foo_run.clone(),
            bar_run.clone(),
            fdef(
                "test_foo",
                "fn test_foo() { let instance = Foo; instance.run(); }",
            ),
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
    assert!(
        signals.is_empty(),
        "ambiguous instance.run() must fail-closed, not warn: {signals:?}"
    );
}

#[test]
fn bare_call_resolves_to_caller_container() {
    let path = PathBuf::from("src/lib.rs");
    let target = fdef_in("a", "target", "fn target() { do_work(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            target.clone(),
            fdef_in("a", "test_target", "fn test_target() { target(); }"),
        ],
    );
    ctx.changed_symbols.insert((path, target.symbol_identity()));

    let signals = run(
        &empty_state(),
        &empty_state(),
        &cfg_with_one_required_test(),
        &ctx,
    );
    assert!(
        signals.is_empty(),
        "bare target() in mod a must cover a::target: {signals:?}"
    );
}

#[test]
fn nested_function_call_does_not_cover_outer_callee() {
    let path = PathBuf::from("src/lib.rs");
    let target = fdef("target", "fn target() { do_work(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            target.clone(),
            fdef("test_x", "fn test_x() { fn unused() { target(); } }"),
        ],
    );
    ctx.changed_symbols.insert((path, target.symbol_identity()));

    let signals = run(
        &empty_state(),
        &empty_state(),
        &cfg_with_one_required_test(),
        &ctx,
    );
    assert_eq!(
        signal_symbols(&signals),
        BTreeSet::from(["target"]),
        "nested unused() call must not cover target: {signals:?}"
    );
}

#[test]
fn python_direct_call_covers_target() {
    let path = PathBuf::from("mod.py");
    let target = fdef("target", "def target():\n    return 1\n");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            target.clone(),
            fdef("test_target", "def test_target():\n    target()\n"),
        ],
    );
    ctx.changed_symbols.insert((path, target.symbol_identity()));

    let signals = run(
        &empty_state(),
        &empty_state(),
        &cfg_with_one_required_test(),
        &ctx,
    );
    assert!(
        signals.is_empty(),
        "python test_target() must cover target: {signals:?}"
    );
}

#[test]
fn comment_todo_call_does_not_mark_reachable() {
    let path = PathBuf::from("src/lib.rs");
    let orphan = fdef("orphan", "fn orphan() { do_work(); }");
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        path.clone(),
        vec![
            orphan.clone(),
            fdef(
                "test_irrelevant",
                "fn test_irrelevant() { /* TODO: call orphan() */ let _ = \"orphan()\"; }",
            ),
        ],
    );
    ctx.changed_symbols.insert((path, orphan.symbol_identity()));

    let signals = run(
        &empty_state(),
        &empty_state(),
        &cfg_with_one_required_test(),
        &ctx,
    );
    assert_eq!(
        signal_symbols(&signals),
        BTreeSet::from(["orphan"]),
        "comment/string must not create a caller edge: {signals:?}"
    );
}
