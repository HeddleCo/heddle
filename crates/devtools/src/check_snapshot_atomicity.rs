// SPDX-License-Identifier: Apache-2.0
//! Workspace-wide AST asserter for the **cross-crate publish-first snapshot**
//! bug class (heddle#354 r8).
//!
//! ## The bug class
//!
//! A snapshot must commit its `OpRecord::Snapshot` **before** it publishes the
//! paired ref (record-first), so the reconciler's authoritative `Snapshot` fold
//! (newest committed record wins) can never resurrect a stale snapshot over a
//! newer concurrent write. The pre-r8 shape was the opposite — publish first,
//! record second:
//!
//! ```ignore
//! self.refs.set_thread(&thread, &state.change_id)?;   // PUBLISH (phase 5)
//! self.oplog.record_snapshot(...)?;                    // RECORD  (phase 4)
//! ```
//!
//! ## Why this asserter exists (the coverage hole r7 left)
//!
//! r7 added a conformance check INSIDE the `refs` crate
//! (`write_read_conformance`). It guards the refs-internal raw writers, but it
//! is blind to **cross-crate** callers: `crates/repo/src/repository_snapshot.rs`
//! and `crates/mount/src/core.rs` both called `refs.set_thread(...)` /
//! `refs.write_head(...)` directly, ahead of the snapshot record, and the
//! refs-only walk never saw them. This asserter walks the ENTIRE workspace and
//! fails CI if any production function co-locates a raw refs-publish with a
//! snapshot-record append — they must go through the record-first chokepoint
//! (`Repository::commit_snapshot_atomic` → `commit_and_publish`) instead.
//!
//! A function that uses `commit_snapshot_atomic` has NEITHER a raw publish NOR a
//! raw `record_snapshot`, so it drops out cleanly; re-introducing the
//! publish-first shape brings it back and fails the gate. A planted cross-crate
//! bypass (see the tests) proves the analyzer is non-vacuous.
//!
//! ## Scope
//!
//! This closes the SNAPSHOT publish-first class across crates. The broader
//! AtomicMutation call-site migration (the other publish-first verbs — marker,
//! thread, goto, fast-forward — which the reconciler handles via canonical-wins
//! HEAD folds or which are tracked for the deferred migration) is intentionally
//! out of scope here; the record-class allowlist below is the extension point as
//! that migration lands. Detection is by snapshot record class precisely so this
//! gate does not flag the deferred verbs.
//!
//! The env-var contract mirrors the sibling tree-load asserter:
//! `HEDDLE_SNAPSHOT_ATOMICITY_SEARCH_DIRS` (colon-separated, default `crates`)
//! and `HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST` (semicolon-separated `path:line`;
//! empty disables, unset uses the built-in default — currently empty, because
//! the two known sites are fixed, not exempted).

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{Attribute, Expr, ImplItemFn, ItemFn, ItemMod, Meta, visit::Visit};
use walkdir::WalkDir;

const DEFAULT_SEARCH_DIRS: &[&str] = &["crates"];

/// Raw ref-publish methods reachable on a `refs` handle across crates. A call to
/// any of these on a `.refs` / `.refs()` receiver is a "publish". Deliberately
/// excludes `commit_and_publish` (the record-first chokepoint — the fix).
const PUBLISH_METHODS: &[&str] = &[
    "set_thread",
    "set_thread_cas",
    "write_head",
    "write_head_cas",
    "set_marker_cas",
    "create_marker",
    "delete_thread",
    "delete_thread_cas",
    "delete_marker",
    "delete_marker_cas",
    "set_remote_thread",
    "delete_remote_thread",
    "set_undo_recovery",
    "update_refs",
];

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!(
            "check-snapshot-atomicity: unexpected argument '{arg}' (configured via env vars: \
HEDDLE_SNAPSHOT_ATOMICITY_SEARCH_DIRS, HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST)"
        );
    }

    check(&read_search_dirs(), &read_allowlist())
}

