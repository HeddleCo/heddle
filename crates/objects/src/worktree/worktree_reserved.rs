// SPDX-License-Identifier: Apache-2.0
//! Reserved worktree paths that user ignore rules cannot un-ignore.
//!
//! Root `.heddle/` holds identity material (`identity.toml`), credentials,
//! and repository-engine state. Pointer checkout also writes cursor files
//! beside the `.heddle` file (`.heddle.identity`, `.heddle.last-turn`,
//! `.identity.lock`, `.identity.tmp.*`, `.last-turn.tmp.*`). Gitignore
//! last-match-wins would otherwise let `!.heddle/` pull that tree into
//! capture. Nested `.heddle/` directories (fixtures) stay ordinary content.
//!
//! Metadata-directory aliases (heddle#2028) are reserved the same way:
//! `.git` and its case, NTFS and HFS+ spellings at every depth, and those of
//! `.heddle` at the root. Checkout never writes them, so capture must
//! never record them; a nested `.git` is another repository, as in Git.

use std::path::{Component, Path};

use crate::{
    error::HeddleError,
    object::{ReservedPathComponent, reserved_path_component, reserved_tree_entry_name},
};

/// Whether `path` is a reserved worktree-root Heddle artifact, or passes
/// through a metadata-directory alias (`.git` at any depth, `.heddle` at the
/// root; see [`crate::object::reserved_tree_entry_name`]).
///
/// The Heddle artifacts are root-anchored only: `examples/calculator/.heddle/`
/// is not reserved. Leading `./` is skipped so `./.heddle/identity.toml`
/// matches.
#[must_use]
pub fn is_reserved_worktree_path(path: &Path) -> bool {
    first_normal_component(path).is_some_and(is_reserved_root_name)
        || reserved_path_component(path.as_os_str().as_encoded_bytes(), false).is_some()
}

/// The reserved component of a worktree write to `rel_path`, if any: a
/// `.git` alias at any depth, a `.heddle` alias at the root, or a
/// `.gitmodules` alias when `symlink` (heddle#2028).
pub fn reserved_worktree_write(rel_path: &Path, symlink: bool) -> Option<ReservedPathComponent> {
    reserved_path_component(rel_path.as_os_str().as_encoded_bytes(), symlink)
}

