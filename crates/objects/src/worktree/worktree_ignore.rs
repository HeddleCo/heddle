// SPDX-License-Identifier: Apache-2.0
//! Ignore pattern helpers for worktree operations.
//!
//! `.heddleignore` follows the same syntax as `.gitignore`: literal
//! names, leading `/` for root-anchored rules, trailing `/` for
//! directory-only matches, `*` and `**` glob wildcards, character
//! classes (`[abc]`), and `!` negation (whitelist) rules. The matcher
//! delegates to the `ignore` crate's gitignore implementation so the
//! semantics are spec-compliant; only the patterns themselves are
//! sourced from `.heddleignore` instead of `.gitignore`.
//!
//! Three "root-admin" pattern names — `.heddle`, `.heddleignore`,
//! and `.git` — get an implicit leading `/` so they match only at
//! the repo root. This preserves the long-standing invariant that a
//! nested `.heddle/` directory (e.g. an `examples/calculator/.heddle`
//! fixture) is *captured*, not silently dropped. Operators who want
//! the gitignore-spec "match anywhere" behavior for those names can
//! write `**/<name>` explicitly.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Whether `path` is covered by any of the `.heddleignore` patterns.
///
/// `is_dir = true` is passed to the underlying gitignore matcher so
/// trailing-slash rules (`target/`, `build/`) match the bare directory
/// entry itself — not just paths *inside* it. This preserves the
/// pre-existing in-house matcher's behavior, where `build/` on a bare
/// `build` path returned `true`. Walker callers depend on this to
/// prune entire directory subtrees before descending; the alternative
/// (`is_dir = false`) caused unnecessary traversal of `target/`,
/// `node_modules/`, and other build trees.
///
/// Non-directory rules (`*.log`, `node_modules`, `[Mm]akefile`) are
/// unaffected — gitignore-spec rules without a trailing slash match
/// regardless of the `is_dir` flag.
pub fn should_ignore(path: &Path, patterns: &[String]) -> bool {
    matched(&build_matcher(patterns), path)
}

