// SPDX-License-Identifier: Apache-2.0
//! Classify `diff` positionals as states vs worktree/path filters.

use std::path::Path;

use repo::Repository;

/// Resolved `diff` refs after path-shaped positionals are peeled off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassifiedDiffRefs {
    pub from: Option<String>,
    pub to: Option<String>,
    pub paths: Vec<String>,
}

/// Split `from`/`to` into states and path filters.
///
/// A spec that resolves as a state stays a state. Otherwise a path-shaped
/// value (separator, extension, or a path that exists under `root`) becomes
/// a worktree filter instead of a missing state. Arguments collected after
/// `--` and `--path` are always filters.
pub fn classify_diff_refs(
    root: &Path,
    repo: Option<&Repository>,
    from: Option<String>,
    to: Option<String>,
    extra_paths: Vec<String>,
) -> ClassifiedDiffRefs {
    let mut paths = extra_paths;
    let from = classify_spec(root, repo, from, &mut paths);
    let to = classify_spec(root, repo, to, &mut paths);
    ClassifiedDiffRefs { from, to, paths }
}

fn classify_spec(
    root: &Path,
    repo: Option<&Repository>,
    spec: Option<String>,
    paths: &mut Vec<String>,
) -> Option<String> {
    let spec = spec?;
    if spec_is_state(repo, &spec) {
        return Some(spec);
    }
    if spec_is_path_filter(root, &spec) {
        paths.push(spec);
        return None;
    }
    Some(spec)
}

fn spec_is_state(repo: Option<&Repository>, spec: &str) -> bool {
    is_head_spec(spec) || repo.is_some_and(|repo| repo.resolve_state(spec).ok().flatten().is_some())
}

fn is_head_spec(spec: &str) -> bool {
    spec == "HEAD"
        || spec == "@"
        || spec
            .strip_prefix("HEAD~")
            .or_else(|| spec.strip_prefix("@~"))
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
}

fn spec_is_path_filter(root: &Path, spec: &str) -> bool {
    looks_path_shaped(spec) || root.join(spec).exists()
}

fn looks_path_shaped(spec: &str) -> bool {
    spec.contains('/') || spec.contains('\\') || Path::new(spec).extension().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn path_shaped_positional_becomes_filter() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("NOTES.md"), "dirty\n").unwrap();
        let classified = classify_diff_refs(
            root.path(),
            None,
            Some("NOTES.md".to_string()),
            None,
            Vec::new(),
        );
        assert_eq!(classified.from, None);
        assert_eq!(classified.to, None);
        assert_eq!(classified.paths, ["NOTES.md"]);
    }

    #[test]
    fn separator_paths_stay_filters() {
        let root = TempDir::new().unwrap();
        let classified =
            classify_diff_refs(root.path(), None, None, None, vec!["NOTES.md".to_string()]);
        assert_eq!(classified.paths, ["NOTES.md"]);
        assert_eq!(classified.from, None);
    }

    #[test]
    fn head_specs_stay_states() {
        let root = TempDir::new().unwrap();
        let classified = classify_diff_refs(
            root.path(),
            None,
            Some("HEAD~1".to_string()),
            Some("HEAD".to_string()),
            Vec::new(),
        );
        assert_eq!(classified.from.as_deref(), Some("HEAD~1"));
        assert_eq!(classified.to.as_deref(), Some("HEAD"));
        assert!(classified.paths.is_empty());
    }

    #[test]
    fn existing_directory_without_separator_is_a_filter() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let classified =
            classify_diff_refs(root.path(), None, Some("src".to_string()), None, Vec::new());
        assert_eq!(classified.from, None);
        assert_eq!(classified.paths, ["src"]);
    }
}
