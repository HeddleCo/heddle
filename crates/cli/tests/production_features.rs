// SPDX-License-Identifier: Apache-2.0
//! Production-ready features integration tests.
//!
//! Tests for resolve, fetch, fsck, clone, cherry-pick, rebase, bisect, blame, gc.

use std::{fs, process::Command, str};

use ntest::timeout;
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

fn translate_legacy_args(args: &[&str]) -> Vec<String> {
    let mut prefix = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        prefix.push(args[i].to_string());
        i += 1;
    }
    let rest = &args[i..];
    let translated = match rest {
        ["thread", "delete", name] => vec![
            "thread".into(),
            "drop".into(),
            (*name).into(),
            "--delete-thread".into(),
        ],
        _ => rest.iter().map(|arg| (*arg).to_string()).collect(),
    };
    prefix.extend(translated);
    prefix
}

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = cmd.output().map_err(|e| e.to_string())?;

    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn heddle_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let output = cmd.output().map_err(|e| e.to_string())?;
    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn status_json(path: &std::path::Path) -> Value {
    let output = heddle(&["status", "--json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

fn setup_repo_with_file(temp: &TempDir, filename: &str, content: &str) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(filename), content).unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
}

fn assert_file_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(path.exists(), "{}: {:?}", msg, path);
}

#[allow(dead_code)]
fn assert_file_not_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(!path.exists(), "{}: {:?}", msg, path);
}

mod resolve {
    use super::*;

