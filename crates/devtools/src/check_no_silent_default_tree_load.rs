// SPDX-License-Identifier: Apache-2.0
//! AST-based asserter for the heddle#90/#93 silent-default-tree-load bug
//! class. Retires the `scripts/check-no-silent-default-tree-load.sh` regex
//! after three rounds of regex-vs-edge-cases (heddle#103).
//!
//! The walker uses `syn::parse_file` on each `.rs` source under the search
//! dirs, then visits every `ExprMethodCall`. A call is a hit when its method
//! matches one of `unwrap_or_default`, `unwrap_or_else(|| Tree::new() /
//! Tree::default())` (closure body, optionally braced), or
//! `unwrap_or_else(Tree::new / Tree::default)` (fn-pointer arg) AND its
//! receiver chain — peeled through `?`, parens, and intermediate method
//! calls like `.ok()`/`.flatten()`/`.transpose()` — bottoms out at a
//! `MethodCall` named `get_tree`.
//!
//! Doc-comments and string literals are exempt by construction: they are not
//! `ExprMethodCall` nodes. Macro-call wrappers (`get_tree_macro!(x)`) are NOT
//! peered through — the AST sees `ExprMacro`, not the expansion. This is a
//! documented limitation; the dispatch-site fixture asserts the behavior.
//!
//! The env-var contract is identical to the legacy shell script so the
//! mutation-test harness drives the same way. `HEDDLE_ASSERTER_SEARCH_DIRS`
//! is a colon-separated list of dirs (default `crates`).
//! `HEDDLE_ASSERTER_ALLOWLIST` is a semicolon-separated list of `path:line`
//! entries; empty string disables the list, unset uses the built-in default.
//! The built-in default is empty: the legacy doc-comment pins are no longer
//! needed because the AST walker doesn't visit doc-comments.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{Expr, ExprMethodCall, Stmt, visit::Visit};

use crate::asserter::{for_each_rs_file, read_allowlist, read_search_dirs};

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!(
            "check-no-silent-default-tree-load: unexpected argument '{arg}' (configured via env vars: HEDDLE_ASSERTER_SEARCH_DIRS, HEDDLE_ASSERTER_ALLOWLIST)"
        );
    }

    let search_dirs = read_search_dirs("HEDDLE_ASSERTER_SEARCH_DIRS");
    let allowlist = read_allowlist("HEDDLE_ASSERTER_ALLOWLIST");

    let mut hits: Vec<Hit> = Vec::new();
    let mut files_scanned = 0usize;
    for dir in &search_dirs {
        scan_dir(dir, &mut hits, &mut files_scanned)
            .with_context(|| format!("scan {}", dir.display()))?;
    }

    let mut failed = 0usize;
    for hit in &hits {
        let key = format!("{}:{}", hit.path.display(), hit.line);
        if allowlist.iter().any(|entry| entry == &key) {
            println!("ok: exempt: {key} — {}", hit.label);
            continue;
        }
        eprintln!("::error::{} at {}: {}", hit.label, key, hit.snippet.trim(),);
        failed += 1;
    }

    if failed > 0 {
        eprintln!(
            "\n::error::Found {failed} `get_tree(...)?.unwrap_or_default()`-class \
site(s) in production code. This pattern silently substitutes \
`Tree::default()` for a missing subtree (heddle#90 merge / heddle#93 \
non-merge). Replace with `repo.require_tree(&hash)?` so missing trees \
surface with a `heddle fsck --full` recovery hint.\n\
\n\
If a site is a legitimate empty-tree sentinel (no-parent-commit marker, \
etc.) add a `path:line` entry to HEDDLE_ASSERTER_ALLOWLIST with a \
one-line justification."
        );
        bail!("asserter failed");
    }

    println!(
        "asserter clean: no silent-default tree load sites in production code \
({files_scanned} file(s) scanned)",
    );
    Ok(())
}

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    label: &'static str,
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

/// Tests legitimately exercise the bug class — skip them. This formalizes the
/// stated intent of the legacy shell asserter, whose `?`-requirement was an
/// incidental filter that happened to skip the test-style `.unwrap()` chains.
///
/// Two shapes:
///   - integration tests live under a `tests/` directory segment, and
///   - unit tests by convention sit in `*_tests.rs` files under `src/`.
fn is_test_path(path: &Path) -> bool {
    for component in path.components() {
        if component.as_os_str() == "tests" {
            return true;
        }
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.ends_with("_tests.rs"))
        .unwrap_or(false)
}

struct Finder<'a> {
    path: PathBuf,
    lines: &'a [&'a str],
    hits: &'a mut Vec<Hit>,
}