/// The testable core: scan `search_dirs`, filter hits by `allowlist`, print
/// findings, and `bail!` if any non-allowlisted publish-first snapshot remains.
/// `run()` is the thin env-reading wrapper around this.
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
            "::error::publish-first snapshot at {key}: `{}` is published in the same function \
that appends an `OpRecord::Snapshot` — {}",
            hit.method,
            hit.snippet.trim()
        );
        failed += 1;
    }

    if failed > 0 {
        eprintln!(
            "\n::error::Found {failed} cross-crate publish-first snapshot site(s). A snapshot must \
commit its `OpRecord::Snapshot` BEFORE publishing the paired ref (record-first), so the \
reconciler's authoritative `Snapshot` fold cannot resurrect a stale snapshot over a newer \
concurrent write. Route the ref write + record through `Repository::commit_snapshot_atomic` \
(which uses the record-first `commit_and_publish` chokepoint) instead of calling \
`refs.set_thread`/`refs.write_head` next to `oplog.record_snapshot`.\n\
\n\
If a site is a legitimate exception, add a `path:line` entry (of the publish call) to \
HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST with a one-line justification."
        );
        bail!("asserter failed");
    }

    println!(
        "asserter clean: no cross-crate publish-first snapshot sites in production code \
({files_scanned} file(s) scanned)"
    );
    Ok(())
}

fn read_search_dirs() -> Vec<PathBuf> {
    match env::var("HEDDLE_SNAPSHOT_ATOMICITY_SEARCH_DIRS") {
        Ok(value) if !value.is_empty() => value.split(':').map(PathBuf::from).collect(),
        _ => DEFAULT_SEARCH_DIRS.iter().map(PathBuf::from).collect(),
    }
}

fn read_allowlist() -> Vec<String> {
    match env::var("HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST") {
        Ok(value) if value.is_empty() => Vec::new(),
        Ok(value) => value.split(';').map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    method: String,
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

/// Tests legitimately exercise the bug class (they drive `set_thread` +
/// `record_snapshot` directly to simulate crashes/races) — skip them. Three
/// shapes: integration tests under a `tests/` segment, unit-test files named
/// `*_tests.rs`, and submodule test files named exactly `tests.rs` (e.g.
/// `crates/repo/src/atomic/tests.rs`). Inline `#[cfg(test)] mod tests { ... }`
/// blocks inside a production file are skipped separately, at the AST level
/// (see `is_cfg_test`), because their `#[cfg(test)]` gate lives in the same file.
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
/// an `all(...)` / `any(...)` predicate) — a test-only module that must not be
/// held to the production no-publish-first invariant.
fn is_cfg_test(attrs: &[Attribute]) -> bool {
    fn meta_mentions_test(meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => path.is_ident("test"),
            Meta::List(list) if list.path.is_ident("cfg") => list.tokens.to_string().contains("test"),
            Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
                list.tokens.to_string().contains("test")
            }
            _ => false,
        }
    }
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && meta_mentions_test(&attr.meta)
    })
}

struct Finder<'a> {
    path: PathBuf,
    lines: &'a [&'a str],
    hits: &'a mut Vec<Hit>,
}