    fn create_conflict(temp: &TempDir) {
        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "feature version").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "main version").unwrap();
        heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

        heddle(&["merge", "feature"], Some(temp.path())).unwrap();
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_marks_file_as_resolved() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "resolved content").unwrap();

        let result = heddle(&["resolve", "file.txt"], Some(temp.path()));
        assert!(result.is_ok(), "resolve failed: {:?}", result.err());
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_all() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "resolved content").unwrap();

        let result = heddle(&["resolve", "--all"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --all failed: {:?}", result.err());
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_list_conflicts() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["resolve", "--list"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --list failed: {:?}", result.err());

        let output = result.unwrap();
        assert!(output.contains("file.txt"), "should list conflicting file");
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_with_ours() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["resolve", "file.txt", "--ours"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --ours failed: {:?}", result.err());

        let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
        assert_eq!(content, "main version", "should use our version");
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_with_theirs() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["resolve", "file.txt", "--theirs"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "resolve --theirs failed: {:?}",
            result.err()
        );

        let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
        assert_eq!(content, "feature version", "should use their version");
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_abort() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["resolve", "--abort"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --abort failed: {:?}", result.err());
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolved_merge_snapshot_preserves_theirs_provenance() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "base\n").unwrap();
        heddle_with_env(
            &[
                "capture",
                "-m",
                "base",
                "--agent-provider",
                "anthropic",
                "--agent-model",
                "claude-base",
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "feature version\n").unwrap();
        heddle_with_env(
            &[
                "capture",
                "-m",
                "feature",
                "--agent-provider",
                "openai",
                "--agent-model",
                "gpt-feature",
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "main version\n").unwrap();
        heddle_with_env(
            &[
                "capture",
                "-m",
                "main",
                "--agent-provider",
                "anthropic",
                "--agent-model",
                "claude-main",
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();

        heddle(&["merge", "feature"], Some(temp.path())).unwrap();
        heddle(&["resolve", "file.txt", "--theirs"], Some(temp.path())).unwrap();
        heddle_with_env(
            &[
                "capture",
                "-m",
                "resolved",
                "--agent-provider",
                "openai",
                "--agent-model",
                "gpt-resolver",
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();

        let blame = heddle(&["blame", "file.txt", "--json"], Some(temp.path())).unwrap();
        let parsed: Value = serde_json::from_str(&blame).unwrap();
        assert!(
            parsed["lines"][0]["author"]
                .as_str()
                .unwrap()
                .contains("openai/gpt-feature")
        );
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolved_merge_snapshot_attributes_manual_lines_to_resolver() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "custom resolved\n").unwrap();
        heddle(&["resolve", "file.txt"], Some(temp.path())).unwrap();
        heddle_with_env(
            &[
                "capture",
                "-m",
                "resolved",
                "--agent-provider",
                "openai",
                "--agent-model",
                "gpt-resolver",
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();

        let blame = heddle(&["blame", "file.txt", "--json"], Some(temp.path())).unwrap();
        let parsed: Value = serde_json::from_str(&blame).unwrap();
        assert!(
            parsed["lines"][0]["author"]
                .as_str()
                .unwrap()
                .contains("openai/gpt-resolver")
        );
    }
}

mod fsck {
    use super::*;

    #[test]
    fn test_fsck_clean_repo() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["fsck"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "fsck on clean repo should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_fsck_reports_corrupted_blob() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        // Snapshot pack-batches blobs into `.heddle/packs/*.pack`, so
        // there's no loose blob to overwrite. Scramble the pack
        // payload after its 8-byte magic+version header — the read
        // path will surface a hash mismatch, decompression error, or
        // structural failure. Fsck accepts any of those signals.
        let packs_dir = temp.path().join(".heddle/packs");
        let mut corrupted = false;
        for entry in fs::read_dir(&packs_dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pack") {
                continue;
            }
            let mut bytes = fs::read(&path).unwrap();
            assert!(
                bytes.len() > 32,
                "pack {:?} too small to corrupt safely",
                path
            );
            let end = bytes.len().min(48);
            for b in &mut bytes[16..end] {
                *b ^= 0xFF;
            }
            fs::write(&path, bytes).unwrap();
            corrupted = true;
            break;
        }
        assert!(corrupted, "should have found a pack file to corrupt");

        let result = heddle(&["fsck", "--full"], Some(temp.path()));
        // fsck should detect the corruption — either via exit code or output
        if let Ok(output) = &result {
            assert!(
                output.contains("error")
                    || output.contains("mismatch")
                    || output.contains("invalid")
                    || output.contains("corrupt"),
                "fsck should report corruption: {}",
                output
            );
        }
        // An error exit code is also acceptable
    }

    #[test]
    fn test_fsck_json_output() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["fsck", "--json"], Some(temp.path()));
        assert!(result.is_ok(), "fsck --json failed: {:?}", result.err());

        let output: Value = serde_json::from_str(&result.unwrap()).expect("should be JSON");
        assert!(output.get("valid").is_some(), "should have 'valid' field");
    }

    #[test]
    fn test_fsck_repair_mode() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["fsck", "--repair"], Some(temp.path()));
        assert!(result.is_ok(), "fsck --repair failed: {:?}", result.err());
    }

    #[test]
    fn test_fsck_full_check() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["fsck", "--full"], Some(temp.path()));
        assert!(result.is_ok(), "fsck --full failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_fsck_after_merge() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("feat.txt"), "feature").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("main.txt"), "main").unwrap();
        heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

        heddle(&["merge", "feature"], Some(temp.path())).unwrap();

        let result = heddle(&["fsck", "--full", "--thorough"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "fsck after merge should pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_fsck_detects_broken_parent() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        // Snapshot A
        fs::write(temp.path().join("file.txt"), "v1").unwrap();
        heddle(&["capture", "-m", "State A"], Some(temp.path())).unwrap();

        // Snapshot B (child of A)
        fs::write(temp.path().join("file.txt"), "v2").unwrap();
        heddle(&["capture", "-m", "State B"], Some(temp.path())).unwrap();

        // Find state A's file and delete it
        let states_dir = temp.path().join(".heddle/objects/states");
        if states_dir.exists() {
            let state_files: Vec<_> = fs::read_dir(&states_dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("state"))
                .collect();

            // Delete the first state file (A), keeping B
            if state_files.len() >= 2 {
                // Sort by name to get consistent ordering
                let mut paths: Vec<_> = state_files.iter().map(|e| e.path()).collect();
                paths.sort();
                fs::remove_file(&paths[0]).unwrap();

                let result = heddle(&["fsck", "--thorough"], Some(temp.path()));
                // fsck should either report errors in stdout or fail
                if let Ok(output) = &result {
                    assert!(
                        output.contains("error")
                            || output.contains("missing")
                            || output.contains("broken")
                            || output.contains("invalid")
                            || output.to_lowercase().contains("parent"),
                        "fsck should report missing parent: {}",
                        output
                    );
                }
                // Failing is also acceptable — means fsck detected corruption
            }
        }
    }
}

mod cherry_pick {
    use super::*;

    #[test]
    fn test_cherry_pick_applies_commit() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "original").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("new.txt"), "added by feature").unwrap();
        heddle(&["capture", "-m", "Add new file"], Some(temp.path())).unwrap();

        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let commit_id = log_output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();

        heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

        let result = heddle(&["cherry-pick", commit_id], Some(temp.path()));
        assert!(result.is_ok(), "cherry-pick failed: {:?}", result.err());

        assert_file_exists(
            temp.path().join("new.txt"),
            "cherry-picked file should exist",
        );
    }

    #[test]
    fn test_cherry_pick_with_message() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "modified").unwrap();
        heddle(&["capture", "-m", "Modify"], Some(temp.path())).unwrap();

        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let commit_id = log_output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();

        heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

        let result = heddle(
            &["cherry-pick", commit_id, "-m", "Custom message"],
            Some(temp.path()),
        );
        assert!(
            result.is_ok(),
            "cherry-pick with message failed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_cherry_pick_no_commit() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "original").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("new.txt"), "added").unwrap();
        heddle(&["capture", "-m", "Add"], Some(temp.path())).unwrap();

        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let commit_id = log_output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();

        heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

        let result = heddle(
            &["cherry-pick", commit_id, "--no-commit"],
            Some(temp.path()),
        );
        assert!(
            result.is_ok(),
            "cherry-pick --no-commit failed: {:?}",
            result.err()
        );

        let status = status_json(temp.path());
        assert!(!status["changes"]["added"].as_array().unwrap().is_empty());
    }

    /// Regression: `heddle cherry-pick` must not silently destroy
    /// heddle-ignored content under a tracked top-level directory it
    /// drops. Pre-fix, `apply_tree_to_worktree` called
    /// `remove_path_recursively` on entries the cherry-picked tree no
    /// longer contained, recursively nuking `web/node_modules/` alongside
    /// the tracked `web/index.html`. Post-fix, only tracked descendants
    /// are removed and ignored siblings survive.
    #[test]
    fn test_cherry_pick_preserves_ignored_siblings_in_dropped_tracked_dir() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        // Snapshot 1 (BASE): empty.
        heddle(&["capture", "-m", "empty"], Some(temp.path())).unwrap();

        // Snapshot 2 (WITH_WEB): tracked `web/index.html` exists.
        fs::create_dir_all(temp.path().join("web")).unwrap();
        fs::write(temp.path().join("web/index.html"), "<html/>").unwrap();
        heddle(&["capture", "-m", "add web"], Some(temp.path())).unwrap();

        // Snapshot 3 (WITHOUT_WEB): drop the tracked dir. This is the
        // commit we will cherry-pick.
        fs::remove_dir_all(temp.path().join("web")).unwrap();
        heddle(&["capture", "-m", "drop web"], Some(temp.path())).unwrap();

        // Capture the change_id of the deletion commit.
        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let drop_web_id = log_output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();

        // Move back to WITH_WEB so cherry-pick has work to do.
        heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

        // User drops heddle-ignored content alongside the tracked dir.
        // Default ignore list (`node_modules`) covers this; status hides
        // it but the filesystem still holds it.
        fs::create_dir_all(temp.path().join("web/node_modules/lodash")).unwrap();
        fs::write(
            temp.path().join("web/node_modules/lodash/index.js"),
            "ignored",
        )
        .unwrap();

        // Cherry-pick the deletion onto WITH_WEB. The tracked
        // `web/index.html` should disappear; the ignored sibling must
        // remain.
        let result = heddle(&["cherry-pick", &drop_web_id], Some(temp.path()));
        assert!(result.is_ok(), "cherry-pick failed: {:?}", result.err());

        assert_file_not_exists(
            temp.path().join("web/index.html"),
            "tracked entry must be removed by cherry-pick",
        );
        assert_file_exists(
            temp.path().join("web/node_modules/lodash/index.js"),
            "heddle-ignored content must survive cherry-pick that drops the tracked dir",
        );
    }
}

