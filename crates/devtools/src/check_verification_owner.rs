// SPDX-License-Identifier: Apache-2.0
//! Grep-gate: Repository Verification State / health proof construction is
//! owned by `heddle-core`. CLI and other crates may inject Machine-Contract
//! Proof and render advice, but must not re-implement health/state builders.
//!
//! Forbidden production definitions outside `crates/core/`:
//! - `build_repository_verification_health*`
//! - `build_verification_health_inner`
//! - `build_native_heddle_health`
//! - `repository_verification_state_from_health*`
//!
//! Thin CLI adapters that call into core (for catalog injection) may keep
//! names like `build_repository_verification_state` / `build_verification_health`.
//!
//! Env:
//! - `HEDDLE_ASSERTER_SEARCH_DIRS` — colon-separated dirs (default `crates`)
//! - `HEDDLE_VERIFICATION_OWNER_ALLOWLIST` — semicolon-separated `path:line`

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{Item, ItemFn, visit::Visit};

use crate::asserter::{for_each_rs_file, read_allowlist, read_search_dirs};

const FORBIDDEN_EXACT: &[&str] = &[
    "build_verification_health_inner",
    "build_native_heddle_health",
];

const FORBIDDEN_PREFIXES: &[&str] = &[
    "build_repository_verification_health",
    "repository_verification_state_from_health",
];

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!(
            "check-verification-owner: unexpected argument '{arg}' (configured via env vars: \
HEDDLE_ASSERTER_SEARCH_DIRS, HEDDLE_VERIFICATION_OWNER_ALLOWLIST)"
        );
    }

    let search_dirs = read_search_dirs("HEDDLE_ASSERTER_SEARCH_DIRS");
    let allowlist = read_allowlist("HEDDLE_VERIFICATION_OWNER_ALLOWLIST");

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
        eprintln!(
            "::error::{} at {}: {}",
            hit.label,
            key,
            hit.snippet.trim()
        );
        failed += 1;
    }

    if failed > 0 {
        eprintln!(
            "\n::error::Found {failed} CLI/out-of-core repository verification proof builder \
definition(s). Core owns Repository Verification State construction \
(`heddle_core::status::build_repository_verification_health_*` and \
`heddle_core::verify::build_repository_verification_state_*`). CLI modules may \
inject Machine-Contract Proof and render RecoveryAdvice, but must not rebuild \
health/state. See docs/VERIFICATION_CLEANUP_PLAN.md Track A."
        );
        bail!("asserter failed");
    }

    println!(
        "asserter clean: no out-of-core repository verification proof builders \
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
    for_each_rs_file(dir, files_scanned, is_skipped_path, |path, source| {
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

/// Allow core (the single owner) and tests.
fn is_skipped_path(path: &Path) -> bool {
    let mut components = path.components().map(|c| c.as_os_str());
    let comps: Vec<_> = components.by_ref().collect();
    // crates/core/**
    for window in comps.windows(2) {
        if window[0] == "crates" && window[1] == "core" {
            return true;
        }
    }
    for component in &comps {
        if *component == "tests" {
            return true;
        }
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.ends_with("_tests.rs") || name == "tests.rs")
        .unwrap_or(false)
}

struct Finder<'a> {
    path: PathBuf,
    lines: &'a [&'a str],
    hits: &'a mut Vec<Hit>,
}

impl<'a, 'ast> Visit<'ast> for Finder<'a> {
    fn visit_item(&mut self, item: &'ast Item) {
        if let Item::Fn(func) = item {
            self.visit_fn(func);
        }
        syn::visit::visit_item(self, item);
    }
}

impl Finder<'_> {
    fn visit_fn(&mut self, func: &ItemFn) {
        let name = func.sig.ident.to_string();
        if !is_forbidden_builder(&name) {
            return;
        }
        let line = func.sig.ident.span().start().line;
        let snippet = self
            .lines
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or("")
            .to_string();
        self.hits.push(Hit {
            path: self.path.clone(),
            line,
            label: "out-of-core repository verification proof builder",
            snippet,
        });
    }
}

fn is_forbidden_builder(name: &str) -> bool {
    if FORBIDDEN_EXACT.contains(&name) {
        return true;
    }
    FORBIDDEN_PREFIXES
        .iter()
        .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}_")))
}

#[cfg(test)]
mod tests {
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
    fn flags_health_inner_builder() {
        let hits = scan_source("fn build_verification_health_inner() {}");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn flags_repository_health_builder() {
        let hits = scan_source(
            "pub fn build_repository_verification_health_with_worktree_status() {}",
        );
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn allows_cli_state_adapter_name() {
        let hits = scan_source("pub(crate) fn build_repository_verification_state() {}");
        assert!(hits.is_empty());
    }

    #[test]
    fn allows_cli_health_adapter_name() {
        let hits = scan_source("pub(crate) fn build_verification_health() {}");
        assert!(hits.is_empty());
    }

    #[test]
    fn flags_from_health_builder() {
        let hits =
            scan_source("fn repository_verification_state_from_health_with_worktree_status() {}");
        assert_eq!(hits.len(), 1);
    }
}
