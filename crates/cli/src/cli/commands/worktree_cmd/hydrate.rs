// SPDX-License-Identifier: Apache-2.0
//! Implementation of `heddle start --hydrate`.
//!
//! An isolated `heddle start --path` checkout is a faithful *source*
//! tree: ignored dependency directories (`node_modules`, `.venv`,
//! `target/`, …) are correctly left out, because heddle never captures
//! ignored paths. The cost is that the checkout isn't immediately
//! buildable — you can't run `tsc`/`eslint`/tests to validate the change
//! you isolated without first re-installing dependencies from scratch.
//!
//! `--hydrate` closes that gap: after the checkout is materialized, it
//! **symlinks** the origin checkout's top-level ignored directories into
//! the new checkout, so the isolated tree is buildable with the deps
//! already present.
//!
//! ## Mechanism: symlink (not copy, not a hook)
//!
//! - **Symlink** keeps the "cheap isolated threads" property intact — a
//!   `node_modules` can be many gigabytes; copying it per thread defeats
//!   the point of a lightweight checkout. A symlink is O(1).
//! - The links stay **ignored**, so the deps are never captured into
//!   heddle (satisfying the issue's correctness AC). heddle's ignore
//!   matcher probes directory entries with `is_dir = true`, so a
//!   trailing-slash rule like `node_modules/` fires on the bare symlink
//!   entry just as it does on a real directory (locked in by
//!   heddle#303's regression tests).
//! - A **copy** would be correct but expensive; a **post-checkout hook**
//!   pushes the work back onto the user and isn't a first-class
//!   affordance. Symlink is the ergonomic + correct middle ground.
//!
//! ## Scope
//!
//! - Top-level ignored directories only. The dogfood case
//!   (`node_modules` at the repo root) is covered; per-package
//!   `node_modules` in a monorepo is not auto-discovered.
//! - Admin directories (`.git`, `.heddle`) are never hydrated even
//!   though they're ignored.
//! - Bytes-on-disk thread modes only (solid / materialized). Virtualized
//!   mounts project the captured tree and aren't hydrated.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use repo::Repository;

/// Directories that are ignored but must never be hydrated: linking
/// `.git` or `.heddle` into a checkout would cross-wire two repos'
/// metadata.
const ADMIN_DIRS: &[&str] = &[".git", ".heddle"];

/// Enumerate the absolute paths of top-level directories in the origin
/// checkout that are ignored (by `.gitignore` in git-overlay mode and/or
/// `.heddleignore`) — the dependency/build dirs an isolated checkout
/// omits. Admin dirs (`.git`, `.heddle`) are excluded. Results are
/// sorted for deterministic output.
pub(crate) fn hydratable_ignored_dirs(repo: &Repository) -> Result<Vec<PathBuf>> {
    let patterns = repo.ignore_patterns()?;
    let root = repo.root();

    let read = std::fs::read_dir(root)
        .with_context(|| format!("read origin checkout root '{}'", root.display()))?;

    let mut dirs = Vec::new();
    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if ADMIN_DIRS.contains(&name_str.as_ref()) {
            continue;
        }

        // A dependency dir is either a real directory or a symlink that
        // resolves to one (the origin itself may already be hydrated via
        // a link, e.g. a pnpm store). Plain files (`.env`, `*.log`) are
        // out of scope — the issue is specifically about deps *dirs*.
        let file_type = entry.file_type()?;
        let is_dir_like = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
        if !is_dir_like {
            continue;
        }

        // `should_ignore` probes with `is_dir = true`, so a
        // trailing-slash rule (`node_modules/`) fires on the bare
        // directory entry — matching how heddle prunes ignored trees
        // during capture (heddle#303).
        if objects::worktree::should_ignore(Path::new(name_str.as_ref()), &patterns) {
            dirs.push(entry.path());
        }
    }

    dirs.sort();
    Ok(dirs)
}

