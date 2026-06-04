// SPDX-License-Identifier: Apache-2.0
//! Shared plumbing for the AST-based `check_*` asserters.
//!
//! Every asserter reads the same two env vars (a colon-separated search
//! path and a semicolon-separated `path:line` allowlist), differing only
//! in the var *names*, and walks the same `.rs`-file tree under each
//! search dir. This module owns that common shape; each `check_*` binary
//! supplies only its env-key constants, its per-path skip predicate, and
//! its `(path, source)` visitor.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use walkdir::WalkDir;

/// Default search root when the env var is unset or empty: the
/// workspace `crates/` tree.
const DEFAULT_SEARCH_DIRS: &[&str] = &["crates"];

/// Read the colon-separated search-dir list from `env_key`. An unset or
/// empty value falls back to [`DEFAULT_SEARCH_DIRS`] (`crates`).
pub fn read_search_dirs(env_key: &str) -> Vec<PathBuf> {
    match env::var(env_key) {
        Ok(value) if !value.is_empty() => value.split(':').map(PathBuf::from).collect(),
        _ => DEFAULT_SEARCH_DIRS.iter().map(PathBuf::from).collect(),
    }
}

/// Read the semicolon-separated `path:line` allowlist from `env_key`.
/// An explicitly-empty value disables the list; an unset var uses the
/// built-in default (also empty).
pub fn read_allowlist(env_key: &str) -> Vec<String> {
    match env::var(env_key) {
        Ok(value) if value.is_empty() => Vec::new(),
        Ok(value) => value.split(';').map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Walk every `.rs` file under `dir`, invoking `visit(path, source)` on
/// each file that `skip` does not reject. A non-existent `dir` is a
/// no-op (mirrors the legacy shell asserters, whose mutation harness
/// sometimes points at empty trees). `files_scanned` is bumped once per
/// file actually read — `skip`ped files are filtered *before* the read,
/// so they don't count.
pub fn for_each_rs_file(
    dir: &Path,
    files_scanned: &mut usize,
    skip: impl Fn(&Path) -> bool,
    mut visit: impl FnMut(&Path, &str) -> Result<()>,
) -> Result<()> {
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
        if skip(path) {
            continue;
        }
        let source = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        *files_scanned += 1;
        visit(path, &source)?;
    }
    Ok(())
}