impl<'a, 'ast> Visit<'ast> for Finder<'a> {
    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if let Some(label) = classify(node)
            && chain_originates_from_get_tree(&node.receiver)
        {
            let line = node.method.span().start().line;
            let snippet = self
                .lines
                .get(line.saturating_sub(1))
                .copied()
                .unwrap_or("")
                .to_string();
            self.hits.push(Hit {
                path: self.path.clone(),
                line,
                label,
                snippet,
            });
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn classify(node: &ExprMethodCall) -> Option<&'static str> {
    let method = node.method.to_string();
    match method.as_str() {
        "unwrap_or_default" => Some("silent-default tree load (heddle#90/#93 bug class)"),
        "unwrap_or_else" => {
            let arg = node.args.first()?;
            if closure_returns_tree_default(arg) {
                Some("silent-default tree load via unwrap_or_else(closure)")
            } else if path_is_tree_default(arg) {
                Some("silent-default tree load via unwrap_or_else(fn-pointer)")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn closure_returns_tree_default(arg: &Expr) -> bool {
    let Expr::Closure(closure) = arg else {
        return false;
    };
    expr_constructs_tree(&closure.body)
}

/// Recognize an expression that constructs a default `Tree` — either
/// `Tree::new()` / `Tree::default()` directly, or a block whose tail
/// expression is one of those.
fn expr_constructs_tree(expr: &Expr) -> bool {
    match expr {
        Expr::Call(call) => path_is_tree_default(&call.func),
        Expr::Block(b) => match b.block.stmts.last() {
            Some(Stmt::Expr(tail, None)) => expr_constructs_tree(tail),
            _ => false,
        },
        Expr::Paren(p) => expr_constructs_tree(&p.expr),
        Expr::Group(g) => expr_constructs_tree(&g.expr),
        _ => false,
    }
}

fn path_is_tree_default(expr: &Expr) -> bool {
    let Expr::Path(p) = expr else { return false };
    let segs = &p.path.segments;
    if segs.len() < 2 {
        return false;
    }
    let last = segs[segs.len() - 1].ident.to_string();
    let prev = segs[segs.len() - 2].ident.to_string();
    prev == "Tree" && (last == "new" || last == "default")
}

/// True iff the receiver chain transitively contains a `MethodCall` whose
/// method ident is `get_tree`. Walks through intermediate method calls
/// (treating `.ok()`, `.flatten()`, `.transpose()`, etc. as transparent) and
/// peels off `?`, parens, and groups. Stops when the chain hits anything that
/// isn't one of those expression shapes — at which point we know `get_tree`
/// is not the origin.
fn chain_originates_from_get_tree(expr: &Expr) -> bool {
    let mut cur = expr;
    loop {
        match cur {
            Expr::MethodCall(mc) => {
                if mc.method == "get_tree" {
                    return true;
                }
                cur = &mc.receiver;
            }
            Expr::Try(t) => cur = &t.expr,
            Expr::Paren(p) => cur = &p.expr,
            Expr::Group(g) => cur = &g.expr,
            Expr::Await(a) => cur = &a.base,
            _ => return false,
        }
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
    fn flags_direct_unwrap_or_default() {
        let hits = scan_source("fn f() -> Tree { repo.get_tree(h)?.unwrap_or_default() }");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_chain_through_ok_flatten() {
        let hits =
            scan_source("fn f() -> Tree { repo.get_tree(h).ok().flatten().unwrap_or_default() }");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_option_chain_transpose() {
        let hits = scan_source(
            "fn f() -> Tree { repo.get_tree(h).transpose()?.flatten().unwrap_or_default() }",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_unwrap_or_else_closure_tree_new() {
        let hits =
            scan_source("fn f() -> Tree { repo.get_tree(h)?.unwrap_or_else(|| Tree::new()) }");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_unwrap_or_else_braced_closure() {
        let hits = scan_source(
            "fn f() -> Tree { repo.get_tree(h)?.unwrap_or_else(|| { Tree::default() }) }",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_unwrap_or_else_fn_pointer() {
        let hits =
            scan_source("fn f() -> Tree { repo.get_tree(h)?.unwrap_or_else(Tree::default) }");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_nested_paren_args() {
        let hits = scan_source(
            "fn f() -> Tree { repo.get_tree(&normalize(s.tree()))?.unwrap_or_default() }",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_triple_nested_parens() {
        let hits = scan_source("fn f() -> Tree { repo.get_tree(((id)))?.unwrap_or_default() }");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn ignores_raw_string_literal() {
        let hits =
            scan_source("fn f() { let s = r#\"get_tree(x)?.unwrap_or_default()\"#; let _ = s; }");
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_unrelated_unwrap_or_default() {
        let hits = scan_source("fn f() -> Vec<u8> { vec.something().unwrap_or_default() }");
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_unwrap_or_else_with_unrelated_default() {
        let hits = scan_source("fn f() -> Tree { repo.get_tree(h)?.unwrap_or_else(|| other()) }");
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_get_tree_without_terminal_unwrap() {
        let hits = scan_source("fn f() { let _ = repo.get_tree(h)?; }");
        assert!(hits.is_empty());
    }

    #[test]
    fn does_not_peer_through_macro() {
        let hits = scan_source("fn f() -> Tree { get_tree_macro!(h)?.unwrap_or_default() }");
        assert!(
            hits.is_empty(),
            "macro-wrapped call is a documented limitation"
        );
    }
}
