// SPDX-License-Identifier: Apache-2.0
//! Novelty module tests.

use std::path::PathBuf;

use objects::object::{Attribution, ContentHash, Principal, State};

use super::*;

fn empty_state() -> State {
    State::new_snapshot(
        ContentHash::compute(b"tree"),
        vec![],
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    )
}

fn fdef(name: &str, body: &str) -> FunctionDef {
    FunctionDef {
        name: name.to_string(),
        container: String::new(),
        signature: format!("fn {name}()"),
        start_line: 1,
        end_line: 3,
        content: body.to_string(),
    }
}

fn distinct_bodies() -> [&'static str; 4] {
    [
        "fn alpha() { let total = first + second + third + fourth; }",
        "fn beta() { for widget in inventory { ship(widget); } }",
        "fn gamma() { match colour { Red => stop(), Green => go() } }",
        "fn delta() { while pending { dequeue().handle(); } flush(); }",
    ]
}

#[test]
fn quiet_with_small_corpus() {
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

#[test]
fn novelty_scoped_to_changed_files() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    let [alpha, beta, gamma, delta] = distinct_bodies();
    ctx.new_functions
        .insert(PathBuf::from("a.rs"), vec![fdef("alpha", alpha)]);
    ctx.new_functions
        .insert(PathBuf::from("b.rs"), vec![fdef("beta", beta)]);
    ctx.new_functions
        .insert(PathBuf::from("c.rs"), vec![fdef("gamma", gamma)]);
    ctx.new_functions
        .insert(PathBuf::from("changed.rs"), vec![fdef("delta", delta)]);
    ctx.changed_paths.insert(PathBuf::from("changed.rs"));
    ctx.changed_symbols.insert((
        PathBuf::from("changed.rs"),
        fdef("delta", "").symbol_identity(),
    ));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);

    assert_eq!(
        signals.len(),
        1,
        "novelty should fire only for the changed file, got: {signals:?}"
    );
    assert_eq!(signals[0].anchor.file, "changed.rs");
    assert_eq!(signals[0].anchor.symbol.as_deref(), Some("delta"));
}

#[test]
fn novelty_stays_quiet_when_changed_symbols_empty() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    let [alpha, beta, gamma, delta] = distinct_bodies();
    ctx.new_functions.insert(
        PathBuf::from("changed.rs"),
        vec![
            fdef("alpha", alpha),
            fdef("beta", beta),
            fdef("gamma", gamma),
            fdef("delta", delta),
        ],
    );
    ctx.changed_paths.insert(PathBuf::from("changed.rs"));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
    assert!(
        signals.is_empty(),
        "empty changed_symbols must not fall back to changed_paths: {signals:?}"
    );
}

#[test]
fn novelty_scoped_to_changed_symbols() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    let [alpha, beta, gamma, delta] = distinct_bodies();
    ctx.new_functions.insert(
        PathBuf::from("changed.rs"),
        vec![
            fdef("alpha", alpha),
            fdef("beta", beta),
            fdef("gamma", gamma),
            fdef("delta", delta),
        ],
    );
    ctx.changed_paths.insert(PathBuf::from("changed.rs"));
    ctx.changed_symbols.insert((
        PathBuf::from("changed.rs"),
        fdef("delta", "").symbol_identity(),
    ));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
    assert_eq!(
        signals.len(),
        1,
        "novelty must not fire on untouched siblings: {signals:?}"
    );
    assert_eq!(signals[0].anchor.symbol.as_deref(), Some("delta"));
}

#[test]
fn novelty_stays_quiet_when_corpus_incomplete() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    ctx.corpus_complete = false;
    let [alpha, beta, gamma, delta] = distinct_bodies();
    ctx.new_functions.insert(
        PathBuf::from("changed.rs"),
        vec![
            fdef("alpha", alpha),
            fdef("beta", beta),
            fdef("gamma", gamma),
            fdef("delta", delta),
        ],
    );
    ctx.changed_paths.insert(PathBuf::from("changed.rs"));
    ctx.changed_symbols.insert((
        PathBuf::from("changed.rs"),
        fdef("delta", "").symbol_identity(),
    ));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
    assert!(
        signals.is_empty(),
        "incomplete corpus must fail-closed: {signals:?}"
    );
}

#[test]
fn same_shape_renamed_identifiers_do_not_fire_novelty() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        PathBuf::from("users.rs"),
        vec![fdef("get_user", "fn get_user() { fetch_user(); }")],
    );
    ctx.new_functions.insert(
        PathBuf::from("orders.rs"),
        vec![fdef("get_order", "fn get_order() { fetch_order(); }")],
    );
    ctx.new_functions.insert(
        PathBuf::from("a.rs"),
        vec![fdef(
            "alpha",
            "fn alpha() { let total = first + second + third + fourth; }",
        )],
    );
    ctx.new_functions.insert(
        PathBuf::from("b.rs"),
        vec![fdef(
            "beta",
            "fn beta() { for widget in inventory { ship(widget); } }",
        )],
    );
    ctx.changed_paths.insert(PathBuf::from("users.rs"));
    ctx.changed_symbols.insert((
        PathBuf::from("users.rs"),
        fdef("get_user", "").symbol_identity(),
    ));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
    assert!(
        signals.is_empty(),
        "same-shape renamed identifiers must not fire novelty: {signals:?}"
    );
}

#[test]
fn unknown_language_fail_closes_without_novelty() {
    let cfg = ReviewSignalsConfig::default();
    let mut ctx = SemanticContext::new();
    ctx.new_functions.insert(
        PathBuf::from("notes.txt"),
        vec![fdef("get_user", "fn get_user() { fetch_user(); }")],
    );
    ctx.new_functions.insert(
        PathBuf::from("a.rs"),
        vec![fdef(
            "alpha",
            "fn alpha() { let total = first + second + third + fourth; }",
        )],
    );
    ctx.new_functions.insert(
        PathBuf::from("b.rs"),
        vec![fdef(
            "beta",
            "fn beta() { for widget in inventory { ship(widget); } }",
        )],
    );
    ctx.new_functions.insert(
        PathBuf::from("c.rs"),
        vec![fdef(
            "gamma",
            "fn gamma() { match colour { Red => stop(), Green => go() } }",
        )],
    );
    ctx.changed_paths.insert(PathBuf::from("notes.txt"));
    ctx.changed_symbols.insert((
        PathBuf::from("notes.txt"),
        fdef("get_user", "").symbol_identity(),
    ));

    let signals = run(&empty_state(), &empty_state(), &cfg, &ctx);
    assert!(
        signals.is_empty(),
        "unknown language must fail-closed without novelty: {signals:?}"
    );
}
