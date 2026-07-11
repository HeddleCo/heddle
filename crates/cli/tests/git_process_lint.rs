// SPDX-License-Identifier: Apache-2.0
//! Runtime git-process dependency lint.
//!
//! Heddle's public Git-overlay workflows must not depend on a `git`
//! executable being present on PATH. Git-format work is handled by
//! native code through Sley; tests and fixture builders may shell out to Git,
//! but runtime CLI crates may not.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const ALLOWED_GIT_SPAWNS: &[(&str, &str, &str)] = &[];

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
    for dir in default_cli_runtime_source_dirs(&workspace) {
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
        "runtime `git` process spawn(s) are not allowed:\n{}\nReplace the call with native/Sley behavior or move fixture setup into tests.",
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

#[test]
fn git_engine_dependency_is_sley_not_gix() {
    let workspace = workspace_root();
    let root_manifest =
        fs::read_to_string(workspace.join("Cargo.toml")).expect("read workspace Cargo.toml");
    assert!(
        root_manifest
            .lines()
            .any(|line| line.trim_start().starts_with("sley = ")),
        "workspace dependencies must name Sley as the Git-format engine"
    );

    let mut manifests = Vec::new();
    collect_manifest_files(&workspace, &mut manifests);
    let mut direct_gix_mentions = Vec::new();
    for manifest in manifests {
        let rel = manifest
            .strip_prefix(&workspace)
            .unwrap_or(&manifest)
            .display()
            .to_string();
        let body = fs::read_to_string(&manifest)
            .unwrap_or_else(|err| panic!("read {}: {err}", manifest.display()));
        for (idx, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("gix")
                || trimmed.starts_with("gitoxide")
                || trimmed.contains("package = \"gix")
            {
                direct_gix_mentions.push(format!("{rel}:{}: {trimmed}", idx + 1));
            }
        }
    }
    assert!(
        direct_gix_mentions.is_empty(),
        "Heddle should depend on Sley, not direct gix/gitoxide crates:\n{}",
        direct_gix_mentions.join("\n")
    );
}

fn scan_file(workspace: &Path, path: &Path, sites: &mut Vec<SpawnSite>) {
    let rel = path
        .strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy();
    // Test modules (whether a `*_tests.rs` sibling or a bare `tests.rs`
    // submodule file) may shell out to Git for fixture setup; they are not
    // runtime code, so they are exempt from the no-git-on-PATH lint. This
    // mirrors the inline `#[cfg(test)] mod tests { .. }` skip below.
    if rel.ends_with("_tests.rs") || rel.ends_with("/tests.rs") {
        return;
    }

    let source =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let mut function = String::from("<module>");
    let mut git_command_aliases = BTreeSet::new();
    let mut pending_command_new: Option<(usize, String, String, usize)> = None;
    let mut pending_cfg_test = false;
    let mut test_module_depth: Option<usize> = None;
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(depth) = test_module_depth {
            let depth = brace_depth_after_line(depth, line);
            test_module_depth = (depth > 0).then_some(depth);
            continue;
        }
        if trimmed == "#[cfg(test)]" {
            pending_cfg_test = true;
            continue;
        }
        if pending_cfg_test {
            if trimmed.starts_with("mod tests") && trimmed.contains('{') {
                let depth = brace_depth_after_line(0, line);
                test_module_depth = (depth > 0).then_some(depth);
                continue;
            }
            if !trimmed.starts_with('#') && !trimmed.is_empty() {
                pending_cfg_test = false;
            }
        }
        if let Some(name) = parse_function_name(trimmed) {
            function = name.to_string();
            git_command_aliases.clear();
            pending_command_new = None;
        }
        if let Some((line, source, function, remaining)) = pending_command_new.take() {
            if line_mentions_git_command_arg(trimmed, &git_command_aliases) {
                sites.push(SpawnSite {
                    file: rel.to_string(),
                    function,
                    line,
                    source: format!("{source} {}", trimmed.trim()),
                });
                continue;
            }
            if remaining > 0 && !trimmed.contains(')') {
                pending_command_new = Some((line, source, function, remaining - 1));
            }
        }
        if let Some(alias) = parse_git_command_alias(trimmed) {
            git_command_aliases.insert(alias.to_ascii_lowercase());
        }
        if is_git_spawn_with_aliases(trimmed, &git_command_aliases) {
            sites.push(SpawnSite {
                file: rel.to_string(),
                function: function.clone(),
                line: idx + 1,
                source: trimmed.to_string(),
            });
        } else if starts_multiline_command_new(trimmed) {
            pending_command_new = Some((idx + 1, trimmed.to_string(), function.clone(), 4));
        }
    }
}

fn collect_manifest_files(dir: &Path, manifests: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()));
    for entry in entries {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | "target")) {
                continue;
            }
            collect_manifest_files(&path, manifests);
        } else if entry.file_name() == "Cargo.toml" {
            manifests.push(path);
        }
    }
}

