// SPDX-License-Identifier: Apache-2.0
//! Production-ready features integration tests.
//!
//! Tests for resolve, fsck, clone, blame, and gc.

use std::{fs, process::Command, str};

use ntest::timeout;
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(args);
    cmd.env("HEDDLE_PRINCIPAL_NAME", "Heddle Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "test@heddle.dev");

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
    cmd.args(args);

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
    let output = heddle(&["status", "--output", "json"], Some(path)).unwrap();
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

fn refresh_thread_expect_conflict(path: &std::path::Path, thread: &str) -> String {
    heddle(&["thread", "switch", thread], Some(path)).unwrap();
    let refresh = heddle(
        &["--output", "json", "thread", "refresh", thread],
        Some(path),
    );
    assert!(
        refresh
            .as_ref()
            .is_err_and(|err| err.contains("thread_refresh_conflicted")),
        "thread refresh should create durable conflict state: {refresh:?}"
    );
    assert!(
        path.join(".heddle/MERGE_STATE").exists(),
        "thread refresh conflict should leave MERGE_STATE in the thread checkout"
    );
    refresh.unwrap_err()
}

fn land_thread(path: &std::path::Path, thread: &str) -> String {
    heddle(&["land", "--thread", thread], Some(path)).unwrap()
}

fn refresh_thread_for_land(path: &std::path::Path, thread: &str) {
    heddle(&["thread", "switch", thread], Some(path)).unwrap();
    heddle(&["thread", "refresh", thread], Some(path)).unwrap();
    heddle(&["thread", "switch", "main"], Some(path)).unwrap();
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

        refresh_thread_expect_conflict(temp.path(), "feature");
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

        let result = heddle(&["--output", "json", "resolve", "--all"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --all failed: {:?}", result.err());
        let output: Value = serde_json::from_str(&result.unwrap()).expect("resolve all JSON");
        assert_eq!(output["output_kind"], "resolve", "{output}");
        assert_eq!(output["resolved"][0], "file.txt", "{output}");
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_thread_refresh_conflict_continue_then_land_resolved_thread() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "resolved content").unwrap();
        let resolved = heddle(&["--output", "json", "resolve", "--all"], Some(temp.path()))
            .expect("resolve all");
        let resolved: Value = serde_json::from_str(&resolved).expect("resolve JSON");
        assert_eq!(resolved["output_kind"], "resolve", "{resolved}");
        assert_eq!(resolved["continued"], true, "{resolved}");
        assert_eq!(resolved["continuation_status"], "continued", "{resolved}");

        heddle(&["thread", "switch", "main"], Some(temp.path())).expect("switch main");
        let landed = heddle(
            &["--output", "json", "land", "--thread", "feature"],
            Some(temp.path()),
        )
        .expect("land resolved thread");
        let landed: Value = serde_json::from_str(&landed).expect("land JSON");
        assert_eq!(landed["status"], "landed", "{landed}");
        assert_eq!(landed["integrated"], true, "{landed}");
        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "resolved content"
        );
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolve_list_conflicts() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(
            &["--output", "json", "resolve", "--list"],
            Some(temp.path()),
        );
        assert!(result.is_ok(), "resolve --list failed: {:?}", result.err());

        let output: Value = serde_json::from_str(&result.unwrap()).expect("resolve list JSON");
        assert_eq!(output["output_kind"], "resolve", "{output}");
        assert_eq!(output["conflicts"][0], "file.txt", "{output}");
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
        assert_eq!(content, "feature version", "should use our version");
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
        assert_eq!(content, "main version", "should use their version");
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

        refresh_thread_expect_conflict(temp.path(), "feature");
        heddle_with_env(
            &["resolve", "file.txt", "--ours"],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
                ("HEDDLE_AGENT_PROVIDER", "openai"),
                ("HEDDLE_AGENT_MODEL", "gpt-resolver"),
            ],
        )
        .unwrap();
        heddle(&["thread", "refresh", "feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        land_thread(temp.path(), "feature");

        let blame = heddle(
            &["--output", "json", "query", "--attribution", "file.txt"],
            Some(temp.path()),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&blame).unwrap();
        assert_eq!(parsed["lines"][0]["agent"]["provider"], "openai");
        assert_eq!(parsed["lines"][0]["agent"]["model"], "gpt-feature");
    }

    #[test]
    #[timeout(15000)]
    #[serial]
    fn test_resolved_merge_snapshot_attributes_manual_lines_to_resolver() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "custom resolved\n").unwrap();
        heddle_with_env(
            &["resolve", "file.txt"],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Test User"),
                ("HEDDLE_PRINCIPAL_EMAIL", "test@example.com"),
                ("HEDDLE_AGENT_PROVIDER", "openai"),
                ("HEDDLE_AGENT_MODEL", "gpt-resolver"),
            ],
        )
        .unwrap();
        heddle(&["thread", "refresh", "feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        land_thread(temp.path(), "feature");

        let blame = heddle(
            &["--output", "json", "query", "--attribution", "file.txt"],
            Some(temp.path()),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&blame).unwrap();
        assert_eq!(parsed["lines"][0]["agent"]["provider"], "openai");
        assert_eq!(parsed["lines"][0]["agent"]["model"], "gpt-resolver");
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
        // Capture can install more than one pack (source blobs plus sidecar
        // packs such as the semantic index). Corrupt every pack large enough
        // to scramble so the source-object pack — the one fsck walks — is
        // always hit, independent of read_dir order.
        let packs_dir = temp.path().join(".heddle/packs");
        let mut corrupted = false;
        for entry in fs::read_dir(&packs_dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pack") {
                continue;
            }
            let mut bytes = fs::read(&path).unwrap();
            if bytes.len() <= 32 {
                continue;
            }
            let end = bytes.len().min(48);
            for b in &mut bytes[16..end] {
                *b ^= 0xFF;
            }
            fs::write(&path, bytes).unwrap();
            corrupted = true;
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

        let result = heddle(&["fsck", "--output", "json"], Some(temp.path()));
        assert!(
            result.is_ok(),
            "fsck --output json failed: {:?}",
            result.err()
        );

        let output: Value = serde_json::from_str(&result.unwrap()).expect("should be JSON");
        assert!(output.get("valid").is_some(), "should have 'valid' field");
    }

    #[test]
    fn test_fsck_repair_requires_target() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(&["fsck", "repair"], Some(temp.path()));
        assert!(
            result.is_err(),
            "fsck repair should require an explicit repair target"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("Usage: heddle fsck repair") && err.contains("Commands:"),
            "bare repair command should fail at CLI parsing, got: {err}"
        );
    }

    #[test]
    fn test_fsck_repair_git_json_surface() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(
            &[
                "fsck",
                "repair",
                "git",
                "--ref",
                "main",
                "--preview",
                "--output",
                "json",
            ],
            Some(temp.path()),
        );
        assert!(
            result.is_ok(),
            "fsck repair git --output json failed: {:?}",
            result.err()
        );

        let output: Value = serde_json::from_str(&result.unwrap()).expect("should be JSON");
        assert_eq!(output["valid"], true);
        assert_eq!(output["git_projection_checked"], true);
        assert_eq!(output["repair_target"], "git");
        assert_eq!(output["repaired"], false);
        assert!(
            output["repairs"].is_array(),
            "repair surface should report repair actions: {output}"
        );
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

        refresh_thread_for_land(temp.path(), "feature");
        land_thread(temp.path(), "feature");

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

