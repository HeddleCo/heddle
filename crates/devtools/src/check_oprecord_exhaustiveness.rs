// SPDX-License-Identifier: Apache-2.0
//! Workspace-wide AST asserter for the **non-exhaustive `OpRecord` consumer**
//! bug class (heddle#354 r9).
//!
//! ## The bug class
//!
//! Every consumer that `match`es over the `OpRecord` variant set (or a future
//! emitted-kind enum like `OpKind`) must handle new variants **exhaustively**.
//! A bare `_ => …` wildcard arm silently swallows any variant added later —
//! the reconciler folds it as a no-op, undo/redo skips it, the op-log query
//! drops it from the default view. Each round of review kept finding the *next*
//! consumer that missed the *previous* round's new variant (a drip). The fix is
//! structural: no production `match` over `OpRecord`/`OpKind` may use a
//! catch-all arm, so `rustc`'s own exhaustiveness check fails the build the
//! moment a variant is added until every consumer is updated.
//!
//! Predicates that genuinely want a default (`matches!(op, OpRecord::X { .. })`,
//! `op.verb()`) are fine: `matches!` expands to a macro (not an `ExprMatch` in
//! the source AST) and `verb()`/`is_checkpoint_verb()` are exhaustive matches in
//! the oplog catalog. Only a literal `match` expression with an unguarded
//! catch-all arm over a target enum is a hit.
//!
//! ## Why an AST asserter (vs. trusting rustc alone)
//!
//! `rustc` exhaustiveness only bites a match that has *no* wildcard. A single
//! `_ => {}` re-opens the whole class and compiles cleanly forever. This gate
//! fails CI if any production consumer reintroduces a wildcard over a target
//! enum, so the rustc teeth can never be filed off. A planted wildcard (see the
//! tests) proves the analyzer is non-vacuous.
//!
//! The env-var contract mirrors the sibling asserters:
//! `HEDDLE_OPRECORD_EXHAUSTIVENESS_SEARCH_DIRS` (colon-separated, default
//! `crates`) and `HEDDLE_OPRECORD_EXHAUSTIVENESS_ALLOWLIST` (semicolon-separated
//! `path:line`; empty disables, unset uses the built-in default — currently
//! empty, because there are no legitimate exceptions).

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{
    Attribute, ExprMatch, ImplItemFn, ItemFn, ItemMod, Meta, Pat, spanned::Spanned, visit::Visit,
};

use crate::asserter::{for_each_rs_file, read_allowlist, read_search_dirs};

/// Enums whose `match` consumers must be exhaustive. `OpKind` is listed
/// proactively so a future emitted-kind enum is covered the day it lands.
const TARGET_ENUMS: &[&str] = &["OpRecord", "OpKind"];

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!(
            "check-oprecord-exhaustiveness: unexpected argument '{arg}' (configured via env vars: \
HEDDLE_OPRECORD_EXHAUSTIVENESS_SEARCH_DIRS, HEDDLE_OPRECORD_EXHAUSTIVENESS_ALLOWLIST)"
        );
    }

    check(
        &read_search_dirs("HEDDLE_OPRECORD_EXHAUSTIVENESS_SEARCH_DIRS"),
        &read_allowlist("HEDDLE_OPRECORD_EXHAUSTIVENESS_ALLOWLIST"),
    )
}

