// SPDX-License-Identifier: Apache-2.0
//! Runtime git-process dependency lint.
//!
//! Heddle's public Git-overlay workflows must not depend on a `git`
//! executable being present on PATH. A few process calls are still
//! intentional: explicit Git escape hatches, best-effort diagnostics,
//! or optional fallback paths that degrade cleanly. This lint makes
//! that inventory reviewable instead of letting new `git` spawns land
//! unnoticed.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const ALLOWED_GIT_SPAWNS: &[(&str, &str, &str)] = &[
    (
        "crates/cli/src/cli/commands/doctor_schemas.rs",
        "find_repo_root",
        "best-effort repo-root fallback when no .heddle/.git ancestor is visible; explicit --repo avoids it",
    ),
    (
        "crates/cli/src/bridge/git_core.rs",
        "resolve_remote_default_branch",
        "optional remote HEAD hint; missing git returns None and callers fall back",
    ),
    (
        "crates/cli/src/bridge/git_core.rs",
        "clone_url_to_bare_via_git",
        "optional partial/filter clone escape hatch where native gix cannot honor the requested capability",
    ),
    (
        "crates/cli/src/cli/commands/clone.rs",
        "read_blob_bytes",
        "optional lazy partial-clone promisor hydration after local object lookup misses",
    ),
    (
        "crates/cli/src/cli/commands/clone.rs",
        "git_available",
        "best-effort clone verification probe; missing git keeps structural Heddle/Git HEAD checks",
    ),
    (
        "crates/cli/src/cli/commands/clone.rs",
        "run_git_clone_step",
        "post-clone Git-clean validation path used only when git is present",
    ),
    (
        "crates/cli/src/cli/commands/clone.rs",
        "git_output",
        "post-clone git status validation path used only when git is present",
    ),
    (
        "crates/cli/src/cli/commands/checkpoint.rs",
        "git_rev_parse_head",
        "Git-overlay checkpoint audit trail records previous/new Git OIDs after a Heddle state is preserved",
    ),
    (
        "crates/cli/src/cli/commands/git_overlay_health.rs",
        "build_plain_git_trust_probe",
        "shared plain-Git first-run trust probe that must not initialize .heddle as a side effect",
    ),
    (
        "crates/cli/src/cli/commands/git_overlay_health.rs",
        "git_probe_stdout",
        "shared plain-Git trust probe for branch and dirty-summary hints before Heddle exists",
    ),
    (
        "crates/cli/src/cli/commands/undo_apply.rs",
        "git_stdout",
        "Git checkpoint undo/redo safety preflight verifies Git HEAD/worktree before moving refs",
    ),
    (
        "crates/cli/src/cli/commands/oss.rs",
        "cmd_version",
        "best-effort bug-context probe; missing git serializes git_version=null",
    ),
    (
        "crates/cli/src/cli/commands/operator_core.rs",
        "git_unmerged_paths",
        "raw Git operation recovery helper for externally-started Git control flows",
    ),
    (
        "crates/cli/src/cli/commands/operator_core.rs",
        "run_git_control_attempt",
        "raw Git continue/abort helper for externally-started Git control flows",
    ),
    (
        "crates/cli/src/cli/commands/merge/mod.rs",
        "validate_git_commit_preconditions_extended",
        "explicit --git-commit preflight; plain Heddle merge does not require this path",
    ),
    (
        "crates/cli/src/cli/commands/merge/git_commit.rs",
        "validate_git_state",
        "explicit --git-commit preflight against Git index/branch state",
    ),
    (
        "crates/cli/src/cli/commands/merge/git_commit.rs",
        "write_git_commit",
        "explicit --git-commit operation that intentionally creates a Git commit",
    ),
    (
        "crates/repo/src/repository.rs",
        "git_remote_tracking_status",
        "supported read path with missing-git fallback to remote_tracking=null",
    ),
    (
        "crates/repo/src/repository.rs",
        "git_overlay_worktree_status",
        "supported read path with missing-git fallback to native Heddle tree comparison",
    ),
];

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct SpawnSite {
    file: String,
    function: String,
    line: usize,
    source: String,
}

#[test]
fn runtime_git_process_spawns_match_reviewed_allowlist() {
    let workspace = workspace_root();
    let mut sites = Vec::new();
    for dir in [
        workspace.join("crates/cli/src"),
        workspace.join("crates/repo/src"),
    ] {
        walk_rust_files(&dir, &mut |path| scan_file(&workspace, path, &mut sites));
    }

    let allowed: BTreeMap<(&str, &str), &str> = ALLOWED_GIT_SPAWNS
        .iter()
        .map(|(file, function, reason)| ((*file, *function), *reason))
        .collect();

    let mut unexpected = Vec::new();
    let mut seen = BTreeSet::new();
    for site in &sites {
        let key = (site.file.as_str(), site.function.as_str());
        if let Some(reason) = allowed.get(&key) {
            assert!(
                !reason.trim().is_empty(),
                "allowlist reason must be nonempty"
            );
            seen.insert(key);
        } else {
            unexpected.push(site.clone());
        }
    }

    assert!(
        unexpected.is_empty(),
        "unreviewed runtime `git` process spawn(s):\n{}\nAdd only intentional optional escape hatches to ALLOWED_GIT_SPAWNS with a reason, or replace the call with native/gix behavior.",
        unexpected
            .iter()
            .map(|site| format!(
                "  {}:{} in {}: {}",
                site.file, site.line, site.function, site.source
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let missing: Vec<_> = allowed
        .keys()
        .copied()
        .filter(|key| !seen.contains(key))
        .collect();
    assert!(
        missing.is_empty(),
        "git-process allowlist entry no longer matches a production spawn; remove or update it: {missing:?}"
    );
}

fn scan_file(workspace: &Path, path: &Path, sites: &mut Vec<SpawnSite>) {
    let rel = path
        .strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy();
    if rel.ends_with("_tests.rs") {
        return;
    }

    let source =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let mut function = String::from("<module>");
    let mut pending_cfg_test = false;
    let mut in_test_module = false;
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if in_test_module {
            continue;
        }
        if trimmed == "#[cfg(test)]" {
            pending_cfg_test = true;
            continue;
        }
        if pending_cfg_test {
            if trimmed.starts_with("mod tests") && trimmed.contains('{') {
                in_test_module = true;
                continue;
            }
            if !trimmed.starts_with('#') && !trimmed.is_empty() {
                pending_cfg_test = false;
            }
        }
        if let Some(name) = parse_function_name(trimmed) {
            function = name.to_string();
        }
        if is_git_spawn(trimmed) {
            sites.push(SpawnSite {
                file: rel.to_string(),
                function: function.clone(),
                line: idx + 1,
                source: trimmed.to_string(),
            });
        }
    }
}

fn is_git_spawn(line: &str) -> bool {
    line.contains("Command::new(\"git\")")
        || line.contains("ProcessCommand::new(\"git\")")
        || line.contains("std::process::Command::new(\"git\")")
}

fn parse_function_name(line: &str) -> Option<&str> {
    let fn_pos = line.find("fn ")?;
    let after = &line[fn_pos + 3..];
    let name_end = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after.len());
    let name = &after[..name_end];
    (!name.is_empty()).then_some(name)
}

fn walk_rust_files(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()));
    for entry in entries {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            walk_rust_files(&path, visit);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}
