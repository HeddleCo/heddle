// SPDX-License-Identifier: Apache-2.0
//! Workspace-wide AST asserter for the **rewind-ledger encapsulation** invariant
//! (heddle#355).
//!
//! ## The invariant
//!
//! The reverse-order rewind ledger is registered through exactly one safe
//! combinator, [`Tx::step`](../../repo/src/atomic/tx.rs), which runs the forward
//! effect FIRST and pushes the inverse onto the ledger only on success. The raw
//! register primitive, `Tx::on_rewind`, has NO ordering enforcement — calling it
//! directly lets a caller queue a compensator *before* (or *without*) its
//! forward effect, the register-then-forward footgun that corrupted pre-existing
//! refs on rollback (cid 3330867774 / 3330867775).
//!
//! `on_rewind` is `pub(crate)` in the `repo` crate, so the compiler already
//! makes a cross-crate call a hard error. This asserter is the belt to that
//! suspenders: it fails CI if *any* call to `on_rewind` appears OUTSIDE
//! `crates/repo/src/atomic/` — catching an in-crate (but out-of-module) use, or
//! a future regression that re-widens the primitive's visibility. The only
//! legitimate callers (`step`, `enroll`, `enroll_whole_op`) all live inside
//! `crates/repo/src/atomic/`, which the walk skips; a planted out-of-module
//! `on_rewind` (see the tests) proves the analyzer is non-vacuous.
//!
//! The env-var contract mirrors the sibling asserters:
//! `HEDDLE_LEDGER_ENCAP_SEARCH_DIRS` (colon-separated, default `crates`) and
//! `HEDDLE_LEDGER_ENCAP_ALLOWLIST` (semicolon-separated `path:line`; empty
//! disables, unset uses the built-in default — currently empty).

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{
    Attribute, ImplItemFn, ItemFn, ItemMod, Meta, MetaList, Token, parse::Parser,
    punctuated::Punctuated, visit::Visit,
};
use walkdir::WalkDir;

const DEFAULT_SEARCH_DIRS: &[&str] = &["crates"];

/// The raw ledger-registration method. Any call to a method of this name outside
/// the atomic module is a hit (over-approximating toward flagging is the safe
/// direction for an encapsulation gate — `on_rewind` is a sufficiently specific
/// name that a same-named method on an unrelated type is implausible).
const LEDGER_METHOD: &str = "on_rewind";

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!(
            "check-atomic-ledger-encapsulation: unexpected argument '{arg}' (configured via env \
vars: HEDDLE_LEDGER_ENCAP_SEARCH_DIRS, HEDDLE_LEDGER_ENCAP_ALLOWLIST)"
        );
    }

    check(&read_search_dirs(), &read_allowlist())
}

/// The testable core: scan `search_dirs`, filter hits by `allowlist`, print
/// findings, and `bail!` if any non-allowlisted external `on_rewind` use remains.
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
            "::error::raw rewind-ledger registration at {key}: `{LEDGER_METHOD}` is called outside \
`crates/repo/src/atomic/` — {}",
            hit.snippet.trim()
        );
        failed += 1;
    }

    if failed > 0 {
        eprintln!(
            "\n::error::Found {failed} out-of-module `{LEDGER_METHOD}` call site(s). The rewind \
ledger must be registered through the forward-first `Tx::step` combinator (or `Tx::enroll`), never \
the raw `{LEDGER_METHOD}` primitive, which has no ordering enforcement and lets a compensator be \
queued before its forward effect runs (heddle#355 cid 3330867774 / 3330867775). Replace the \
`{LEDGER_METHOD}` call with `tx.step(forward, inverse)`.\n\
\n\
If a site is a legitimate exception, add a `path:line` entry (of the call) to \
HEDDLE_LEDGER_ENCAP_ALLOWLIST with a one-line justification."
        );
        bail!("asserter failed");
    }

    println!(
        "asserter clean: no out-of-module `{LEDGER_METHOD}` call sites in production code \
({files_scanned} file(s) scanned)"
    );
    Ok(())
}

fn read_search_dirs() -> Vec<PathBuf> {
    match env::var("HEDDLE_LEDGER_ENCAP_SEARCH_DIRS") {
        Ok(value) if !value.is_empty() => value.split(':').map(PathBuf::from).collect(),
        _ => DEFAULT_SEARCH_DIRS.iter().map(PathBuf::from).collect(),
    }
}

