// SPDX-License-Identifier: Apache-2.0
//! Compiled-once `.heddleignore` matcher for hot-path walker use.
//!
//! Backed by `ignore::gitignore::Gitignore` so `.heddleignore`
//! supports the full gitignore syntax: `*` / `**` globs, character
//! classes (`[abc]`), `!` negation, leading `/` for root-anchored,
//! trailing `/` for directory-only. See
//! `crates/objects/src/worktree/worktree_ignore.rs` for the matcher
//! contract documentation; this file mirrors the same rules but
//! pre-compiles them once per walk instead of rebuilding for each
//! path test.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use objects::object::ContentHash;

#[derive(Debug, Default, Clone)]
pub(crate) struct WorktreeIgnoreMatcher {
    /// Compiled gitignore matcher. `None` only for the
    /// `Default::default()` empty matcher; otherwise always set.
    matcher: Option<Gitignore>,
    /// Raw pattern strings, retained for `fingerprint()` — the
    /// fingerprint stays stable across walker re-entries iff the
    /// patterns are byte-identical, modulo ordering.
    raw_patterns: Vec<String>,
    /// Canonical absolute paths of *other* threads' worktrees that
    /// happen to be nested under the walk root. Populated once per
    /// scan via [`Repository::nested_thread_worktree_exclusions`] and
    /// checked per directory entry by absolute path. The `ignore`
    /// crate doesn't model this case (each worktree is tracked by
    /// Heddle, not by `.heddleignore`), so we plumb it through the
    /// same matcher to keep the walker's filter surface uniform.
    nested_worktree_exclusions: Vec<PathBuf>,
}