/// The testable core: scan `search_dirs`, filter hits by `allowlist`, print
/// findings, and `bail!` if any non-allowlisted wildcard arm over a target enum
/// remains. `run()` is the thin env-reading wrapper around this.
fn check(search_dirs: &[PathBuf], allowlist: &[String]) -> Result<()> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut files_scanned = 0usize;
    for dir in search_dirs {
        scan_dir(dir, &mut hits, &mut files_scanned)
            .with_context(|| format!("scan {}", dir.display()))?;
    }

    let mut failed = 0usize;
    for hit in &hits {
        let key = format!("{}:{}", hit.path.display(), hit.line);
        if allowlist.iter().any(|entry| entry == &key) {
            println!("ok: exempt: {key} — {}", hit.snippet.trim());
            continue;
        }
        eprintln!(
            "::error::non-exhaustive {} match at {key}: a wildcard arm (`{}`) silently swallows \
unhandled variants — {}",
            hit.enum_name,
            hit.arm,
            hit.snippet.trim()
        );
        failed += 1;
    }

    if failed > 0 {
        eprintln!(
            "\n::error::Found {failed} non-exhaustive match(es) over a target enum ({}). A \
production `match` over these enums must name every variant (group no-op variants in an explicit \
`A | B | C => {{}}` arm), so adding a variant is a COMPILE error until every consumer is updated — \
that is what closes the \"new variant not propagated to every consumer\" class. Replace the \
catch-all `_ =>` arm with explicit per-variant arms.\n\
\n\
If a site is a legitimate exception, add a `path:line` entry (of the wildcard arm) to \
HEDDLE_OPRECORD_EXHAUSTIVENESS_ALLOWLIST with a one-line justification.",
            TARGET_ENUMS.join("/")
        );
        bail!("asserter failed");
    }

    println!(
        "asserter clean: every production match over {} is exhaustive ({files_scanned} file(s) \
scanned)",
        TARGET_ENUMS.join("/")
    );
    Ok(())
}

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    enum_name: String,
    arm: String,
    snippet: String,
}

fn scan_dir(dir: &Path, hits: &mut Vec<Hit>, files_scanned: &mut usize) -> Result<()> {
    for_each_rs_file(dir, files_scanned, is_test_path, |path, source| {
        let file = syn::parse_file(source).with_context(|| format!("parse {}", path.display()))?;
        let lines: Vec<&str> = source.lines().collect();
        let mut visitor = Finder {
            path: path.to_path_buf(),
            lines: &lines,
            hits,
        };
        visitor.visit_file(&file);
        Ok(())
    })
}

/// Skip test code: tests legitimately plant wildcard arms (this asserter's own
/// tests do). Mirrors the sibling asserter — a `tests/` path segment, a
/// `*_tests.rs` / `tests.rs` file, and inline `#[cfg(test)]` items (handled in
/// the visitor).
fn is_test_path(path: &Path) -> bool {
    for component in path.components() {
        if component.as_os_str() == "tests" {
            return true;
        }
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.ends_with("_tests.rs") || name == "tests.rs")
        .unwrap_or(false)
}

/// True iff the item carries a `#[cfg(test)]` attribute (directly, or nested in
/// an `all(...)` / `any(...)` predicate).
fn is_cfg_test(attrs: &[Attribute]) -> bool {
    fn meta_mentions_test(meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => path.is_ident("test"),
            Meta::List(list) if list.path.is_ident("cfg") => {
                list.tokens.to_string().contains("test")
            }
            Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
                list.tokens.to_string().contains("test")
            }
            _ => false,
        }
    }
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && meta_mentions_test(&attr.meta))
}

struct Finder<'a> {
    path: PathBuf,
    lines: &'a [&'a str],
    hits: &'a mut Vec<Hit>,
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        syn::visit::visit_impl_item_fn(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        if let Some(enum_name) = matched_target_enum(node) {
            for arm in &node.arms {
                // A guarded arm (`_ if cond =>`) does not make the match
                // exhaustive on its own, so it is not a silent-swallow: other
                // arms must still cover the variants. Only an UNGUARDED
                // catch-all swallows unhandled variants.
                if arm.guard.is_none() && pat_is_catch_all(&arm.pat) {
                    let line = arm.pat.span().start().line;
                    let snippet = self
                        .lines
                        .get(line.saturating_sub(1))
                        .copied()
                        .unwrap_or("")
                        .to_string();
                    self.hits.push(Hit {
                        path: self.path.clone(),
                        line,
                        enum_name: enum_name.clone(),
                        arm: pat_text(&arm.pat),
                        snippet,
                    });
                }
            }
        }
        // Recurse so nested matches (and matches inside the arms) are checked.
        syn::visit::visit_expr_match(self, node);
    }
}

/// If any arm pattern names a `TARGET_ENUMS` variant, the match is "over" that
/// enum — return its name. Catches `OpRecord::Snapshot { .. }`,
/// `OpRecord::Fork(..)`, and or-patterns / refs / parens thereof.
fn matched_target_enum(node: &ExprMatch) -> Option<String> {
    node.arms.iter().find_map(|arm| pat_target_enum(&arm.pat))
}

