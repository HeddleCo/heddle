// SPDX-License-Identifier: Apache-2.0
//! Reserved worktree paths that user ignore rules cannot un-ignore.
//!
//! Root `.heddle/` holds identity material (`identity.toml`), credentials,
//! and repository-engine state. Gitignore last-match-wins would otherwise
//! let `!.heddle/` pull that tree into capture. Nested `.heddle/`
//! directories (fixtures) stay ordinary content.

use std::path::{Component, Path};

/// Whether `path` is the worktree-root `.heddle` entry or a path under it.
///
/// Root-anchored only: `examples/calculator/.heddle/` is not reserved.
/// Leading `./` is skipped so `./.heddle/identity.toml` matches.
#[must_use]
pub fn is_reserved_worktree_path(path: &Path) -> bool {
    first_normal_component(path).is_some_and(|name| name == ".heddle")
}

/// Whether a directory child is reserved without allocating a joined path.
///
/// Used by the walker prune so root `.heddle` and anything already under
/// that tree are skipped even if a matcher is later refactored.
#[must_use]
pub fn is_reserved_directory_child(parent: &Path, name: &str) -> bool {
    if is_reserved_worktree_path(parent) {
        return true;
    }
    name == ".heddle" && is_worktree_root(parent)
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

    use super::{is_reserved_directory_child, is_reserved_worktree_path};

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
    fn does_not_reserve_nested_or_unrelated_paths() {
        for path in [
            "",
            ".",
            "src/main.rs",
            "heddle",
            ".heddleignore",
            "examples/calculator/.heddle/identity.toml",
            "examples/calculator/.heddle",
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
        assert!(!is_reserved_directory_child(Path::new(""), "src"));
        assert!(!is_reserved_directory_child(
            Path::new("examples/calculator"),
            ".heddle"
        ));
    }
}