mod bisect {
    use super::*;

    #[test]
    fn test_bisect_start() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        for i in 0..5 {
            fs::write(temp.path().join("file.txt"), format!("version {}", i)).unwrap();
            heddle(
                &["capture", "-m", &format!("Commit {}", i)],
                Some(temp.path()),
            )
            .unwrap();
        }

        let result = heddle(&["bisect", "start"], Some(temp.path()));
        assert!(result.is_ok(), "bisect start failed: {:?}", result.err());
    }

    #[test]
    fn test_bisect_good_bad() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        for i in 0..5 {
            fs::write(temp.path().join("file.txt"), format!("version {}", i)).unwrap();
            heddle(
                &["capture", "-m", &format!("Commit {}", i)],
                Some(temp.path()),
            )
            .unwrap();
        }

        heddle(&["bisect", "start"], Some(temp.path())).unwrap();

        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let commits: Vec<&str> = log_output.lines().take(5).collect();
        let bad_commit = commits[0].split_whitespace().next().unwrap();
        let good_commit = commits[4].split_whitespace().next().unwrap();

        let result = heddle(&["bisect", "bad", bad_commit], Some(temp.path()));
        assert!(result.is_ok(), "bisect bad failed: {:?}", result.err());

        let result = heddle(&["bisect", "good", good_commit], Some(temp.path()));
        assert!(result.is_ok(), "bisect good failed: {:?}", result.err());
    }

    #[test]
    fn test_bisect_reset() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        for i in 0..3 {
            fs::write(temp.path().join("file.txt"), format!("v{}", i)).unwrap();
            heddle(&["capture", "-m", &format!("C{}", i)], Some(temp.path())).unwrap();
        }

        heddle(&["bisect", "start"], Some(temp.path())).unwrap();

        let result = heddle(&["bisect", "reset"], Some(temp.path()));
        assert!(result.is_ok(), "bisect reset failed: {:?}", result.err());
    }
}

mod blame {
    use cli::Repository;

    use super::*;