fn pat_target_enum(pat: &Pat) -> Option<String> {
    let path = match pat {
        Pat::Path(p) => Some(&p.path),
        Pat::TupleStruct(ts) => Some(&ts.path),
        Pat::Struct(s) => Some(&s.path),
        Pat::Or(or) => return or.cases.iter().find_map(pat_target_enum),
        Pat::Paren(p) => return pat_target_enum(&p.pat),
        Pat::Reference(r) => return pat_target_enum(&r.pat),
        _ => None,
    }?;
    let first = path.segments.first()?.ident.to_string();
    TARGET_ENUMS
        .iter()
        .find(|name| **name == first)
        .map(|name| (*name).to_string())
}

/// True iff this arm pattern catches every remaining value: a `_` wildcard or a
/// bare lowercase binding (`other => …`), including inside or-patterns / parens
/// / references. An uppercase bare ident is treated as a (unit) variant/const,
/// not a catch-all, so we do not misclassify it.
fn pat_is_catch_all(pat: &Pat) -> bool {
    match pat {
        Pat::Wild(_) => true,
        Pat::Ident(p) => p.subpat.is_none() && is_binding_ident(&p.ident.to_string()),
        Pat::Or(or) => or.cases.iter().any(pat_is_catch_all),
        Pat::Paren(p) => pat_is_catch_all(&p.pat),
        Pat::Reference(r) => pat_is_catch_all(&r.pat),
        _ => false,
    }
}

/// A binding identifier (catch-all) starts with `_` or a lowercase letter;
/// variants/consts conventionally start uppercase.
fn is_binding_ident(ident: &str) -> bool {
    ident
        .chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_lowercase())
}