impl Finder<'_> {
    /// Evaluate one function body: if it both publishes a ref AND appends a
    /// snapshot record, every publish call in it is a hit. The `body` is the
    /// whole fn block, visited recursively (closures included — the snapshot
    /// path historically wrapped its work in an IIFE), so a publish nested in a
    /// closure is still attributed to the enclosing function.
    fn evaluate_fn(&mut self, body: &syn::Block) {
        let mut collector = CallCollector::default();
        collector.visit_block(body);
        if collector.publishes.is_empty() || !collector.has_snapshot_record {
            return;
        }
        for (method, line) in collector.publishes {
            let snippet = self
                .lines
                .get(line.saturating_sub(1))
                .copied()
                .unwrap_or("")
                .to_string();
            self.hits.push(Hit {
                path: self.path.clone(),
                line,
                method,
                snippet,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        // Skip inline `#[cfg(test)] mod tests { ... }` blocks: test code drives
        // the publish-first shape on purpose to exercise reconcile/recovery.
        if is_cfg_test(&node.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        // Evaluate this fn; nested fns are captured by the recursive collector,
        // so we do NOT descend the Finder again (avoids double-attribution).
        self.evaluate_fn(&node.block);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        self.evaluate_fn(&node.block);
    }
}

/// Collects, within one function body, the raw ref-publish calls and whether a
/// snapshot-record append is present. Recurses through closures and blocks (the
/// syn default), so it sees calls wherever they sit in the body.
#[derive(Default)]
struct CallCollector {
    /// `(method_name, line)` for each raw `refs`-publish call.
    publishes: Vec<(String, usize)>,
    /// A snapshot record append (`oplog.record_snapshot(...)` or the free-fn
    /// `record_snapshot_in_oplog(...)` wrapper) is present.
    has_snapshot_record: bool,
    /// Local bindings that alias a refs handle (`let r = &self.refs;`,
    /// `let refs = self.inner.repo.refs();`). A publish method invoked on one of
    /// these names is a publish too — closes the aliased-handle blind spot
    /// where a bypass routed through a local escaped the analyzer (heddle#354
    /// r9, cid 3330304661). Over-approximating across sibling scopes is the safe
    /// direction for an atomicity gate (better to flag than to miss).
    refs_aliases: std::collections::HashSet<String>,
}

impl CallCollector {
    /// True iff `expr` resolves to a refs handle: a `.refs` field, a `.refs()`
    /// accessor, or a local bound to one of those (tracked in `refs_aliases`).
    /// Peels `&`/`?`/paren/group/await wrappers so an aliased or borrowed handle
    /// is seen the same as a direct one.
    fn expr_is_refs_handle(&self, expr: &Expr) -> bool {
        let mut cur = expr;
        loop {
            match cur {
                Expr::Field(field) => {
                    return matches!(&field.member, syn::Member::Named(ident) if ident == "refs");
                }
                Expr::MethodCall(mc) => return mc.method == "refs",
                Expr::Path(p) => {
                    return p
                        .path
                        .get_ident()
                        .is_some_and(|id| self.refs_aliases.contains(&id.to_string()));
                }
                Expr::Reference(r) => cur = &r.expr,
                Expr::Try(t) => cur = &t.expr,
                Expr::Paren(p) => cur = &p.expr,
                Expr::Group(g) => cur = &g.expr,
                Expr::Await(a) => cur = &a.base,
                _ => return false,
            }
        }
    }
}

impl<'ast> Visit<'ast> for CallCollector {
    fn visit_local(&mut self, node: &'ast syn::Local) {
        // `let <ident> = <refs handle>;` registers `<ident>` as an alias, so a
        // later `<ident>.set_thread(...)` is recognized as a publish. Handles
        // alias-of-alias chains (`let b = a;`) because `expr_is_refs_handle`
        // also accepts a path that is already a known alias.
        if let Some(init) = &node.init
            && self.expr_is_refs_handle(&init.expr)
            && let syn::Pat::Ident(pat_ident) = &node.pat
            && pat_ident.subpat.is_none()
        {
            self.refs_aliases.insert(pat_ident.ident.to_string());
        }
        syn::visit::visit_local(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if method == "record_snapshot" {
            self.has_snapshot_record = true;
        } else if PUBLISH_METHODS.contains(&method.as_str())
            && self.expr_is_refs_handle(&node.receiver)
        {
            self.publishes
                .push((method, node.method.span().start().line));
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let Expr::Path(p) = node.func.as_ref()
            && p.path
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "record_snapshot_in_oplog")
        {
            self.has_snapshot_record = true;
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
    fn flags_publish_then_snapshot_record_same_fn() {
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                self.refs.set_thread(&t, &id)?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "set_thread");
    }

    /// The cross-crate planted bypass the task calls for: a repo/mount-style
    /// caller writing a ref before recording a snapshot. Uses the `.refs()`
    /// accessor and the `record_snapshot_in_oplog` free-fn wrapper — both must be
    /// caught, proving the analyzer is non-vacuous beyond the refs crate.
    #[test]
    fn flags_planted_cross_crate_bypass_accessor_and_free_fn() {
        let hits = scan_source(
            "fn capture(&self) -> Result<()> { \
                self.inner.repo.refs().set_thread(&served, &change_id)?; \
                repo::snapshot_metadata::record_snapshot_in_oplog(&self.inner.repo, &change_id, p, th)?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1, "planted cross-crate bypass must be flagged");
        assert_eq!(hits[0].method, "set_thread");
    }

    #[test]
    fn flags_publish_via_aliased_refs_handle() {
        // A bypass routed through a local alias of the refs handle must still
        // be flagged (heddle#354 r9, cid 3330304661) — an aliased handle was a
        // blind spot the gate missed.
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                let r = &self.refs; \
                r.set_thread(&t, &id)?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1, "publish via aliased refs handle must be flagged");
        assert_eq!(hits[0].method, "set_thread");
    }

    #[test]
    fn flags_publish_via_accessor_alias_chain() {
        // Alias of a `.refs()` accessor, then an alias-of-alias chain.
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                let a = self.inner.repo.refs(); \
                let b = a; \
                b.write_head(&Head::Detached { state })?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1, "publish via aliased accessor handle must be flagged");
        assert_eq!(hits[0].method, "write_head");
    }

    #[test]
    fn ignores_non_refs_alias() {
        // A local bound to a non-refs value must NOT be treated as a refs alias.
        let hits = scan_source(
            "fn f(&self) -> Result<()> { \
                let r = &self.cache; \
                r.set_thread(&t, &id)?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert!(hits.is_empty(), "a non-refs local must not be a refs alias");
    }

    #[test]
    fn flags_detached_write_head_with_snapshot_record() {
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                self.refs.write_head(&Head::Detached { state })?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "write_head");
    }

    #[test]
    fn flags_publish_inside_closure() {
        // The snapshot path historically wrapped its body in an IIFE; a publish
        // nested in a closure must still be attributed to the enclosing fn.
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                let r = (|| -> Result<()> { \
                    self.refs.set_thread(&t, &id)?; \
                    self.oplog.record_snapshot(&id, p, th, s)?; \
                    Ok(()) })(); \
                r }",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn ignores_record_first_via_commit_and_publish() {
        // The fix shape: a single chokepoint call, no raw publish, no raw
        // record_snapshot — must NOT be flagged.
        let hits = scan_source(
            "fn cap(&self) -> Result<()> { \
                self.commit_snapshot_atomic(&id, prev, thread)?; \
                Ok(()) }",
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_publish_without_snapshot_record() {
        // A pure publish (e.g. `seed_default_thread`, git-overlay HEAD sync) has
        // no paired snapshot record — not a snapshot-atomicity hazard.
        let hits = scan_source(
            "fn seed(&self) -> Result<()> { self.refs.set_thread(&main, &id)?; Ok(()) }",
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_snapshot_record_without_publish() {
        // The committer/oplog side records without a co-located raw publish.
        let hits = scan_source(
            "fn rec(&self) -> Result<()> { self.oplog.record_snapshot(&id, p, th, s)?; Ok(()) }",
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_non_refs_receiver() {
        // `set_thread`-named method on a non-refs receiver is not a ref publish.
        let hits = scan_source(
            "fn f(&self) -> Result<()> { \
                self.cache.set_thread(&t, &id)?; \
                self.oplog.record_snapshot(&id, p, th, s)?; \
                Ok(()) }",
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_inline_cfg_test_module() {
        // Test code drives the publish-first shape on purpose (to exercise
        // reconcile/recovery), so an inline `#[cfg(test)] mod tests` must be
        // skipped even when it lives in a production-named file.
        let hits = scan_source(
            "fn prod() {} \
             #[cfg(test)] \
             mod tests { \
                fn race() { \
                    self.refs.set_thread(&t, &id).unwrap(); \
                    self.oplog.record_snapshot(&id, p, th, s).unwrap(); \
                } \
             }",
        );
        assert!(hits.is_empty(), "inline #[cfg(test)] module must be skipped");
    }

    #[test]
    fn ignores_string_literal() {
        let hits = scan_source(
            "fn f() { let s = \"self.refs.set_thread(x); record_snapshot(y)\"; let _ = s; }",
        );
        assert!(hits.is_empty());
    }

    const PLANTED: &str = "fn cap(&self) -> Result<()> { \
            self.refs.set_thread(&t, &id)?; \
            self.oplog.record_snapshot(&id, p, th, s)?; \
            Ok(()) }";

    #[test]
    fn check_bails_on_planted_site_and_exempts_via_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bypass.rs"), PLANTED).unwrap();
        let dirs = vec![dir.path().to_path_buf()];

        // No allowlist → the planted publish-first site fails the gate.
        assert!(
            check(&dirs, &[]).is_err(),
            "a planted publish-first site must fail the check"
        );

        // Allowlisting the publish line exempts it (the publish is on line 1).
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
            "fn cap(&self) -> Result<()> { self.commit_snapshot_atomic(&id, p, t)?; Ok(()) }",
        )
        .unwrap();
        assert!(check(&[dir.path().to_path_buf()], &[]).is_ok());
    }

    /// Enforcement test: scan the REAL workspace tree with the built-in (empty)
    /// allowlist and assert clean. This is what makes the gate fail CI under
    /// `cargo test --workspace` if a publish-first snapshot site is introduced.
    #[test]
    fn production_tree_has_no_publish_first_snapshot() {
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
            "cross-crate publish-first snapshot site(s) found: {:?}",
            hits.iter()
                .map(|h| format!("{}:{} ({})", h.path.display(), h.line, h.method))
                .collect::<Vec<_>>()
        );
    }
}