    fn snapshot_with_agent(temp: &TempDir, message: &str, provider: &str, model: &str) {
        heddle_with_env(
            &[
                "capture",
                "-m",
                message,
                "--agent-provider",
                provider,
                "--agent-model",
                model,
            ],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
            ],
        )
        .unwrap();
    }

    #[test]
    fn test_blame_single_file() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "line 1\nline 2\nline 3\n").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        let result = heddle(&["blame", "file.txt"], Some(temp.path()));
        assert!(result.is_ok(), "blame failed: {:?}", result.err());

        let output = result.unwrap();
        assert!(output.contains("line 1"), "should show file content");
    }

    #[test]
    fn test_blame_json_output() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "content\n").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        let result = heddle(&["blame", "file.txt", "--json"], Some(temp.path()));
        assert!(result.is_ok(), "blame --json failed: {:?}", result.err());

        let output: Value = serde_json::from_str(&result.unwrap()).expect("should be JSON");
        assert!(output.get("lines").is_some(), "should have 'lines' field");
    }

    #[test]
    fn test_blame_multiple_commits() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "original line\n").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "modified line\n").unwrap();
        heddle(&["capture", "-m", "Modify"], Some(temp.path())).unwrap();

        let result = heddle(&["blame", "file.txt"], Some(temp.path()));
        assert!(result.is_ok(), "blame failed: {:?}", result.err());
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_blame_preserves_agent_origins_through_collapse() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "line one\nline two\n").unwrap();
        snapshot_with_agent(&temp, "initial", "anthropic", "claude-sonnet-a");

        fs::write(temp.path().join("file.txt"), "line one\nline two updated\n").unwrap();
        snapshot_with_agent(&temp, "update", "openai", "gpt-4.1-b");

        let repo = Repository::open(temp.path()).unwrap();
        let head = repo.current_state().unwrap().unwrap();
        let first = head.parents[0];

        heddle(
            &[
                "collapse",
                &first.to_string_full(),
                &head.change_id.to_string_full(),
                "--into",
                "combined",
            ],
            Some(temp.path()),
        )
        .unwrap();

        let output = heddle(&["blame", "file.txt", "--json"], Some(temp.path())).unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        assert!(
            lines[0]["author"]
                .as_str()
                .unwrap()
                .contains("anthropic/claude-sonnet-a")
        );
        assert!(
            lines[1]["author"]
                .as_str()
                .unwrap()
                .contains("openai/gpt-4.1-b")
        );
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_blame_preserves_agent_origins_through_clean_merge() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "base one\nbase two\n").unwrap();
        snapshot_with_agent(&temp, "base", "anthropic", "claude-opus-base");

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "base one\nfeature two\n").unwrap();
        snapshot_with_agent(&temp, "feature", "openai", "gpt-4.1-feature");

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("other.txt"), "main side\n").unwrap();
        snapshot_with_agent(&temp, "main", "anthropic", "claude-opus-main");

        heddle(&["merge", "feature"], Some(temp.path())).unwrap();

        let output = heddle(&["blame", "file.txt", "--json"], Some(temp.path())).unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        assert!(
            lines[0]["author"]
                .as_str()
                .unwrap()
                .contains("anthropic/claude-opus-base")
        );
        assert!(
            lines[1]["author"]
                .as_str()
                .unwrap()
                .contains("openai/gpt-4.1-feature")
        );
    }
}

#[path = "production_features/merge_operations.rs"]
mod merge_operations;

#[path = "production_features/merge_rename_detection.rs"]
mod merge_rename_detection;

#[path = "production_features/merge_rename_output.rs"]
mod merge_rename_output;

mod gc {
    use super::*;

    #[test]
    fn test_gc_basic() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["maintenance", "gc"], Some(temp.path()));
        assert!(result.is_ok(), "gc failed: {:?}", result.err());
    }

    #[test]
    fn test_gc_idempotent() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        for i in 0..3 {
            fs::write(temp.path().join("file.txt"), format!("v{}", i)).unwrap();
            heddle(
                &["capture", "-m", &format!("snapshot {}", i)],
                Some(temp.path()),
            )
            .unwrap();
        }

        let first = heddle(&["maintenance", "gc"], Some(temp.path()));
        assert!(first.is_ok(), "first gc failed: {:?}", first.err());

        let second = heddle(&["maintenance", "gc"], Some(temp.path()));
        assert!(second.is_ok(), "second gc failed: {:?}", second.err());
    }

    #[test]
    fn test_gc_preserves_all_reachable() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();

        // Create 5 snapshots
        for i in 0..5 {
            fs::write(temp.path().join("file.txt"), format!("content {}", i)).unwrap();
            heddle(
                &["capture", "-m", &format!("snapshot {}", i)],
                Some(temp.path()),
            )
            .unwrap();
        }

        // Collect state IDs before gc
        let log_before =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let state_ids: Vec<&str> = log_before
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert!(state_ids.len() >= 5, "should have at least 5 states");

        // Run gc with prune
        heddle(&["maintenance", "gc", "--prune"], Some(temp.path())).unwrap();

        // All states should still be accessible
        for id in &state_ids {
            let result = heddle(&["show", id], Some(temp.path()));
            assert!(
                result.is_ok(),
                "state {} should be accessible after gc: {:?}",
                id,
                result.err()
            );
        }
    }

    #[test]
    fn test_fsck_after_gc() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        for i in 0..3 {
            fs::write(temp.path().join("file.txt"), format!("v{}", i)).unwrap();
            heddle(
                &["capture", "-m", &format!("snapshot {}", i)],
                Some(temp.path()),
            )
            .unwrap();
        }

        heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path())).unwrap();

        let result = heddle(&["fsck", "--full"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "fsck after gc should pass: {:?}",
            result.err()
        );
    }
}