/// Symlink each directory in `sources` into `checkout`, using the
/// source's final path component as the link name. Skips any entry whose
/// destination already exists (captured or pre-staged) so we never
/// clobber. Returns the names actually linked, in input order.
pub(crate) fn hydrate_checkout(checkout: &Path, sources: &[PathBuf]) -> Result<Vec<String>> {
    let mut linked = Vec::new();
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = checkout.join(name);

        // `symlink_metadata` (not `exists`) so a broken symlink or any
        // already-present entry counts as a collision — we never clobber
        // captured content or a user's pre-staged link.
        if dest.symlink_metadata().is_ok() {
            continue;
        }

        symlink_dir(source, &dest).with_context(|| {
            format!("hydrate '{}' -> '{}'", dest.display(), source.display())
        })?;
        linked.push(name.to_string_lossy().into_owned());
    }
    Ok(linked)
}

/// Make the hydrated dep symlinks stay ignored in the isolated checkout.
///
/// The origin may ignore deps only via `.gitignore` (the common
/// git-overlay setup). The isolated checkout has no `.git` and is
/// reopened as a *native* heddle repo, whose ignore resolution reads
/// `.heddleignore` — never `.gitignore`. Without preserving the rule the
/// symlinked dep dirs surface as uncaptured *added* paths, and capture
/// fails trying to follow the absolute link target out of the checkout.
///
/// We materialize the effective ignore rule for each linked name into
/// the checkout's own `.heddleignore`, so the checkout is self-consistent
/// regardless of which ignore source the origin used. Names already
/// covered by an existing `.heddleignore` are left untouched. When we
/// create the file from scratch it self-ignores, so the generated
/// artifact does not itself surface as uncaptured content.
pub(crate) fn preserve_hydrated_ignores(checkout: &Path, linked: &[String]) -> Result<()> {
    if linked.is_empty() {
        return Ok(());
    }

    let ignore_path = checkout.join(".heddleignore");
    let existing = match std::fs::read_to_string(&ignore_path) {
        Ok(contents) => Some(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("read '{}'", ignore_path.display()));
        }
    };

    let patterns: Vec<String> = existing
        .iter()
        .flat_map(|c| c.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();

    let mut to_add: Vec<String> = Vec::new();

    // When creating the file fresh, ignore it too — it's a hydration
    // artifact local to this checkout, not source the user authored.
    if existing.is_none() {
        to_add.push(".heddleignore".to_string());
    }

    for name in linked {
        // Already covered by the checkout's native ignore source? Leave
        // the existing rule in place rather than appending a duplicate.
        let mut probe = patterns.clone();
        probe.extend(to_add.iter().cloned());
        if objects::worktree::should_ignore(Path::new(name), &probe) {
            continue;
        }
        // Trailing-slash (dir-only) rule mirrors how deps are ignored —
        // it fires on the bare symlink entry (heddle#303).
        to_add.push(format!("{name}/"));
    }

    if to_add.is_empty() {
        return Ok(());
    }

    let mut out = existing.unwrap_or_default();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in &to_add {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(&ignore_path, out)
        .with_context(|| format!("write hydrate ignore rules to '{}'", ignore_path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    // Test seam: lets integration tests simulate a host/FS that rejects
    // directory symlinks (Windows without the privilege, exotic FS) so
    // the hydrate rollback contract is exercised on a platform that
    // *does* support them. No-op in production (env var unset).
    objects::fault_inject::maybe_fail_at("hydrate_symlink_dir")?;
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    objects::fault_inject::maybe_fail_at("hydrate_symlink_dir")?;
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn hydrate_checkout_symlinks_each_source_and_skips_existing() {
        let temp = TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(origin.join("node_modules")).unwrap();
        std::fs::create_dir_all(origin.join(".venv")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        // Pre-existing entry in the checkout must not be clobbered.
        std::fs::create_dir_all(checkout.join(".venv")).unwrap();

        let sources = vec![origin.join("node_modules"), origin.join(".venv")];
        let linked = hydrate_checkout(&checkout, &sources).unwrap();

        assert_eq!(
            linked,
            vec!["node_modules".to_string()],
            "only the non-colliding source should be linked"
        );
        let nm = checkout.join("node_modules");
        assert!(
            std::fs::symlink_metadata(&nm)
                .unwrap()
                .file_type()
                .is_symlink(),
            "node_modules should be a symlink"
        );
        assert_eq!(std::fs::read_link(&nm).unwrap(), origin.join("node_modules"));
        // The pre-existing .venv stayed a real directory (not clobbered).
        assert!(
            !std::fs::symlink_metadata(checkout.join(".venv"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "pre-existing .venv must be preserved, not replaced by a link"
        );
    }
}