/// A short human description of the catch-all arm for the error message.
/// Deliberately avoids a token-stream pretty-printer (no `quote` dependency);
/// the snippet line carries the full context.
fn pat_text(pat: &Pat) -> String {
    match pat {
        Pat::Wild(_) => "_".to_string(),
        Pat::Ident(p) => p.ident.to_string(),
        Pat::Or(or) => or
            .cases
            .iter()
            .map(pat_text)
            .collect::<Vec<_>>()
            .join(" | "),
        Pat::Paren(p) => pat_text(&p.pat),
        Pat::Reference(r) => format!("&{}", pat_text(&r.pat)),
        _ => "<catch-all>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scan_source(src: &str) -> Vec<Hit> {
        let file = syn::parse_file(src).expect("parse");
        let lines: Vec<&str> = src.lines().collect();
        let mut hits = Vec::new();
        let mut v = Finder {
            path: PathBuf::from("test.rs"),
            lines: &lines,
            hits: &mut hits,
        };
        v.visit_file(&file);
        hits
    }

    #[test]
    fn flags_wildcard_arm_over_oprecord() {
        let hits = scan_source(
            "fn f(op: &OpRecord) { match op { \
                OpRecord::Snapshot { .. } => a(), \
                _ => {} \
            } }",
        );
        assert_eq!(hits.len(), 1, "wildcard over OpRecord must be flagged");
        assert_eq!(hits[0].enum_name, "OpRecord");
    }

    #[test]
    fn flags_bare_binding_catch_all() {
        let hits = scan_source(
            "fn f(op: &OpRecord) -> u8 { match op { \
                OpRecord::Goto { .. } => 1, \
                other => g(other), \
            } }",
        );
        assert_eq!(hits.len(), 1, "bare binding catch-all must be flagged");
    }

    #[test]
    fn flags_wildcard_inside_or_pattern() {
        let hits = scan_source(
            "fn f(op: &OpRecord) { match op { \
                OpRecord::Snapshot { .. } | _ => {} \
            } }",
        );
        assert_eq!(
            hits.len(),
            1,
            "wildcard inside an or-pattern must be flagged"
        );
    }

    #[test]
    fn flags_o_p_kind_match_too() {
        let hits = scan_source("fn f(k: OpKind) { match k { OpKind::Snapshot => a(), _ => {} } }");
        assert_eq!(hits.len(), 1, "OpKind is also a target enum");
        assert_eq!(hits[0].enum_name, "OpKind");
    }

    #[test]
    fn ignores_exhaustive_match() {
        let hits = scan_source(
            "fn f(op: &OpRecord) { match op { \
                OpRecord::Snapshot { .. } => a(), \
                OpRecord::Goto { .. } | OpRecord::Fork { .. } => b(), \
            } }",
        );
        assert!(hits.is_empty(), "an exhaustive match must not be flagged");
    }

    #[test]
    fn ignores_guarded_catch_all() {
        // A guarded arm does not make the match exhaustive by itself; the other
        // arms still have to cover the variants, so rustc keeps the teeth.
        let hits = scan_source(
            "fn f(op: &OpRecord) { match op { \
                OpRecord::Snapshot { .. } => a(), \
                _ if cond() => b(), \
                OpRecord::Goto { .. } => c(), \
            } }",
        );
        assert!(
            hits.is_empty(),
            "a guarded catch-all is not a silent swallow"
        );
    }

    #[test]
    fn ignores_wildcard_over_non_target_enum() {
        // A `match` over a string/other enum legitimately uses `_`.
        let hits =
            scan_source("fn f(kind: &str) -> u8 { match kind { \"snapshot\" => 1, _ => 0 } }");
        assert!(hits.is_empty(), "non-target matches keep their wildcard");
    }

    #[test]
    fn ignores_matches_macro() {
        // `matches!` expands to a macro, not an `ExprMatch`, so its internal
        // `_ => false` is invisible to the source AST — the idiomatic predicate
        // stays allowed.
        let hits =
            scan_source("fn f(op: &OpRecord) -> bool { matches!(op, OpRecord::Snapshot { .. }) }");
        assert!(hits.is_empty(), "matches! predicate must not be flagged");
    }

    #[test]
    fn ignores_inline_cfg_test_module() {
        let hits = scan_source(
            "fn prod() {} \
             #[cfg(test)] mod tests { \
                fn t(op: &OpRecord) { match op { OpRecord::Snapshot { .. } => a(), _ => {} } } \
             }",
        );
        assert!(
            hits.is_empty(),
            "inline #[cfg(test)] module must be skipped"
        );
    }

    const PLANTED: &str = "fn f(op: &OpRecord) { match op { \
            OpRecord::Snapshot { .. } => a(), \
            _ => {} \
        } }";

    #[test]
    fn check_bails_on_planted_site_and_exempts_via_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bypass.rs"), PLANTED).unwrap();
        let dirs = vec![dir.path().to_path_buf()];

        // No allowlist → the planted wildcard fails the gate (non-vacuous).
        assert!(
            check(&dirs, &[]).is_err(),
            "a planted wildcard over OpRecord must fail the check"
        );

        // Allowlisting the wildcard arm's line exempts it (the `_ =>` is line 1).
        let key = format!("{}:1", dir.path().join("bypass.rs").display());
        assert!(
            check(&dirs, &[key]).is_ok(),
            "an allowlisted site must pass the check"
        );
    }

    #[test]
    fn check_passes_on_clean_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("clean.rs"),
            "fn f(op: &OpRecord) { match op { \
                OpRecord::Snapshot { .. } => a(), \
                OpRecord::Goto { .. } => b(), \
            } }",
        )
        .unwrap();
        assert!(check(&[dir.path().to_path_buf()], &[]).is_ok());
    }

    /// Enforcement test: scan the REAL workspace tree with the built-in (empty)
    /// allowlist and assert clean. This makes the gate fail CI under
    /// `cargo test --workspace` if a non-exhaustive `OpRecord`/`OpKind` match is
    /// introduced in production code.
    #[test]
    fn production_tree_oprecord_matches_are_exhaustive() {
        let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("devtools crate dir has a parent (the crates/ dir)")
            .to_path_buf();
        let mut hits = Vec::new();
        let mut scanned = 0usize;
        scan_dir(&crates_dir, &mut hits, &mut scanned).expect("scan crates/");
        assert!(
            scanned > 0,
            "expected to scan some files under {crates_dir:?}"
        );
        assert!(
            hits.is_empty(),
            "non-exhaustive OpRecord/OpKind match(es) found: {:?}",
            hits.iter()
                .map(|h| format!(
                    "{}:{} ({} arm `{}`)",
                    h.path.display(),
                    h.line,
                    h.enum_name,
                    h.arm
                ))
                .collect::<Vec<_>>()
        );
    }
}