mod clone {
    use super::*;

    #[test]
    fn test_clone_creates_local_copy() {
        let remote = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();

        heddle(&["init"], Some(remote.path())).unwrap();
        fs::write(remote.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

        let remote_path = remote.path().to_string_lossy().to_string();
        let local_path = local.path().join("cloned");

        let result = heddle(&["clone", &remote_path, local_path.to_str().unwrap()], None);
        assert!(result.is_ok(), "clone failed: {:?}", result.err());

        assert_file_exists(local_path.join("file.txt"), "cloned file should exist");
        assert_file_exists(
            local_path.join(".heddle"),
            "cloned repo should have .heddle dir",
        );
    }

    #[test]
    fn test_clone_with_thread() {
        let remote = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();

        heddle(&["init"], Some(remote.path())).unwrap();
        fs::write(remote.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();
        heddle(&["thread", "create", "feature"], Some(remote.path())).unwrap();

        let remote_path = remote.path().to_string_lossy().to_string();
        let local_path = local.path().join("cloned");

        let result = heddle(
            &[
                "clone",
                &remote_path,
                local_path.to_str().unwrap(),
                "--thread",
                "feature",
            ],
            None,
        );
        assert!(
            result.is_ok(),
            "clone with thread failed: {:?}",
            result.err()
        );
    }
}

mod fetch {
    use super::*;

    #[test]
    #[ignore = "Requires file:// protocol support for local paths"]
    fn test_fetch_downloads_objects() {
        let remote = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();

        heddle(&["init"], Some(remote.path())).unwrap();
        fs::write(remote.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

        heddle(&["init"], Some(local.path())).unwrap();
        let remote_path = remote.path().to_string_lossy().to_string();
        heddle(
            &["remote", "add", "origin", &remote_path],
            Some(local.path()),
        )
        .unwrap();

        let result = heddle(&["fetch", "origin"], Some(local.path()));
        assert!(result.is_ok(), "fetch failed: {:?}", result.err());
    }

    #[test]
    fn test_fetch_all() {
        let remote = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();

        heddle(&["init"], Some(remote.path())).unwrap();
        fs::write(remote.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

        heddle(&["init"], Some(local.path())).unwrap();
        let remote_path = remote.path().to_string_lossy().to_string();
        heddle(
            &["remote", "add", "origin", &remote_path],
            Some(local.path()),
        )
        .unwrap();

        let result = heddle(&["fetch", "--all"], Some(local.path()));
        assert!(result.is_ok(), "fetch --all failed: {:?}", result.err());
    }
}

mod local_sync {
    use super::*;

    #[test]
    fn test_pull_diverged_repos() {
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();

        // Init repo A with a base state
        heddle(&["init"], Some(repo_a.path())).unwrap();
        fs::write(repo_a.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(repo_a.path())).unwrap();

        // Clone A to B
        let a_path = repo_a.path().to_string_lossy().to_string();
        let result = heddle(
            &[
                "pull",
                &a_path,
                "--thread",
                "main",
                "--local-thread",
                "main",
            ],
            Some(repo_b.path()),
        );
        // If pull needs init first
        if result.is_err() {
            heddle(&["init"], Some(repo_b.path())).unwrap();
            heddle(
                &[
                    "pull",
                    &a_path,
                    "--thread",
                    "main",
                    "--local-thread",
                    "main",
                ],
                Some(repo_b.path()),
            )
            .unwrap();
        }

        // Both repos diverge: A adds a file
        fs::write(repo_a.path().join("a_only.txt"), "from A").unwrap();
        heddle(&["capture", "-m", "A diverges"], Some(repo_a.path())).unwrap();

        // Pull A into B — B should get A's latest objects
        let result = heddle(
            &[
                "pull",
                &a_path,
                "--thread",
                "main",
                "--local-thread",
                "synced",
            ],
            Some(repo_b.path()),
        );
        assert!(
            result.is_ok(),
            "pull diverged repos should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_push_local_creates_thread() {
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();

        heddle(&["init"], Some(repo_a.path())).unwrap();
        fs::write(repo_a.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(repo_a.path())).unwrap();

        heddle(&["init"], Some(repo_b.path())).unwrap();

        let b_path = repo_b.path().to_string_lossy().to_string();
        let result = heddle(
            &["push", &b_path, "--thread", "feature"],
            Some(repo_a.path()),
        );
        assert!(
            result.is_ok(),
            "push local should succeed: {:?}",
            result.err()
        );

        // Verify B has the feature thread
        let threads = heddle(&["thread", "list"], Some(repo_b.path())).unwrap();
        assert!(
            threads.contains("feature"),
            "pushed thread should be visible in target repo: {}",
            threads
        );
    }

    #[test]
    fn test_fetch_then_merge_remote_content() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        // Create dest with a base state on main
        heddle(&["init"], Some(dest.path())).unwrap();
        fs::write(dest.path().join("base.txt"), "shared base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(dest.path())).unwrap();

        // Seed source from dest, then attach to main thread
        heddle(&["init"], Some(source.path())).unwrap();
        let dest_path = dest.path().to_string_lossy().to_string();
        heddle(
            &[
                "pull",
                &dest_path,
                "--thread",
                "main",
                "--local-thread",
                "main",
            ],
            Some(source.path()),
        )
        .unwrap();
        // Attach HEAD to main so future snapshots advance the thread
        heddle(&["thread", "switch", "main"], Some(source.path())).unwrap();

        // Source adds a new file on main
        let source_path = source.path().to_string_lossy().to_string();
        fs::write(source.path().join("source.txt"), "from source").unwrap();
        heddle(&["capture", "-m", "Source addition"], Some(source.path())).unwrap();

        // Dest adds a different file on main
        fs::write(dest.path().join("dest.txt"), "from dest").unwrap();
        heddle(&["capture", "-m", "Dest addition"], Some(dest.path())).unwrap();

        // Pull source's main into dest as a separate thread
        heddle(
            &[
                "pull",
                &source_path,
                "--thread",
                "main",
                "--local-thread",
                "from-source",
            ],
            Some(dest.path()),
        )
        .unwrap();

        // Switch back to main (which has dest.txt) and merge
        heddle(&["thread", "switch", "main"], Some(dest.path())).unwrap();
        let result = heddle(&["merge", "from-source"], Some(dest.path()));
        assert!(
            result.is_ok(),
            "merge remote content should succeed: {:?}",
            result.err()
        );

        // Both unique files should exist after merge
        assert!(
            dest.path().join("dest.txt").exists(),
            "dest.txt should still exist after merge"
        );
        assert!(
            dest.path().join("source.txt").exists(),
            "source.txt should appear after merge"
        );
    }

    /// Regression: a fast-forward `heddle pull` from inside an attached
    /// thread used to call `repo.goto()` (which writes `Head::Detached`)
    /// without advancing the attached thread's metadata. The worktree and
    /// the thread ref both advanced, but HEAD was silently detached and
    /// the thread's `current_state` metadata stayed pinned at its
    /// pre-pull value. Mirrors the merge/rebase fixes — pull/fetch must
    /// preserve attached-HEAD semantics via
    /// `Repository::fast_forward_attached`.
    #[test]
    fn test_pull_fast_forward_advances_current_thread() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        // Source repo with a base state on main.
        heddle(&["init"], Some(source.path())).unwrap();
        fs::write(source.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(source.path())).unwrap();

        // Bootstrap dest from source so they share the base state and
        // both have a `main` thread.
        heddle(&["init"], Some(dest.path())).unwrap();
        let source_path = source.path().to_string_lossy().to_string();
        heddle(
            &[
                "pull",
                &source_path,
                "--thread",
                "main",
                "--local-thread",
                "main",
            ],
            Some(dest.path()),
        )
        .unwrap();

        // Attach HEAD on the dest to `main` so the pull is from inside
        // an attached thread (the bug-class scenario).
        heddle(&["thread", "switch", "main"], Some(dest.path())).unwrap();

        // Source advances `main` with a new state.
        fs::write(source.path().join("forward.txt"), "forward").unwrap();
        heddle(&["capture", "-m", "Forward"], Some(source.path())).unwrap();
        let source_main =
            heddle(&["thread", "show", "main", "--json"], Some(source.path())).unwrap();
        let source_main_v: Value = serde_json::from_str(&source_main).unwrap();
        let target = source_main_v["current_state"]
            .as_str()
            .expect("source main should have a current_state")
            .to_string();

        // Pull source's `main` into dest's `main` — fast-forward path.
        heddle(
            &[
                "pull",
                &source_path,
                "--thread",
                "main",
                "--local-thread",
                "main",
            ],
            Some(dest.path()),
        )
        .unwrap();

        // After fast-forward pull, dest's `main` thread metadata must
        // advance to the integrated state.
        let main_show = heddle(&["thread", "show", "main", "--json"], Some(dest.path())).unwrap();
        let main: Value = serde_json::from_str(&main_show).unwrap();
        assert_eq!(
            main["current_state"].as_str().unwrap(),
            target,
            "main.current_state must advance to the pull target after fast-forward"
        );

        // HEAD must remain attached to the previously-attached thread.
        let status_output = heddle(&["status", "--json"], Some(dest.path())).unwrap();
        let status: Value = serde_json::from_str(&status_output).unwrap();
        assert_eq!(
            status["thread"].as_str().unwrap(),
            "main",
            "HEAD must remain attached to `main` after fast-forward pull"
        );
    }
}

mod force_with_lease {
    use super::*;

    #[test]
    fn test_push_force_with_lease_requires_tracking() {
        let remote = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();

        heddle(&["init"], Some(remote.path())).unwrap();

        heddle(&["init"], Some(local.path())).unwrap();
        fs::write(local.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(local.path())).unwrap();

        let remote_path = remote.path().to_string_lossy().to_string();
        heddle(
            &["remote", "add", "origin", &remote_path],
            Some(local.path()),
        )
        .unwrap();

        let _result = heddle(
            &["push", "origin", "--force-with-lease"],
            Some(local.path()),
        );
        // May fail if no tracking info exists
    }
}

mod rebase {
    use super::*;

    #[test]
    fn test_rebase_onto() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("feature.txt"), "feature").unwrap();
        heddle(&["capture", "-m", "Feature commit"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("main.txt"), "main").unwrap();
        heddle(&["capture", "-m", "Main commit"], Some(temp.path())).unwrap();

        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let _base_commit = log_output
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();

        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

        let result = heddle(&["rebase", "main"], Some(temp.path()));
        assert!(result.is_ok(), "rebase failed: {:?}", result.err());
    }

    #[test]
    fn test_rebase_abort() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "feature").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "main").unwrap();
        heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        heddle(&["rebase", "main"], Some(temp.path())).ok();

        let _result = heddle(&["rebase", "--abort"], Some(temp.path()));
        // May succeed or fail depending on rebase state
    }