impl WorktreeIgnoreMatcher {
    pub(crate) fn new(patterns: &[String]) -> Self {
        Self {
            matcher: Some(build_matcher(patterns)),
            raw_patterns: patterns.to_vec(),
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

    /// Top-level "is this path ignored" probe. Matches the free-function
    /// matcher in `objects::worktree::should_ignore`: passes
    /// `is_dir = true` so trailing-slash rules (`build/`) fire on the
    /// bare directory entry as well as on paths inside it.
    #[cfg(test)]
    pub(crate) fn should_ignore(&self, path: &Path) -> bool {
        self.matched_relative(path, /* is_dir */ true)
    }

    /// Test-only "would this child be ignored as a file/dir entry"
    /// probe. Asserts agreement with the free-function matcher (which
    /// also uses `is_dir = true`); production callers reach for
    /// `should_prune_directory_child` instead.
    #[cfg(test)]
    pub(crate) fn should_ignore_child(&self, parent: &Path, name: &str) -> bool {
        let path = parent.join(name);
        self.matched_relative(&path, /* is_dir */ true)
    }

    pub(crate) fn should_prune_directory_child(&self, parent: &Path, name: &str) -> bool {
        let path = parent.join(name);
        // Directory descent: tell gitignore the entry is a dir so
        // `build/` rules fire. The walker uses this signal to decide
        // whether to descend; it's the right place to be strict.
        self.matched_relative(&path, /* is_dir */ true)
    }

    fn matched_relative(&self, path: &Path, is_dir: bool) -> bool {
        let Some(gi) = &self.matcher else {
            return false;
        };
        matches!(
            gi.matched_path_or_any_parents(path, is_dir),
            ignore::Match::Ignore(_)
        )
    }

    pub(crate) fn fingerprint(&self) -> ContentHash {
        // Hash patterns in declaration order — gitignore semantics are
        // *order-sensitive*. `*.log` followed by `!keep.log` ignores
        // every log file except `keep.log`; the same rules in reverse
        // (`!keep.log` then `*.log`) ignore *every* log file because the
        // negation is unset by the later catch-all. Two `.heddleignore`
        // files with identical rule sets but different orders produce
        // different ignore semantics, and the untracked-cache layer
        // needs cache keys to reflect that.
        ContentHash::compute_typed("heddle.ignore", self.raw_patterns.join("\0").as_bytes())
    }
}

/// Build a `Gitignore` matcher from raw pattern strings, applying
/// heddle's root-admin special-cases (`.heddle`, `.heddleignore`,
/// `.git` become root-anchored `/.heddle`, etc.). See the matching
/// helper in `objects::worktree::worktree_ignore`.
fn build_matcher(patterns: &[String]) -> Gitignore {
    let mut builder = GitignoreBuilder::new("");
    for pattern in patterns {
        let line = canonical_line(pattern);
        let _ = builder.add_line(None, &line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn canonical_line(pattern: &str) -> String {
    match pattern {
        ".heddle" => "/.heddle".to_string(),
        ".heddleignore" => "/.heddleignore".to_string(),
        ".git" => "/.git".to_string(),
        other => other.to_string(),
    }
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
                should_ignore(&path, &patterns),
                "compiled matcher must agree with the free-function matcher on '{}'",
                path.display(),
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
                should_ignore(&path, &patterns),
                "should_ignore_child must agree with the free-function matcher on '{}'",
                path.display(),
            );
        }
    }

    #[test]
    fn matcher_fingerprint_changes_when_pattern_order_changes() {
        // Gitignore semantics are order-sensitive — `*.log` followed
        // by `!keep.log` matches differently from the reverse order.
        // The fingerprint must reflect that so cached untracked-walk
        // results invalidate when an operator reorders rules.
        let matcher_a = WorktreeIgnoreMatcher::new(&["*.log".to_string(), "!keep.log".to_string()]);
        let matcher_b = WorktreeIgnoreMatcher::new(&["!keep.log".to_string(), "*.log".to_string()]);
        assert_ne!(
            matcher_a.fingerprint(),
            matcher_b.fingerprint(),
            "fingerprint must distinguish rule orders (negation semantics)"
        );
    }

    #[test]
    fn matcher_fingerprint_changes_when_ignore_semantics_change() {
        let matcher_a = WorktreeIgnoreMatcher::new(&["build/".to_string()]);
        let matcher_b = WorktreeIgnoreMatcher::new(&["build".to_string()]);

        assert_ne!(matcher_a.fingerprint(), matcher_b.fingerprint());
    }

    // ---- New gitignore-spec coverage on the compiled matcher ----

    #[test]
    fn compiled_matcher_supports_path_relative_globs() {
        let matcher = WorktreeIgnoreMatcher::new(&["config/*.toml".to_string()]);
        assert!(matcher.should_ignore(&PathBuf::from("config/secrets.toml")));
        assert!(!matcher.should_ignore(&PathBuf::from("secrets.toml")));
        assert!(!matcher.should_ignore(&PathBuf::from("other/secrets.toml")));
    }

    #[test]
    fn compiled_matcher_supports_double_star_globs() {
        let matcher = WorktreeIgnoreMatcher::new(&["**/*.pem".to_string()]);
        assert!(matcher.should_ignore(&PathBuf::from("dev.pem")));
        assert!(matcher.should_ignore(&PathBuf::from("keys/dev.pem")));
        assert!(matcher.should_ignore(&PathBuf::from("nested/deeper/key.pem")));
    }

    #[test]
    fn compiled_matcher_supports_negation_rules() {
        let matcher = WorktreeIgnoreMatcher::new(&["*.log".to_string(), "!keep.log".to_string()]);
        assert!(matcher.should_ignore(&PathBuf::from("debug.log")));
        assert!(!matcher.should_ignore(&PathBuf::from("keep.log")));
    }

    #[test]
    fn compiled_matcher_supports_root_anchored_patterns() {
        let matcher = WorktreeIgnoreMatcher::new(&["/build".to_string()]);
        assert!(matcher.should_ignore(&PathBuf::from("build/output")));
        assert!(!matcher.should_ignore(&PathBuf::from("nested/build/file")));
    }
}