fn is_git_spawn(line: &str) -> bool {
    let compact = line.split_whitespace().collect::<String>();
    let lower = compact.to_ascii_lowercase();
    lower.contains("command::new(\"git\")")
        || lower.contains("processcommand::new(\"git\")")
        || lower.contains("command::new(r#\"git\"#)")
        || lower.contains("command::new(git")
        || lower.contains("command::new(&git")
        || shell_wrapper_mentions_git(&lower)
}

fn is_git_spawn_with_aliases(line: &str, aliases: &BTreeSet<String>) -> bool {
    if is_git_spawn(line) {
        return true;
    }
    let compact = line
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase();
    aliases.iter().any(|alias| {
        compact.contains(&format!("command::new({alias})"))
            || compact.contains(&format!("command::new(&{alias})"))
            || compact.contains(&format!("processcommand::new({alias})"))
            || compact.contains(&format!("processcommand::new(&{alias})"))
    })
}

fn starts_multiline_command_new(line: &str) -> bool {
    let compact = line
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase();
    (compact.contains("command::new(") || compact.contains("processcommand::new("))
        && !compact.contains(')')
}

fn line_mentions_git_command_arg(line: &str, aliases: &BTreeSet<String>) -> bool {
    let compact = line
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase();
    compact.contains("\"git\"")
        || compact.contains("r#\"git\"#")
        || aliases.iter().any(|alias| {
            compact == *alias
                || compact == format!("{alias},")
                || compact == format!("&{alias}")
                || compact == format!("&{alias},")
        })
}

fn parse_git_command_alias(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = line.strip_prefix("let ")?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim().trim_start_matches("mut ").trim();
    if !is_rust_identifier(name) {
        return None;
    }
    let value = value.trim().trim_end_matches(';').trim();
    matches!(value, "\"git\"" | "r#\"git\"#").then_some(name)
}

fn is_rust_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn shell_wrapper_mentions_git(line: &str) -> bool {
    let shell_spawn = line.contains("command::new(\"sh\")")
        || line.contains("command::new(\"bash\")")
        || line.contains("command::new(\"cmd\")")
        || line.contains("command::new(\"powershell\")")
        || line.contains("command::new(\"pwsh\")");
    shell_spawn && line.contains("git")
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

fn brace_depth_after_line(mut depth: usize, line: &str) -> usize {
    for ch in line.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
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

fn default_cli_runtime_source_dirs(workspace: &Path) -> Vec<PathBuf> {
    [
        "crates/cli/src",
        "crates/cli-shared/src",
        "crates/client/src",
        // Facade and Git-projection engine must stay free of runtime `git`
        // process spawns (program gap C4 / product contract).
        "crates/core/src",
        "crates/crypto/src",
        "crates/daemon/src",
        "crates/git-projection/src",
        "crates/grpc/src",
        "crates/ingest/src",
        "crates/merge/src",
        "crates/mount/src",
        "crates/objects/src",
        "crates/oplog/src",
        "crates/wire/src",
        "crates/refs/src",
        "crates/repo/src",
        "crates/runtime-bridge/src",
        "crates/schema/src",
        "crates/semantic/src",
        "crates/state_review/src",
        "crates/weft-client-shim/src",
    ]
    .into_iter()
    .map(|path| workspace.join(path))
    .filter(|path| path.exists())
    .collect()
}

#[test]
fn git_spawn_detector_catches_aliases_and_shell_wrappers() {
    for line in [
        "Command::new(\"git\")",
        "std::process::Command::new(\"git\")",
        "tokio::process::Command::new(\"git\")",
        "ProcessCommand::new(\"git\")",
        "Command::new(GIT_BINARY)",
        "Command::new(&git_path)",
        "Command::new(\"sh\").arg(\"-c\").arg(\"git status\")",
        "Command::new(\"bash\").args([\"-c\", \"git fetch\"])",
    ] {
        assert!(is_git_spawn(line), "should flag {line:?}");
    }

    for line in [
        "Command::new(\"heddle\")",
        "Command::new(\"xdg-open\")",
        "Command::new(\"cmd\").args([\"/C\", \"start\", url])",
        "let git = gix::open(path)?;",
    ] {
        assert!(!is_git_spawn(line), "should not flag {line:?}");
    }
}

#[test]
fn git_spawn_detector_catches_multiline_and_local_aliases() {
    let mut aliases = BTreeSet::new();
    aliases.insert("git_cmd".to_string());
    assert!(is_git_spawn_with_aliases(
        "Command::new(git_cmd).arg(\"status\")",
        &aliases
    ));
    assert!(is_git_spawn_with_aliases(
        "std::process::Command::new(&git_cmd)",
        &aliases
    ));

    assert!(starts_multiline_command_new("Command::new("));
    assert!(line_mentions_git_command_arg(
        "    \"git\"",
        &BTreeSet::new()
    ));
    assert_eq!(
        parse_git_command_alias("let git_cmd = \"git\";"),
        Some("git_cmd")
    );
    assert_eq!(
        parse_git_command_alias("let mut git_cmd = r#\"git\"#;"),
        Some("git_cmd")
    );
    assert_eq!(parse_git_command_alias("let git = gix::open(path)?;"), None);
}