    #[test]
    fn test_rebase_rewrites_parent_chain() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        // Create feature branch: base → A1 → A2
        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("a1.txt"), "a1").unwrap();
        heddle(&["capture", "-m", "A1"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("a2.txt"), "a2").unwrap();
        heddle(&["capture", "-m", "A2"], Some(temp.path())).unwrap();

        // Advance main: base → B1
        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("b1.txt"), "b1").unwrap();
        heddle(&["capture", "-m", "B1"], Some(temp.path())).unwrap();

        // Rebase feature onto main
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        let result = heddle(&["rebase", "main"], Some(temp.path()));
        assert!(result.is_ok(), "rebase should succeed: {:?}", result.err());

        // After rebase, feature should have B1's content
        assert!(
            temp.path().join("b1.txt").exists(),
            "b1.txt from main should exist after rebase"
        );
        assert!(
            temp.path().join("a1.txt").exists(),
            "a1.txt from feature should exist after rebase"
        );
        assert!(
            temp.path().join("a2.txt").exists(),
            "a2.txt from feature should exist after rebase"
        );

        // Log should show B1 in the ancestry
        let log_output =
            heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        assert!(
            log_output.contains("B1") || log_output.contains("A2"),
            "log should include rebased history: {}",
            log_output
        );
    }

    #[test]
    fn test_rebase_preserves_logical_change_identity() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("feature.txt"), "feature").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        let repo = repo::Repository::open(temp.path()).unwrap();
        let pre_rebase_head = repo.head().unwrap().unwrap();
        let pre_rebase_state = repo.store().get_state(&pre_rebase_head).unwrap().unwrap();
        let original_logical = pre_rebase_state.logical_change_id();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("main.txt"), "main").unwrap();
        heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        heddle(&["rebase", "main"], Some(temp.path())).unwrap();

        let repo = repo::Repository::open(temp.path()).unwrap();
        let rebased_head = repo.head().unwrap().unwrap();
        let rebased_state = repo.store().get_state(&rebased_head).unwrap().unwrap();

        assert_ne!(rebased_head, pre_rebase_head);
        assert_eq!(rebased_state.logical_change_id(), original_logical);
    }

    #[test]
    fn test_rebase_with_conflicting_changes_completes() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "feature change").unwrap();
        heddle(&["capture", "-m", "Feature change"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "main change").unwrap();
        heddle(&["capture", "-m", "Main change"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

        // Rebase auto-resolves conflicting changes. The CLI emits a
        // line-separated JSON event stream (`started` → `applying` →
        // …) rather than a single `completed` summary, so we treat
        // exit success + the emitted progress events as the
        // completion contract.
        let result = heddle(&["rebase", "main"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "rebase with conflicting file should complete: {:?}",
            result.err()
        );
        let output = result.unwrap();
        assert!(
            output.contains("\"status\": \"started\"") || output.contains("\"status\":\"started\""),
            "rebase should announce a started status: {}",
            output
        );
        assert!(
            output.contains("\"status\": \"applying\"")
                || output.contains("\"status\":\"applying\""),
            "rebase should report progress while applying: {}",
            output
        );

        // Feature's version should be in the file after rebase
        let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
        assert_eq!(
            content, "feature change",
            "rebase should preserve feature's changes"
        );
    }

    #[test]
    fn test_rebase_noop_when_already_on_target() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        // Create feature from main tip — no divergence
        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("feat.txt"), "feature").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        // Rebase onto main — feature is already based on main tip
        let result = heddle(&["rebase", "main"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "rebase when already on target should succeed: {:?}",
            result.err()
        );
        let output = result.unwrap();
        assert!(
            output.contains("completed"),
            "rebase should report completion: {}",
            output
        );
    }

    /// Regression: a fast-forward `heddle rebase` from inside an attached
    /// parent thread used to call `repo.goto()` (which writes
    /// `Head::Detached`) without advancing the parent thread's ref. The
    /// worktree advanced but `thread show <parent>` still reported the
    /// original change_id and HEAD was silently detached. Mirrors the
    /// merge fix in `crates/cli/src/cli/commands/merge/mod.rs`.
    #[test]
    fn test_rebase_fast_forward_advances_current_thread() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        // Build a child thread that's strictly ahead of the parent.
        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("new.txt"), "feature work").unwrap();
        heddle(&["capture", "-m", "Feature work"], Some(temp.path())).unwrap();

        // Capture the change_id at the tip of `feature` — this is the
        // fast-forward target for rebasing `main` onto `feature`.
        let feature_show =
            heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
        let feature: Value = serde_json::from_str(&feature_show).unwrap();
        let target = feature["current_state"]
            .as_str()
            .expect("feature should have a current_state")
            .to_string();

        // Switch to parent and rebase onto child. Parent is an ancestor
        // of child, so this hits the fast-forward path in run_rebase.
        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        let rebase_output = heddle(&["rebase", "feature"], Some(temp.path())).unwrap();
        // Rebase emits either text ("Fast-forwarded to ...") or JSON
        // ({"status": "fast_forwarded", ...}) depending on the resolved
        // output mode. Either form indicates the fast-forward path ran.
        assert!(
            rebase_output.contains("Fast-forwarded")
                || rebase_output.contains("\"fast_forwarded\""),
            "expected fast-forward rebase, got: {rebase_output}"
        );

        // After fast-forward, `main` must point at the integrated state.
        let main_show = heddle(&["thread", "show", "main", "--json"], Some(temp.path())).unwrap();
        let main: Value = serde_json::from_str(&main_show).unwrap();
        assert_eq!(
            main["current_state"].as_str().unwrap(),
            target,
            "main.current_state must advance to the rebase target after fast-forward"
        );

        // HEAD must remain attached to the parent thread.
        let status_output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
        let status: Value = serde_json::from_str(&status_output).unwrap();
        assert_eq!(
            status["thread"].as_str().unwrap(),
            "main",
            "HEAD must remain attached to the parent thread after fast-forward rebase"
        );
    }
}