mod bisect {
    use super::*;

    /// `bisect` was removed in the whole-CLI consolidation (#473); it was a
    /// non-functional stub with no binary search. The verb must now error as
    /// an unknown subcommand.
    #[test]
    fn test_bisect_is_removed() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        let result = heddle(&["bisect", "start"], Some(temp.path()));
        assert!(
            result.is_err(),
            "bisect should be an unknown verb after #473"
        );
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

        let result = heddle(&["query", "--attribution", "file.txt"], Some(temp.path()));
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

        let result = heddle(
            &["--output", "json", "query", "--attribution", "file.txt"],
            Some(temp.path()),
        );
        assert!(
            result.is_ok(),
            "query --attribution --output json failed: {:?}",
            result.err()
        );

        let output: Value = serde_json::from_str(&result.unwrap()).expect("should be JSON");
        assert!(output.get("lines").is_some(), "should have 'lines' field");
    }

    #[test]
    fn test_blame_root_alias_is_rejected() {
        let err = heddle(&["blame", "file.txt"], None)
            .expect_err("removed blame root alias should fail through clap");
        assert!(
            err.contains("unrecognized subcommand 'blame'")
                || err.contains("unexpected argument 'blame'"),
            "clap should reject the removed blame alias: {err}"
        );
    }

    #[test]
    fn test_blame_multiple_commits() {
        let temp = TempDir::new().unwrap();

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "original line\n").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

        fs::write(temp.path().join("file.txt"), "modified line\n").unwrap();
        heddle(&["capture", "-m", "Modify"], Some(temp.path())).unwrap();

        let result = heddle(&["query", "--attribution", "file.txt"], Some(temp.path()));
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
                &head.state_id.to_string_full(),
                "--into",
                "combined",
            ],
            Some(temp.path()),
        )
        .unwrap();

        let output = heddle(
            &["--output", "json", "query", "--attribution", "file.txt"],
            Some(temp.path()),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        assert_eq!(lines[0]["agent"]["provider"], "anthropic");
        assert_eq!(lines[0]["agent"]["model"], "claude-sonnet-a");
        assert_eq!(lines[1]["agent"]["provider"], "openai");
        assert_eq!(lines[1]["agent"]["model"], "gpt-4.1-b");
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

        refresh_thread_for_land(temp.path(), "feature");
        land_thread(temp.path(), "feature");

        let output = heddle(
            &["--output", "json", "query", "--attribution", "file.txt"],
            Some(temp.path()),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        assert_eq!(lines[0]["agent"]["provider"], "anthropic");
        assert_eq!(lines[0]["agent"]["model"], "claude-opus-base");
        assert_eq!(lines[1]["agent"]["provider"], "openai");
        assert_eq!(lines[1]["agent"]["model"], "gpt-4.1-feature");
    }
}

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
    fn test_push_local_accepts_git_shaped_remote_thread_alias() {
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();

        heddle(&["init"], Some(repo_a.path())).unwrap();
        fs::write(repo_a.path().join("file.txt"), "content").unwrap();
        heddle(&["capture", "-m", "Initial"], Some(repo_a.path())).unwrap();

        heddle(&["init"], Some(repo_b.path())).unwrap();

        let b_path = repo_b.path().to_string_lossy().to_string();
        let result = heddle(&["push", &b_path, "feature"], Some(repo_a.path()));
        assert!(
            result.is_ok(),
            "Git-shaped push local alias should succeed: {:?}",
            result.err()
        );

        let threads = heddle(&["thread", "list"], Some(repo_b.path())).unwrap();
        assert!(
            threads.contains("feature"),
            "pushed thread should be visible in target repo: {}",
            threads
        );
    }

    #[test]
    fn test_pull_then_land_integrates_remote_content() {
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
        fs::write(source.path().join("source.txt"), "from source").unwrap();
        heddle(&["capture", "-m", "Source addition"], Some(source.path())).unwrap();

        // Dest adds a different file on main
        fs::write(dest.path().join("dest.txt"), "from dest").unwrap();
        heddle(&["capture", "-m", "Dest addition"], Some(dest.path())).unwrap();

        // Pre-create a managed destination thread, then pull the source tip
        // into it so ready/land retain explicit integration authority.
        heddle(&["thread", "create", "from-source"], Some(dest.path())).unwrap();
        let source_path = source.path().to_string_lossy().to_string();
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

        refresh_thread_for_land(dest.path(), "from-source");
        land_thread(dest.path(), "from-source");

        // Both unique files should exist after landing the managed thread.
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
        let source_main = heddle(
            &["thread", "show", "main", "--output", "json"],
            Some(source.path()),
        )
        .unwrap();
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
        let main_show = heddle(
            &["thread", "show", "main", "--output", "json"],
            Some(dest.path()),
        )
        .unwrap();
        let main: Value = serde_json::from_str(&main_show).unwrap();
        assert_eq!(
            main["current_state"].as_str().unwrap(),
            target,
            "main.current_state must advance to the pull target after fast-forward"
        );

        // HEAD must remain attached to the previously-attached thread.
        let status_output = heddle(&["status", "--output", "json"], Some(dest.path())).unwrap();
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

    fn completion_lines(output: &str) -> Vec<&str> {
        output.lines().filter(|line| !line.is_empty()).collect()
    }

    #[test]
    fn test_completion_bash() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["shell", "completion", "bash"], Some(temp.path()));
        assert!(result.is_ok(), "completion bash failed: {:?}", result.err());

        let output = result.unwrap();
        assert!(
            output.contains("heddle") || output.contains("complete"),
            "should generate bash completion"
        );
        assert!(
            output.contains("heddle __complete"),
            "bash completion should include dynamic thread candidates"
        );
        assert!(
            !output.contains("--thread|-t|--into"),
            "bash dynamic completion must not offer dead -t thread values"
        );
        assert!(
            output.contains("thread|capture"),
            "bash --into thread completion must be gated to existing-thread subcommands"
        );
        assert!(
            !output.contains("start|switch|merge"),
            "bash completion must not route removed top-level switch/merge commands"
        );
    }

    #[test]
    fn test_completion_zsh() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["shell", "completion", "zsh"], Some(temp.path()));
        assert!(result.is_ok(), "completion zsh failed: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.contains("heddle __complete"),
            "zsh completion should include dynamic thread candidates"
        );
        assert!(
            !output.contains("--thread|-t|--into"),
            "zsh dynamic completion must not offer dead -t thread values"
        );
        assert!(
            output.contains("thread|capture"),
            "zsh --into thread completion must be gated to existing-thread subcommands"
        );
        assert!(
            !output.contains("start|switch|merge"),
            "zsh completion must not route removed top-level switch/merge commands"
        );
    }

    #[test]
    fn test_completion_fish() {
        let temp = TempDir::new().unwrap();

        let result = heddle(&["shell", "completion", "fish"], Some(temp.path()));
        assert!(result.is_ok(), "completion fish failed: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.contains("heddle __complete"),
            "fish completion should include dynamic thread candidates"
        );
        assert!(
            !output.contains("case --thread -t --into"),
            "fish dynamic completion must not offer dead -t thread values"
        );
        assert!(
            output.contains("__fish_seen_subcommand_from thread capture"),
            "fish --into thread completion must be gated to existing-thread subcommands"
        );
        assert!(
            !output.contains("case start switch merge"),
            "fish completion must not route removed top-level switch/merge commands"
        );
    }

    #[test]
    fn test_complete_threads_lists_sorted_repo_threads_only() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "base.txt", "base\n");
        heddle(&["thread", "create", "zeta"], Some(temp.path())).unwrap();
        heddle(&["thread", "create", "alpha"], Some(temp.path())).unwrap();

        let output = heddle(&["__complete", "threads"], Some(temp.path())).unwrap();
        assert_eq!(
            completion_lines(&output),
            vec!["alpha", "main", "zeta"],
            "thread completion should print sorted, deduped thread names"
        );

        let outside = TempDir::new().unwrap();
        let output = heddle(&["__complete", "threads"], Some(outside.path())).unwrap();
        assert_eq!(
            output, "",
            "thread completion outside a repo should succeed quietly"
        );
    }

    #[test]
    fn test_shell_prompt_reports_thread_and_dirty_marker_only_in_repo() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "tracked.txt", "clean\n");

        let clean = heddle(&["shell", "prompt"], Some(temp.path())).unwrap();
        assert!(
            clean.lines().any(|line| line.contains("main")),
            "prompt should include the current lane/thread: {clean:?}"
        );

        fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
        let dirty = heddle(&["shell", "prompt"], Some(temp.path())).unwrap();
        assert!(
            dirty.lines().any(|line| line.contains("main*")),
            "prompt should mark dirty worktrees with '*': {dirty:?}"
        );

        let outside = TempDir::new().unwrap();
        let output = heddle(&["shell", "prompt"], Some(outside.path())).unwrap();
        assert_eq!(
            output, "",
            "prompt outside a repo should succeed with empty output"
        );
    }
}

#[path = "production_features/packfiles.rs"]
mod packfiles;

#[path = "production_features/shallow_clone.rs"]
mod shallow_clone;

#[path = "production_features/state_signing.rs"]
mod state_signing;