fn read_allowlist() -> Vec<String> {
    match env::var("HEDDLE_LEDGER_ENCAP_ALLOWLIST") {
        Ok(value) if value.is_empty() => Vec::new(),
        Ok(value) => value.split(';').map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    snippet: String,
}

fn scan_dir(dir: &Path, hits: &mut Vec<Hit>, files_scanned: &mut usize) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.with_context(|| format!("walkdir under {}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(OsStr::to_str) != Some("rs") {
            continue;
        }
        // The atomic module IS the ledger's home — `step`/`enroll`/
        // `enroll_whole_op` legitimately call `on_rewind` there.
        if is_atomic_module_path(path) {
            continue;
        }
        // Tests legitimately drive the raw primitive to exercise ledger
        // behavior; skip them.
        if is_test_path(path) {
            continue;
        }
        let source =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        *files_scanned += 1;
        let file = syn::parse_file(&source).with_context(|| format!("parse {}", path.display()))?;
        let lines: Vec<&str> = source.lines().collect();
        let mut visitor = Finder {
            path: path.to_path_buf(),
            lines: &lines,
            hits,
        };
        visitor.visit_file(&file);
    }
    Ok(())
}

/// True iff the path is inside the atomic module (`.../repo/src/atomic/...`),
/// the one place the raw ledger primitive is allowed. Matched on the consecutive
/// `repo`/`src`/`atomic` component triple so a substring or a sibling `atomic`
/// dir elsewhere doesn't accidentally exempt code.
fn is_atomic_module_path(path: &Path) -> bool {
    let components: Vec<&OsStr> = path.iter().collect();
    components.windows(3).any(|w| {
        w[0] == OsStr::new("repo") && w[1] == OsStr::new("src") && w[2] == OsStr::new("atomic")
    })
}

/// Tests legitimately exercise the raw `on_rewind` primitive. Three shapes:
/// integration tests under a `tests/` segment, unit-test files named
/// `*_tests.rs`, and submodule test files named exactly `tests.rs`. Inline
/// `#[cfg(test)] mod tests { ... }` blocks are skipped separately at the AST
/// level (see `is_cfg_test`).
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

/// True iff the item is genuinely *test-only* — compiled out of every non-test
/// build — and so legitimately allowed to drive the raw primitive.
///
/// This is NOT a substring match on `test`: `#[cfg(not(test))]` mentions `test`
/// but is *production* code, and skipping it was a real bypass of the exact
/// `on_rewind` call the gate guards (cid 3330966933). We parse the `cfg`
/// predicate and skip the item only when it can NEVER be active with the `test`
/// flag unset. Over-approximating toward *scanning* (the false-positive direction)
/// is the safe bias for an encapsulation gate, so an unparseable or unknown
/// predicate is treated as live.
fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && cfg_is_test_only(&attr.meta))
}

/// Evaluate a `#[cfg(...)]` attribute's predicate: is the gated item excluded
/// from all non-test builds (i.e. test-only)?
fn cfg_is_test_only(cfg_meta: &Meta) -> bool {
    let Meta::List(list) = cfg_meta else {
        return false;
    };
    if !list.path.is_ident("cfg") {
        return false;
    }
    match syn::parse2::<Meta>(list.tokens.clone()) {
        // Test-only iff the predicate can never be true once `test` is unset.
        Ok(inner) => !pred_can_be_true_without_test(&inner),
        // Unparseable predicate → don't exempt it (scan, the safe direction).
        Err(_) => false,
    }
}

/// Can this `cfg` predicate be satisfied in a build where `test` is NOT set
/// (treating every other cfg atom as a free variable)? Mutually recursive with
/// [`pred_can_be_false_without_test`] to evaluate `not(...)`.
fn pred_can_be_true_without_test(meta: &Meta) -> bool {
    match meta {
        // The `test` atom is false in a non-test build; any other atom is free.
        Meta::Path(path) => !path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("not") => match cfg_children(list) {
            Some(children) if children.len() == 1 => {
                pred_can_be_false_without_test(&children[0])
            }
            _ => true,
        },
        Meta::List(list) if list.path.is_ident("all") => match cfg_children(list) {
            Some(children) => children.iter().all(pred_can_be_true_without_test),
            None => true,
        },
        Meta::List(list) if list.path.is_ident("any") => match cfg_children(list) {
            Some(children) => children.iter().any(pred_can_be_true_without_test),
            None => true,
        },
        // `feature = "x"`, `target_os = "y"`, or an unknown predicate fn: a free
        // atom that can be true in a non-test build.
        _ => true,
    }
}

/// Can this `cfg` predicate be *false* in a build where `test` is NOT set?
/// Dual of [`pred_can_be_true_without_test`], used to evaluate `not(...)`.
fn pred_can_be_false_without_test(meta: &Meta) -> bool {
    match meta {
        // `test` is false in prod (so the predicate is falsifiable); any other
        // atom is free and can be left unset.
        Meta::Path(_) => true,
        Meta::List(list) if list.path.is_ident("not") => match cfg_children(list) {
            Some(children) if children.len() == 1 => {
                pred_can_be_true_without_test(&children[0])
            }
            _ => true,
        },
        // `all` is false if any child can be false; `any` is false only if every
        // child can be false.
        Meta::List(list) if list.path.is_ident("all") => match cfg_children(list) {
            Some(children) => children.iter().any(pred_can_be_false_without_test),
            None => true,
        },
        Meta::List(list) if list.path.is_ident("any") => match cfg_children(list) {
            Some(children) => children.iter().all(pred_can_be_false_without_test),
            None => true,
        },
        _ => true,
    }
}

