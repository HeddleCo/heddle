// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

use objects::object::ContentHash;

#[derive(Debug, Default, Clone)]
pub(crate) struct WorktreeIgnoreMatcher {
    patterns: Vec<CompiledPattern>,
    /// Canonical absolute paths of *other* threads' worktrees that
    /// happen to be nested under the walk root. Populated once per
    /// scan via [`Repository::nested_thread_worktree_exclusions`] and
    /// checked per directory entry by absolute path. The `ignore`
    /// crate doesn't model this case (each worktree is tracked by
    /// Heddle, not by `.heddleignore`), so we plumb it through the
    /// same matcher to keep the walker's filter surface uniform.
    nested_worktree_exclusions: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
enum CompiledPattern {
    RootAdmin(String),
    Directory(String),
    Suffix(String),
    Component(String),
}

impl WorktreeIgnoreMatcher {
    pub(crate) fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| compile_pattern(pattern))
                .collect(),
            nested_worktree_exclusions: Vec::new(),
        }
    }

    /// Attach the canonical absolute paths of other thread worktrees
    /// that should be skipped during the walk. Caller is expected to
    /// canonicalize ahead of time so we don't pay an `fs::canonicalize`
    /// per directory entry. Empty paths are silently dropped.
    pub(crate) fn with_nested_worktree_exclusions(mut self, paths: Vec<PathBuf>) -> Self {
        self.nested_worktree_exclusions = paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        self
    }

    /// Whether the given absolute path matches a nested-thread-worktree
    /// exclusion. Used by the walker to skip descending into a sibling
    /// thread's worktree that happens to live under the current
    /// thread's walk root.
    pub(crate) fn should_prune_absolute_path(&self, absolute: &Path) -> bool {
        if self.nested_worktree_exclusions.is_empty() {
            return false;
        }
        self.nested_worktree_exclusions
            .iter()
            .any(|excluded| paths_equivalent(excluded, absolute))
    }

    #[cfg(test)]
    pub(crate) fn should_ignore(&self, path: &Path) -> bool {
        self.patterns.iter().any(|pattern| pattern.matches(path))
    }

    pub(crate) fn should_ignore_child(&self, parent: &Path, name: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.matches_child(parent, name))
    }

    pub(crate) fn should_prune_directory_child(&self, parent: &Path, name: &str) -> bool {
        self.should_ignore_child(parent, name)
    }

    pub(crate) fn fingerprint(&self) -> ContentHash {
        let mut canonical_patterns: Vec<String> = self
            .patterns
            .iter()
            .map(CompiledPattern::canonical_repr)
            .collect();
        canonical_patterns.sort();
        ContentHash::compute_typed("heddle.ignore", canonical_patterns.join("\0").as_bytes())
    }
}

impl CompiledPattern {
    #[cfg(test)]
    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::RootAdmin(pattern) => is_root_path_match(path, pattern),
            Self::Directory(pattern) => is_dir_match(path, pattern),
            Self::Suffix(suffix) => matches_suffix(path, suffix),
            Self::Component(pattern) => has_matching_component(path, pattern),
        }
    }

    fn matches_child(&self, parent: &Path, name: &str) -> bool {
        match self {
            Self::RootAdmin(pattern) => {
                root_component(parent, name).is_some_and(|value| value == pattern)
            }
            Self::Directory(pattern) => {
                name == pattern || parent_components(parent).any(|component| component == pattern)
            }
            Self::Suffix(suffix) => {
                name.ends_with(suffix)
                    || parent_components(parent).any(|component| component.ends_with(suffix))
            }
            Self::Component(pattern) => {
                name == pattern || parent_components(parent).any(|component| component == pattern)
            }
        }
    }

    fn canonical_repr(&self) -> String {
        match self {
            Self::RootAdmin(pattern) => format!("root:{pattern}"),
            Self::Directory(pattern) => format!("directory:{pattern}"),
            Self::Suffix(pattern) => format!("suffix:{pattern}"),
            Self::Component(pattern) => format!("component:{pattern}"),
        }
    }
}

fn compile_pattern(pattern: &str) -> CompiledPattern {
    if is_root_admin_pattern(pattern) {
        return CompiledPattern::RootAdmin(pattern.to_string());
    }
    if pattern.ends_with('/') {
        return CompiledPattern::Directory(pattern.trim_end_matches('/').to_string());
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return CompiledPattern::Suffix(suffix.to_string());
    }
    CompiledPattern::Component(pattern.to_string())
}

fn is_root_admin_pattern(pattern: &str) -> bool {
    matches!(pattern, ".heddle" | ".heddleignore" | ".git")
}

