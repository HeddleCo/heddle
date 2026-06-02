// SPDX-License-Identifier: Apache-2.0
//! Implementation of `heddle start --hydrate` (heddle#302).
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
//! ## Atomicity (heddle#356)
//!
//! `--hydrate` is the last fallible leg of `thread start` and can fail on
//! a host/filesystem that rejects directory symlinks. The whole start runs
//! under the [`AtomicMutation`](repo::atomic) primitive, so the rollback is
//! not hand-rolled here: the start mutation calls [`symlink_one`] inside a
//! `Tx::step`, registering a precise unlink inverse per created link, and
//! the checkout materialization registers its own dir-rewind inverse. A
//! partial hydrate (k of N links made, the (k+1)-th fails) unwinds all k
//! links AND the checkout, back to the exact pre-start state.
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

/// Decide whether `source` should be hydrated into `checkout`, returning
/// the `(dest, link_name)` to create or `None` to skip. Skips a `source`
/// with no final component, or a destination that already exists (captured
/// or pre-staged) so we never clobber.
///
/// Separated from [`create_symlink`] so the start mutation can wrap ONLY
/// the actual link creation in a forward-first `Tx::step`: the per-link
/// unlink inverse is registered after the create succeeds, and a skipped
/// `source` (`None`) never touches the rewind ledger.
pub(crate) fn plan_link(checkout: &Path, source: &Path) -> Option<(PathBuf, String)> {
    let name = source.file_name()?;
    let dest = checkout.join(name);

    // `symlink_metadata` (not `exists`) so a broken symlink or any
    // already-present entry counts as a collision — we never clobber
    // captured content or a user's pre-staged link.
    if dest.symlink_metadata().is_ok() {
        return None;
    }

    Some((dest, name.to_string_lossy().into_owned()))
}

/// Create one hydrate symlink: `dest` -> `source`. A single all-or-nothing
/// filesystem op (the forward of a `Tx::step`); the caller registers
/// [`unlink_hydrated`] as the inverse only after this returns `Ok`.
pub(crate) fn create_symlink(source: &Path, dest: &Path) -> Result<()> {
    symlink_dir(source, dest)
        .with_context(|| format!("hydrate '{}' -> '{}'", dest.display(), source.display()))
}

