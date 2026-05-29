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

use anyhow::Result;
use repo::Repository;

/// Directories that are ignored but must never be hydrated: linking
/// `.git` or `.heddle` into a checkout would corrupt it.
const ADMIN_DIRS: &[&str] = &[".git", ".heddle"];

/// Enumerate the absolute paths of top-level directories in the origin
/// checkout that are ignored (by `.gitignore` in git-overlay mode and/or
/// `.heddleignore`) — the dependency/build dirs an isolated checkout
/// omits. Admin dirs (`.git`, `.heddle`) are excluded.
pub(crate) fn hydratable_ignored_dirs(_repo: &Repository) -> Result<Vec<PathBuf>> {
    // Stub: real implementation lands in the green commit.
    Ok(Vec::new())
}

/// Symlink each directory in `sources` into `checkout`, using the
/// source's final path component as the link name. Skips any entry whose
/// destination already exists (captured or pre-staged) so we never
/// clobber. Returns the names actually linked, in input order.
pub(crate) fn hydrate_checkout(_checkout: &Path, _sources: &[PathBuf]) -> Result<Vec<String>> {
    // Stub: real implementation lands in the green commit.
    Ok(Vec::new())
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