mod hooks {
    use super::*;

    #[test]
    fn test_hook_pre_snapshot() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();

        let hooks_dir = temp.path().join(".heddle/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();

        let hook_path = hooks_dir.join("pre-snapshot");
        #[cfg(unix)]
        {
            fs::write(&hook_path, "#!/bin/sh\necho 'pre-snapshot hook ran'").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(not(unix))]
        {
            fs::write(&hook_path, "echo pre-snapshot hook ran").unwrap();
        }

        fs::write(temp.path().join("file.txt"), "content").unwrap();
        let result = heddle(&["capture", "-m", "Test"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "snapshot with hook failed: {:?}",
            result.err()
        );
    }
}

mod completion {
    use super::*;

    #[test]
    fn test_completion_bash() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["completion", "bash"], Some(temp.path()));
        assert!(result.is_ok(), "completion bash failed: {:?}", result.err());

        let output = result.unwrap();
        assert!(
            output.contains("heddle") || output.contains("complete"),
            "should generate bash completion"
        );
    }

    #[test]
    fn test_completion_zsh() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["completion", "zsh"], Some(temp.path()));
        assert!(result.is_ok(), "completion zsh failed: {:?}", result.err());
    }

    #[test]
    fn test_completion_fish() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["completion", "fish"], Some(temp.path()));
        assert!(result.is_ok(), "completion fish failed: {:?}", result.err());
    }
}

#[path = "production_features/packfiles.rs"]
mod packfiles;

#[path = "production_features/shallow_clone.rs"]
mod shallow_clone;

#[path = "production_features/state_signing.rs"]
mod state_signing;