/// Best-effort inverse of [`create_symlink`]: unlink a hydrate symlink this
/// invocation created. Tolerant of a missing link — the checkout-rewind
/// inverse (LIFO, after this) may already have removed the whole tree.
pub(crate) fn unlink_hydrated(checkout: &Path, name: &str) -> std::io::Result<()> {
    match remove_symlink_dir(&checkout.join(name)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Remove a directory symlink created by [`create_symlink`]/[`symlink_dir`]. The
/// forward always makes a *directory* symlink, and on Windows a directory
/// symlink must be removed with `RemoveDirectory` (`remove_dir`) — `DeleteFile`
/// (`remove_file`) errors on it, so the rollback would fail (heddle#356 cid
/// 3333881572). On Unix a symlink (file or dir) is removed by `unlink`
/// (`remove_file`).
#[cfg(not(windows))]
fn remove_symlink_dir(link: &Path) -> std::io::Result<()> {
    std::fs::remove_file(link)
}

#[cfg(windows)]
fn remove_symlink_dir(link: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(link)
}

/// The worktree-local, never-captured exclude file hydrate records its
/// dep-ignore rules in — heddle's analogue of `.git/info/exclude`. It lives
/// under the checkout's own `.heddle/` (which is always ignored), so writing to
/// it never surfaces as worktree content. [`Repository::ignore_patterns`]
/// reads it alongside `.heddleignore`.
pub(crate) fn hydrate_exclude_path(checkout: &Path) -> PathBuf {
    checkout.join(".heddle").join("info").join("exclude")
}

/// Make the hydrated dep symlinks stay ignored in the isolated checkout.
///
/// The origin may ignore deps only via `.gitignore` (the common
/// git-overlay setup). The isolated checkout has no `.git` and is
/// reopened as a *native* heddle repo, whose ignore resolution reads
/// `.heddleignore` + the worktree-local exclude — never `.gitignore`.
/// Without preserving the rule the symlinked dep dirs surface as uncaptured
/// *added* paths, and capture fails trying to follow the absolute link target
/// out of the checkout.
///
/// We materialize the rule for each linked name into the checkout's
/// worktree-local exclude file ([`hydrate_exclude_path`]) — NOT the
/// possibly-tracked `.heddleignore`. The exclude file is never captured, so a
/// successful `start --hydrate` leaves the tracked tree clean even when the
/// checkout already carries a tracked `.heddleignore` (heddle#356 cid
/// 3333881577). Names already covered by the checkout's `.heddleignore` or a
/// prior exclude entry are skipped rather than duplicated.
pub(crate) fn preserve_hydrated_ignores(checkout: &Path, linked: &[String]) -> Result<()> {
    if linked.is_empty() {
        return Ok(());
    }

    let read_opt = |path: &Path| -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read '{}'", path.display())),
        }
    };
    let active_patterns = |contents: &Option<String>| -> Vec<String> {
        contents
            .iter()
            .flat_map(|c| c.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    };

    // Rules already in force via the checkout's tracked `.heddleignore` plus any
    // prior exclude entries — a dep already covered needs no new rule.
    let tracked = read_opt(&checkout.join(".heddleignore"))?;
    let exclude_path = hydrate_exclude_path(checkout);
    let existing_exclude = read_opt(&exclude_path)?;

    let mut covered: Vec<String> = active_patterns(&tracked);
    covered.extend(active_patterns(&existing_exclude));

    let mut to_add: Vec<String> = Vec::new();
    for name in linked {
        let mut probe = covered.clone();
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

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create '{}'", parent.display()))?;
    }
    let mut out = existing_exclude.unwrap_or_default();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in &to_add {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(&exclude_path, out)
        .with_context(|| format!("write hydrate ignore rules to '{}'", exclude_path.display()))?;
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

    /// Link one source into the checkout via plan + create, mirroring the
    /// start mutation's per-link `Tx::step`. Returns the linked name (or
    /// `None` when the plan skipped a collision).
    fn link_one(checkout: &Path, source: &Path) -> Option<String> {
        let (dest, name) = plan_link(checkout, source)?;
        create_symlink(source, &dest).unwrap();
        Some(name)
    }

    #[test]
    fn plan_link_links_source_and_skips_existing() {
        let temp = TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(origin.join("node_modules")).unwrap();
        std::fs::create_dir_all(origin.join(".venv")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        // Pre-existing entry in the checkout must not be clobbered.
        std::fs::create_dir_all(checkout.join(".venv")).unwrap();

        let linked_nm = link_one(&checkout, &origin.join("node_modules"));
        let skipped_venv = plan_link(&checkout, &origin.join(".venv"));

        assert_eq!(linked_nm.as_deref(), Some("node_modules"));
        assert!(skipped_venv.is_none(), "a colliding source links nothing");
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

    #[test]
    fn unlink_hydrated_removes_link_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(origin.join("node_modules")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();

        link_one(&checkout, &origin.join("node_modules")).unwrap();
        assert!(std::fs::symlink_metadata(checkout.join("node_modules")).is_ok());

        unlink_hydrated(&checkout, "node_modules").unwrap();
        assert!(
            std::fs::symlink_metadata(checkout.join("node_modules")).is_err(),
            "the hydrate link must be unlinked"
        );
        // The origin dir behind the link must survive (we unlink, never follow).
        assert!(origin.join("node_modules").is_dir());
        // Idempotent: unlinking an already-gone link is a no-op.
        unlink_hydrated(&checkout, "node_modules").unwrap();
    }

    /// heddle#356 cid 3333881572: on Windows a *directory* symlink must be
    /// removed with `RemoveDirectory` (`remove_dir`) — `DeleteFile`
    /// (`remove_file`) errors on it, so the rollback would fail. The forward
    /// always creates a directory symlink, so the inverse must use the dir
    /// remover. (Gated to Windows; on Unix the same contract is covered by
    /// `unlink_hydrated_removes_link_and_is_idempotent`, which unlinks a dir
    /// symlink via `remove_file`.)
    #[cfg(windows)]
    #[test]
    fn unlink_hydrated_removes_directory_symlink_on_windows() {
        let temp = TempDir::new().unwrap();
        let origin = temp.path().join("origin");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(origin.join("node_modules")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();

        // Create a directory symlink exactly like the hydrate forward does.
        symlink_dir(&origin.join("node_modules"), &checkout.join("node_modules")).unwrap();
        assert!(std::fs::symlink_metadata(checkout.join("node_modules")).is_ok());

        unlink_hydrated(&checkout, "node_modules").unwrap();
        assert!(
            std::fs::symlink_metadata(checkout.join("node_modules")).is_err(),
            "a directory symlink must be removed on rollback (remove_dir, not remove_file)"
        );
        assert!(origin.join("node_modules").is_dir(), "the link target must survive");
    }
}