/// Refuse a worktree write whose worktree-relative path is reserved (see
/// [`reserved_worktree_write`]). For commands that write one path the user
/// named; whole-tree writers skip such paths with a warning instead.
pub fn check_worktree_write_path(rel_path: &Path, symlink: bool) -> Result<(), HeddleError> {
    match reserved_worktree_write(rel_path, symlink) {
        Some(reason) => Err(HeddleError::ReservedWorktreePath {
            path: rel_path.to_path_buf(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Whether a directory child is reserved without allocating a joined path.
///
/// Used by the walker prune so root `.heddle` and pointer-cursor artifacts
/// are skipped even if a matcher is later refactored.
#[must_use]
pub fn is_reserved_directory_child(parent: &Path, name: &str) -> bool {
    if is_reserved_worktree_path(parent) {
        return true;
    }
    let at_root = is_worktree_root(parent);
    (at_root && is_reserved_root_name(std::ffi::OsStr::new(name)))
        || reserved_tree_entry_name(name.as_bytes(), at_root, false).is_some()
}

fn is_reserved_root_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name == ".heddle"
        || name == ".heddle.identity"
        || name == ".heddle.last-turn"
        || name == ".identity.lock"
        || name == ".identity.tmp"
        || name.starts_with(".identity.tmp.")
        || name.starts_with(".last-turn.tmp.")
}

fn is_worktree_root(path: &Path) -> bool {
    path.as_os_str().is_empty() || path == Path::new(".")
}

fn first_normal_component(path: &Path) -> Option<&std::ffi::OsStr> {
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(name) => return Some(name),
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        check_worktree_write_path, is_reserved_directory_child, is_reserved_worktree_path,
    };

    #[test]
    fn reserves_root_heddle_tree_and_identity() {
        for path in [
            ".heddle",
            ".heddle/identity.toml",
            ".heddle/objects/pack",
            ".heddle/info/exclude",
            "./.heddle/identity.toml",
        ] {
            assert!(
                is_reserved_worktree_path(Path::new(path)),
                "expected reserved: {path}"
            );
        }
    }

    #[test]
    fn reserves_pointer_checkout_cursor_artifacts() {
        for path in [
            ".heddle.identity",
            "./.heddle.identity",
            ".heddle.last-turn",
            ".identity.lock",
            ".identity.tmp.123.0",
            ".identity.tmp",
            ".last-turn.tmp.123.0",
        ] {
            assert!(
                is_reserved_worktree_path(Path::new(path)),
                "expected reserved: {path}"
            );
        }
    }

    #[test]
    fn does_not_reserve_nested_or_unrelated_paths() {
        for path in [
            "",
            ".",
            "src/main.rs",
            "heddle",
            ".heddleignore",
            "examples/calculator/.heddle/identity.toml",
            "examples/calculator/.heddle",
            "examples/foo/.heddle.identity",
            "examples/foo/.heddle.last-turn",
            "examples/foo/.identity.lock",
            "src/.identity.tmp.1.2",
            "../.heddle/identity.toml",
        ] {
            assert!(
                !is_reserved_worktree_path(Path::new(path)),
                "expected not reserved: {path}"
            );
        }
    }

    #[test]
    fn directory_child_reserves_root_heddle_and_descendants() {
        assert!(is_reserved_directory_child(Path::new(""), ".heddle"));
        assert!(is_reserved_directory_child(Path::new("."), ".heddle"));
        assert!(is_reserved_directory_child(
            Path::new(".heddle"),
            "identity.toml"
        ));
        assert!(is_reserved_directory_child(
            Path::new(""),
            ".heddle.identity"
        ));
        assert!(is_reserved_directory_child(
            Path::new("."),
            ".identity.lock"
        ));
        assert!(is_reserved_directory_child(
            Path::new(""),
            ".identity.tmp.9.1"
        ));
        assert!(!is_reserved_directory_child(Path::new(""), "src"));
        assert!(!is_reserved_directory_child(
            Path::new("examples/calculator"),
            ".heddle"
        ));
        assert!(!is_reserved_directory_child(
            Path::new("examples/foo"),
            ".heddle.identity"
        ));
    }

    #[test]
    fn reserves_metadata_directory_aliases() {
        for path in [
            ".git",
            ".git/hooks/pre-commit",
            "a/.git/hooks/x",
            "a/b/.GIT/config",
            "src/GIT~1",
            ".git::$INDEX_ALLOCATION/x",
            ".HEDDLE/config.toml",
            ".heddle./config.toml",
            "HEDDLE~1/hooks/pre-capture",
        ] {
            assert!(
                is_reserved_worktree_path(Path::new(path)),
                "expected reserved: {path}"
            );
            assert!(
                check_worktree_write_path(Path::new(path), false).is_err(),
                "expected write refused: {path}"
            );
        }
        assert!(is_reserved_directory_child(Path::new("vendor/lib"), ".git"));
        assert!(check_worktree_write_path(Path::new("sub/.gitmodules"), true).is_err());
        assert!(check_worktree_write_path(Path::new("sub/.gitmodules"), false).is_ok());
        assert!(is_reserved_directory_child(Path::new(""), ".HEDDLE"));
        assert!(!is_reserved_directory_child(
            Path::new("examples"),
            ".HEDDLE"
        ));
    }

    #[test]
    fn writes_to_ordinary_dotfiles_are_allowed() {
        for path in [
            ".github/workflows/ci.yml",
            ".gitignore",
            ".heddleignore",
            "examples/calculator/.heddle/config.toml",
            "src/main.rs",
        ] {
            assert!(
                check_worktree_write_path(Path::new(path), false).is_ok(),
                "expected write allowed: {path}"
            );
        }
    }
}