/// Parse the comma-separated child predicates of an `all(...)` / `any(...)` /
/// `not(...)` cfg combinator.
fn cfg_children(list: &MetaList) -> Option<Vec<Meta>> {
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .ok()
        .map(|punctuated| punctuated.into_iter().collect())
}

struct Finder<'a> {
    path: PathBuf,
    lines: &'a [&'a str],
    hits: &'a mut Vec<Hit>,
}

impl Finder<'_> {
    fn record(&mut self, line: usize) {
        let snippet = self
            .lines
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or("")
            .to_string();
        self.hits.push(Hit {
            path: self.path.clone(),
            line,
            snippet,
        });
    }
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

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == LEDGER_METHOD {
            self.record(node.method.span().start().line);
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    /// Catch the UFCS / free-function-call form, `Tx::on_rewind(tx, ...)` (or
    /// `<Tx>::on_rewind(...)`), which `visit_expr_method_call` never sees — a
    /// real bypass, since `on_rewind` is `pub(crate)` and any other `repo`
    /// module could reach it this way (cid 3330966935).
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(expr_path) = node.func.as_ref()
            && let Some(segment) = expr_path.path.segments.last()
            && segment.ident == LEDGER_METHOD
        {
            self.record(segment.ident.span().start().line);
        }
        syn::visit::visit_expr_call(self, node);
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
    fn flags_on_rewind_call() {
        let hits = scan_source(
            "fn stage(tx: &mut Tx) -> Result<()> { \
                tx.on_rewind(move || Ok(())); \
                do_forward()?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1, "an out-of-module on_rewind call must be flagged");
    }

    #[test]
    fn ignores_step_combinator() {
        // The fix shape: `tx.step(forward, inverse)` — no raw on_rewind.
        let hits = scan_source(
            "fn stage(tx: &mut Tx) -> Result<()> { \
                tx.step(|| do_forward(), move || Ok(()))?; \
                Ok(()) }",
        );
        assert!(hits.is_empty(), "the step combinator must not be flagged");
    }

    #[test]
    fn ignores_inline_cfg_test_module() {
        let hits = scan_source(
            "fn prod() {} \
             #[cfg(test)] \
             mod tests { \
                fn drives_primitive(tx: &mut Tx) { tx.on_rewind(|| Ok(())); } \
             }",
        );
        assert!(hits.is_empty(), "inline #[cfg(test)] module must be skipped");
    }

    #[test]
    fn ignores_string_literal() {
        let hits = scan_source("fn f() { let s = \"tx.on_rewind(x)\"; let _ = s; }");
        assert!(hits.is_empty(), "a string mentioning on_rewind is not a call");
    }

    #[test]
    fn atomic_module_path_is_recognized() {
        assert!(is_atomic_module_path(Path::new("crates/repo/src/atomic/tx.rs")));
        assert!(is_atomic_module_path(Path::new(
            "/work/crates/repo/src/atomic/tests.rs"
        )));
        // A sibling `atomic` dir under a different crate is NOT exempt.
        assert!(!is_atomic_module_path(Path::new("crates/cli/src/atomic/x.rs")));
        assert!(!is_atomic_module_path(Path::new(
            "crates/cli/src/cli/commands/undo_apply.rs"
        )));
    }

    const PLANTED: &str = "fn stage(tx: &mut Tx) -> Result<()> { \
            tx.on_rewind(move || Ok(())); \
            Ok(()) }";

    #[test]
    fn check_bails_on_planted_site_and_exempts_via_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        // A non-atomic, non-test path so neither exclusion fires.
        let crate_src = dir.path().join("cli/src");
        std::fs::create_dir_all(&crate_src).unwrap();
        std::fs::write(crate_src.join("bypass.rs"), PLANTED).unwrap();
        let dirs = vec![dir.path().to_path_buf()];

        assert!(
            check(&dirs, &[]).is_err(),
            "a planted out-of-module on_rewind site must fail the check"
        );

        let key = format!("{}:1", crate_src.join("bypass.rs").display());
        assert!(
            check(&dirs, &[key]).is_ok(),
            "an allowlisted site must pass the check"
        );
    }

    #[test]
    fn check_ignores_planted_site_inside_atomic_module() {
        let dir = tempfile::tempdir().unwrap();
        // Same planted call, but under `repo/src/atomic/` — the ledger's home.
        let atomic = dir.path().join("repo/src/atomic");
        std::fs::create_dir_all(&atomic).unwrap();
        std::fs::write(atomic.join("inner.rs"), PLANTED).unwrap();
        assert!(
            check(&[dir.path().to_path_buf()], &[]).is_ok(),
            "on_rewind inside crates/repo/src/atomic/ is allowed"
        );
    }

    #[test]
    fn flags_cfg_not_test_on_rewind() {
        // `#[cfg(not(test))]` is PRODUCTION code — it mentions `test` but is live
        // whenever `test` is unset, so the old substring match wrongly exempted
        // it (cid 3330966933). The predicate-aware walk must flag it.
        let hits = scan_source(
            "#[cfg(not(test))] \
             fn prod(tx: &mut Tx) -> Result<()> { tx.on_rewind(move || Ok(())); Ok(()) }",
        );
        assert_eq!(
            hits.len(),
            1,
            "on_rewind under #[cfg(not(test))] is production and must be flagged"
        );
    }

    #[test]
    fn still_skips_genuinely_test_only_cfg() {
        // The legitimate exemptions stay exempt: `cfg(test)` and any predicate
        // that requires `test` (e.g. `all(unix, test)`).
        assert!(
            scan_source("#[cfg(test)] fn t(tx: &mut Tx) { tx.on_rewind(|| Ok(())); }").is_empty(),
            "#[cfg(test)] item is test-only and must be skipped"
        );
        assert!(
            scan_source("#[cfg(all(unix, test))] fn t(tx: &mut Tx) { tx.on_rewind(|| Ok(())); }")
                .is_empty(),
            "#[cfg(all(unix, test))] requires test → test-only → skipped"
        );
    }

    #[test]
    fn scans_any_test_or_feature_cfg() {
        // `any(test, feature = "x")` is live in a non-test build (feature x), so
        // it must NOT be exempted just because it mentions `test`.
        let hits = scan_source(
            "#[cfg(any(test, feature = \"x\"))] \
             fn prod(tx: &mut Tx) { tx.on_rewind(|| Ok(())); }",
        );
        assert_eq!(hits.len(), 1, "any(test, feature) is prod-reachable and must be flagged");
    }

    #[test]
    fn flags_ufcs_on_rewind() {
        // The UFCS / path-call form is invisible to method-call scanning but is a
        // real bypass (cid 3330966935): `on_rewind` is `pub(crate)`.
        let hits = scan_source(
            "fn stage(tx: &mut Tx) -> Result<()> { Tx::on_rewind(tx, move || Ok(())); Ok(()) }",
        );
        assert_eq!(hits.len(), 1, "a UFCS Tx::on_rewind call must be flagged");
    }

    #[test]
    fn check_bails_on_cfg_not_test_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let crate_src = dir.path().join("cli/src");
        std::fs::create_dir_all(&crate_src).unwrap();
        std::fs::write(
            crate_src.join("bypass.rs"),
            "#[cfg(not(test))]\n\
             fn prod(tx: &mut Tx) -> Result<()> { tx.on_rewind(move || Ok(())); Ok(()) }",
        )
        .unwrap();
        assert!(
            check(&[dir.path().to_path_buf()], &[]).is_err(),
            "a #[cfg(not(test))] on_rewind bypass must fail the check"
        );
    }

    #[test]
    fn check_bails_on_ufcs_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let crate_src = dir.path().join("cli/src");
        std::fs::create_dir_all(&crate_src).unwrap();
        std::fs::write(
            crate_src.join("bypass.rs"),
            "fn stage(tx: &mut Tx) -> Result<()> { Tx::on_rewind(tx, move || Ok(())); Ok(()) }",
        )
        .unwrap();
        assert!(
            check(&[dir.path().to_path_buf()], &[]).is_err(),
            "a UFCS Tx::on_rewind bypass must fail the check"
        );
    }

    /// Enforcement test: scan the REAL workspace tree with the built-in (empty)
    /// allowlist and assert clean. This is what makes the gate fail CI under
    /// `cargo test --workspace` if an out-of-module `on_rewind` is introduced.
    #[test]
    fn production_tree_has_no_external_ledger_use() {
        let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("devtools crate dir has a parent (the crates/ dir)")
            .to_path_buf();
        let mut hits = Vec::new();
        let mut scanned = 0usize;
        scan_dir(&crates_dir, &mut hits, &mut scanned).expect("scan crates/");
        assert!(scanned > 0, "expected to scan some files under {crates_dir:?}");
        assert!(
            hits.is_empty(),
            "out-of-module on_rewind call site(s) found: {:?}",
            hits.iter()
                .map(|h| format!("{}:{}", h.path.display(), h.line))
                .collect::<Vec<_>>()
        );
    }
}