/// Build a `Gitignore` matcher from the given pattern strings,
/// translating the root-admin special cases (`.heddle`,
/// `.heddleignore`, `.git`) into root-anchored gitignore syntax.
fn build_matcher(patterns: &[String]) -> Gitignore {
    // Root path is symbolic — paths fed to `matched` are interpreted
    // relative to it. Callers always pass repo-relative paths, so the
    // root just needs to be a stable, in-memory anchor.
    let mut builder = GitignoreBuilder::new("");
    for pattern in patterns {
        let line = canonical_line(pattern);
        // `add_line` returns Err only on malformed glob syntax. We
        // silently skip malformed user patterns — heddle's ingest path
        // shouldn't error on a typo'd `.heddleignore` line; it should
        // ignore the bad rule and keep going.
        let _ = builder.add_line(None, &line);
    }
    // `build()` only fails on internal compile errors. The empty
    // matcher (`Gitignore::empty()`) matches nothing — the right
    // failure mode if we get here.
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Rewrite root-admin special-case names into root-anchored
/// gitignore syntax. Pass-through for every other pattern, so
/// gitignore semantics (`*`, `**`, `[abc]`, `!negation`, trailing
/// `/`, leading `/`) all flow through verbatim.
fn canonical_line(pattern: &str) -> String {
    match pattern {
        ".heddle" => "/.heddle".to_string(),
        ".heddleignore" => "/.heddleignore".to_string(),
        ".git" => "/.git".to_string(),
        other => other.to_string(),
    }
}

/// Apply the matcher to a relative path. Whitelist (`!negation`)
/// rules unset the match; we surface only the `Ignore` outcome.
///
/// `is_dir = true`: trailing-slash rules (`build/`) match the bare
/// directory entry as well as paths inside it. See the docstring on
/// `should_ignore` for the migration rationale.
fn matched(gi: &Gitignore, path: &Path) -> bool {
    matches!(
        gi.matched_path_or_any_parents(path, /* is_dir */ true),
        ignore::Match::Ignore(_)
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_glob_extension() {
        let patterns = vec!["*.log".to_string()];
        assert!(should_ignore(&PathBuf::from("test.log"), &patterns));
        assert!(should_ignore(&PathBuf::from("debug.log"), &patterns));
        assert!(!should_ignore(&PathBuf::from("test.txt"), &patterns));
    }

    #[test]
    fn test_directory_pattern() {
        let patterns = vec!["build/".to_string()];
        assert!(should_ignore(&PathBuf::from("build/output.txt"), &patterns));
        // Bare directory match: walker callers ask `should_ignore` to
        // decide whether to prune `build/` before descending. With
        // `is_dir = true` plumbed into the gitignore matcher, the
        // trailing-slash rule fires on the directory entry itself.
        // Without this, walks of large dependency / build trees
        // (`target/`, `node_modules/`) recurse unnecessarily.
        assert!(should_ignore(&PathBuf::from("build"), &patterns));
        assert!(should_ignore(&PathBuf::from("build/anything"), &patterns));
        assert!(!should_ignore(&PathBuf::from("builder.txt"), &patterns));
    }

    #[test]
    fn dir_only_rule_covers_symlinked_deps_dir() {
        // heddle#303: a `node_modules` *symlink* (used as a workaround
        // for the isolated-checkout hydrate gap) must be covered by a
        // `node_modules/` (dir-only) rule, not treated as an uncaptured
        // path that silently blocks `ready`. The matcher is path-based
        // and always probes with `is_dir = true`, so it cannot — and
        // must not — distinguish a symlink-to-dir from a real directory:
        // the trailing-slash rule fires on the bare `node_modules` entry
        // either way. Walker/scan callers never descend a symlink, so
        // this is the entry that decides whether the link is ignored.
        let patterns = vec!["node_modules/".to_string()];
        assert!(should_ignore(&PathBuf::from("node_modules"), &patterns));
        assert!(should_ignore(
            &PathBuf::from("nested/node_modules"),
            &patterns
        ));
    }

    #[test]
    fn test_simple_pattern() {
        let patterns = vec!["node_modules".to_string()];
        assert!(should_ignore(
            &PathBuf::from("node_modules/package.json"),
            &patterns
        ));
        assert!(!should_ignore(&PathBuf::from("src/main.rs"), &patterns));
    }

    #[test]
    fn test_simple_pattern_does_not_match_prefixes() {
        let patterns = vec!["target".to_string()];
        assert!(should_ignore(
            &PathBuf::from("target/output.txt"),
            &patterns
        ));
        assert!(should_ignore(&PathBuf::from("build/target/app"), &patterns));
        assert!(!should_ignore(&PathBuf::from("target.txt"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("targeted/output.txt"),
            &patterns
        ));
    }

    #[test]
    fn test_root_admin_patterns_do_not_ignore_nested_paths() {
        let patterns = vec![".heddle".to_string(), ".heddleignore".to_string()];
        assert!(should_ignore(&PathBuf::from(".heddle/objects"), &patterns));
        assert!(should_ignore(
            &PathBuf::from(".heddle/state/index.bin"),
            &patterns
        ));
        assert!(should_ignore(&PathBuf::from(".heddleignore"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddle/objects"),
            &patterns
        ));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddle/state/index.bin"),
            &patterns
        ));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddleignore"),
            &patterns
        ));
    }

    // ---- New gitignore-spec coverage ----

    #[test]
    fn test_path_relative_glob_matches_specific_directory_only() {
        // `config/*.toml` is the case the user called out — a glob
        // anchored to a specific subdirectory, with `*` matching one
        // path segment. Plain `secrets.toml` at the root must NOT be
        // ignored.
        let patterns = vec!["config/*.toml".to_string()];
        assert!(should_ignore(
            &PathBuf::from("config/secrets.toml"),
            &patterns
        ));
        assert!(should_ignore(
            &PathBuf::from("config/database.toml"),
            &patterns
        ));
        assert!(!should_ignore(&PathBuf::from("secrets.toml"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("other/secrets.toml"),
            &patterns
        ));
    }

    #[test]
    fn test_double_star_recursive_glob_descends_directories() {
        // `**/*.pem` matches at any depth — the canonical "find every
        // PEM key under any directory" pattern.
        let patterns = vec!["**/*.pem".to_string()];
        assert!(should_ignore(&PathBuf::from("dev.pem"), &patterns));
        assert!(should_ignore(&PathBuf::from("keys/dev.pem"), &patterns));
        assert!(should_ignore(
            &PathBuf::from("nested/deeper/key.pem"),
            &patterns
        ));
        assert!(!should_ignore(&PathBuf::from("dev.txt"), &patterns));
    }

    #[test]
    fn test_negation_rule_whitelists_a_path() {
        // `*.log` then `!keep.log` — the negation rule unsets the
        // earlier match for that specific name.
        let patterns = vec!["*.log".to_string(), "!keep.log".to_string()];
        assert!(should_ignore(&PathBuf::from("debug.log"), &patterns));
        assert!(!should_ignore(&PathBuf::from("keep.log"), &patterns));
    }

    #[test]
    fn test_leading_slash_anchors_to_root_only() {
        // `/build` (root-anchored) ignores the top-level `build/` but
        // not a nested `nested/build/` directory. Distinct semantics
        // from the bare `build` pattern, which matches anywhere.
        let patterns = vec!["/build".to_string()];
        assert!(should_ignore(&PathBuf::from("build/output"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("nested/build/file"),
            &patterns
        ));
    }

    #[test]
    fn test_character_class_matches_set() {
        // `[Mm]akefile` — matches uppercase or lowercase variants.
        // Standard gitignore character class.
        let patterns = vec!["[Mm]akefile".to_string()];
        assert!(should_ignore(&PathBuf::from("Makefile"), &patterns));
        assert!(should_ignore(&PathBuf::from("makefile"), &patterns));
        assert!(!should_ignore(&PathBuf::from("Rakefile"), &patterns));
    }

    #[test]
    fn test_comments_and_blank_lines_are_handled_upstream() {
        // The matcher itself accepts every line it's given verbatim
        // (gitignore-spec treats `#` as a comment marker). Repository
        // strips comments before calling, but verify the matcher
        // tolerates them so a future refactor can stop stripping
        // without behavior change.
        let patterns = vec!["# comment".to_string(), "".to_string(), "*.log".to_string()];
        assert!(should_ignore(&PathBuf::from("foo.log"), &patterns));
        assert!(!should_ignore(&PathBuf::from("foo.txt"), &patterns));
    }

    #[test]
    fn test_malformed_pattern_does_not_break_matcher() {
        // Unbalanced bracket: builder errors silently and the
        // pattern is dropped. Other rules continue to apply.
        let patterns = vec!["[unbalanced".to_string(), "*.log".to_string()];
        assert!(should_ignore(&PathBuf::from("foo.log"), &patterns));
    }
}