#[cfg(test)]
fn is_root_path_match(path: &Path, pattern: &str) -> bool {
    root_component(path, "").is_some_and(|value| value == pattern)
}

#[cfg(test)]
fn is_dir_match(path: &Path, dir_pattern: &str) -> bool {
    let path_str = path.to_string_lossy();
    if path_str.starts_with(&format!("{dir_pattern}/")) {
        return true;
    }
    has_matching_component(path, dir_pattern)
}

#[cfg(test)]
fn matches_suffix(path: &Path, suffix: &str) -> bool {
    let path_str = path.to_string_lossy();
    if path_str.ends_with(suffix) {
        return true;
    }
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.ends_with(suffix))
    })
}

#[cfg(test)]
fn has_matching_component(path: &Path, pattern: &str) -> bool {
    parent_components(path).any(|component| component == pattern)
}

fn root_component<'a>(parent: &'a Path, name: &'a str) -> Option<&'a str> {
    parent_components(parent)
        .next()
        .or_else(|| (!name.is_empty()).then_some(name))
}

fn parent_components(path: &Path) -> impl Iterator<Item = &str> {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
}

/// Compare two paths for equivalence. Tries the cheap pointer-equal
/// case first, then falls back to canonicalization so symlinked or
/// `./`-laden inputs still match. Failure to canonicalize is treated
/// as "not equal" — better to over-walk than to silently drop a
/// directory the user actually wants captured.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a_can), Ok(b_can)) => a_can == b_can,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use objects::worktree::should_ignore;

    use super::WorktreeIgnoreMatcher;

    #[test]
    fn compiled_matcher_matches_existing_ignore_semantics() {
        let patterns = vec![
            ".heddle".to_string(),
            ".heddleignore".to_string(),
            ".git".to_string(),
            "build/".to_string(),
            "node_modules".to_string(),
            "*.log".to_string(),
            "target".to_string(),
        ];
        let matcher = WorktreeIgnoreMatcher::new(&patterns);
        let paths = [
            PathBuf::from(".heddle/objects"),
            PathBuf::from("examples/calculator/.heddle/objects"),
            PathBuf::from("build"),
            PathBuf::from("build/output.txt"),
            PathBuf::from("builder.txt"),
            PathBuf::from("node_modules/package.json"),
            PathBuf::from("src/main.rs"),
            PathBuf::from("debug.log"),
            PathBuf::from("nested/debug.log"),
            PathBuf::from("target/output.txt"),
            PathBuf::from("targeted/output.txt"),
        ];

        for path in paths {
            assert_eq!(
                matcher.should_ignore(&path),
                should_ignore(&path, &patterns)
            );
        }
    }

    #[test]
    fn compiled_child_matcher_matches_existing_ignore_semantics() {
        let patterns = vec![
            ".heddle".to_string(),
            ".heddleignore".to_string(),
            ".git".to_string(),
            "build/".to_string(),
            "node_modules".to_string(),
            "*.log".to_string(),
            "target".to_string(),
        ];
        let matcher = WorktreeIgnoreMatcher::new(&patterns);
        let paths = [
            PathBuf::from(".heddle/objects"),
            PathBuf::from("examples/calculator/.heddle/objects"),
            PathBuf::from("build"),
            PathBuf::from("build/output.txt"),
            PathBuf::from("builder.txt"),
            PathBuf::from("node_modules/package.json"),
            PathBuf::from("src/main.rs"),
            PathBuf::from("debug.log"),
            PathBuf::from("nested/debug.log"),
            PathBuf::from("target/output.txt"),
            PathBuf::from("targeted/output.txt"),
        ];

        for path in paths {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            assert_eq!(
                matcher.should_ignore_child(parent, name),
                should_ignore(&path, &patterns)
            );
        }
    }

    #[test]
    fn matcher_fingerprint_is_order_independent_for_equivalent_patterns() {
        let matcher_a = WorktreeIgnoreMatcher::new(&[
            "build/".to_string(),
            "*.log".to_string(),
            ".git".to_string(),
        ]);
        let matcher_b = WorktreeIgnoreMatcher::new(&[
            ".git".to_string(),
            "*.log".to_string(),
            "build/".to_string(),
        ]);

        assert_eq!(matcher_a.fingerprint(), matcher_b.fingerprint());
    }

    #[test]
    fn matcher_fingerprint_changes_when_ignore_semantics_change() {
        let matcher_a = WorktreeIgnoreMatcher::new(&["build/".to_string()]);
        let matcher_b = WorktreeIgnoreMatcher::new(&["build".to_string()]);

        assert_ne!(matcher_a.fingerprint(), matcher_b.fingerprint());
    }
}