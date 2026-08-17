// SPDX-License-Identifier: Apache-2.0
//! Production-ready features integration tests.
//!
//! Tests for resolve, fsck, clone, blame, and gc.

use std::{fs, process::Command, str};

use ntest::timeout;
use objects::object::{StateAttachmentBody, StructuredConflict};
use oplog::{ConflictResolutionMode, OpLogBackend, OpRecord};
use repo::Repository;
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

#[path = "support/mod.rs"]
mod cli_test_support;

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    cli_test_support::heddle(args, cwd, &[])
}

fn heddle_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<String, String> {
    cli_test_support::heddle(args, cwd, envs)
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

    fn recent_oplog_entries(temp: &TempDir) -> Vec<oplog::OpEntry> {
        Repository::open(temp.path())
            .unwrap()
            .oplog()
            .recent(200)
            .unwrap()
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
    fn test_resolve_marks_file_as_resolved() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        fs::write(temp.path().join("file.txt"), "resolved content").unwrap();

        let result = heddle(&["resolve", "file.txt"], Some(temp.path()));
        assert!(result.is_ok(), "resolve failed: {:?}", result.err());
    }

    #[test]
    #[serial(inner_attrs = [timeout(30000)])]
    fn manual_resolve_appends_attributed_conflict_resolved_op_record() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);
        fs::write(temp.path().join("file.txt"), "resolved content").unwrap();

        let resolved = heddle_with_env(
            &["--output", "json", "resolve", "file.txt"],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Resolution Owner"),
                ("HEDDLE_PRINCIPAL_EMAIL", "owner@example.com"),
                ("HEDDLE_AGENT_PROVIDER", "openai"),
                ("HEDDLE_AGENT_MODEL", "gpt-resolver"),
            ],
        )
        .unwrap();
        let resolved: Value = serde_json::from_str(&resolved).expect("resolve JSON");
        let resolution = &resolved["resolutions"][0];
        assert_eq!(resolution["path"], "file.txt", "{resolved}");
        assert_eq!(resolution["mode"], "edit", "{resolved}");
        assert_eq!(resolution["resolver"]["kind"], "agent", "{resolved}");
        assert_eq!(
            resolution["resolver"]["agent"]["provider"], "openai",
            "{resolved}"
        );

        let entries = recent_oplog_entries(&temp);
        let event = entries
            .iter()
            .find_map(|entry| match &entry.operation {
                OpRecord::ConflictResolved {
                    conflict_id,
                    resolution,
                    resolver,
                    mode,
                } => Some((conflict_id, resolution, resolver, mode)),
                _ => None,
            })
            .expect("production resolve must append ConflictResolved");
        assert!(event.0.starts_with("conflict-"), "{}", event.0);
        assert_eq!(event.0, resolution["conflict_id"].as_str().unwrap());
        assert_eq!(event.1, "edit");
        assert_eq!(*event.3, ConflictResolutionMode::Edit);
        assert_eq!(event.2.principal.name, b"Resolution Owner");
        assert_eq!(event.2.principal.email, b"owner@example.com");
        let agent = event.2.agent.as_ref().expect("resolver agent attribution");
        assert_eq!(agent.provider, "openai");
        assert_eq!(agent.model, "gpt-resolver");
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
    fn rebase_auto_resolution_appends_attributed_conflict_resolved_op_record() {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "feature-one\ntwo\nthree\n").unwrap();
        heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("file.txt"), "one\ntwo\nmain-three\n").unwrap();
        heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        heddle_with_env(
            &["thread", "refresh", "feature"],
            Some(temp.path()),
            &[
                ("HEDDLE_PRINCIPAL_NAME", "Automation Owner"),
                ("HEDDLE_PRINCIPAL_EMAIL", "automation@example.com"),
                ("HEDDLE_AGENT_PROVIDER", "openai"),
                ("HEDDLE_AGENT_MODEL", "gpt-auto-resolver"),
            ],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "feature-one\ntwo\nmain-three\n"
        );
        let entries = recent_oplog_entries(&temp);
        let event = entries
            .iter()
            .find_map(|entry| match &entry.operation {
                OpRecord::ConflictResolved {
                    conflict_id,
                    resolution,
                    resolver,
                    mode: ConflictResolutionMode::Auto,
                } => Some((conflict_id, resolution, resolver)),
                _ => None,
            })
            .expect("rebase auto-merge must append ConflictResolved");
        assert_eq!(event.0, "file.txt");
        assert_eq!(event.1, "auto");
        assert_eq!(event.2.principal.name, b"Automation Owner");
        assert_eq!(event.2.principal.email, b"automation@example.com");
        let agent = event
            .2
            .agent
            .as_ref()
            .expect("auto resolver agent attribution");
        assert_eq!(agent.provider, "openai");
        assert_eq!(agent.model, "gpt-auto-resolver");
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
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
    #[serial(inner_attrs = [timeout(30000)])]
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
        let repo = Repository::open(temp.path()).unwrap();
        let merged_state = repo.head().unwrap().expect("resolved merge state");
        let attachment = repo
            .latest_state_attachment(
                &merged_state,
                repo::StateAttachmentKind::StructuredConflicts,
            )
            .unwrap()
            .expect("structured conflicts retained on resolved state");
        assert!(matches!(
            attachment.body,
            StateAttachmentBody::StructuredConflicts(_)
        ));

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
    #[serial(inner_attrs = [timeout(15000)])]
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
        assert_eq!(output["conflict_paths"][0], "file.txt", "{output}");
        let conflict = &output["conflicts"][0];
        assert_eq!(conflict["path"], "file.txt", "{output}");
        assert!(
            conflict["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("conflict-")),
            "{output}"
        );
        for side in ["base", "ours", "theirs"] {
            assert!(conflict[side]["source_state"].is_string(), "{output}");
            assert!(conflict[side]["blob_id"].is_string(), "{output}");
            assert!(conflict[side]["hunk_hash"].is_string(), "{output}");
            assert!(
                conflict[side]["range"]["start_line"].is_number(),
                "{output}"
            );
            assert!(conflict[side]["range"]["end_line"].is_number(), "{output}");
        }

        let repo = Repository::open(temp.path()).unwrap();
        let merge_state = repo.merge_state_manager().load().unwrap().unwrap();
        let payload_id = merge_state
            .structured_conflicts
            .expect("structured conflict payload id");
        let payload_blob = repo.require_blob(&payload_id).unwrap();
        assert_eq!(payload_blob.hash(), payload_id);
        let payload = StructuredConflict::decode(payload_blob.content()).unwrap();
        assert_eq!(payload.conflicts.len(), 1);
        for side in [
            &payload.conflicts[0].base,
            &payload.conflicts[0].ours,
            &payload.conflicts[0].theirs,
        ] {
            let blob = repo.require_blob(&side.blob_id.unwrap()).unwrap();
            side.verify_blob(blob.content()).unwrap();
        }
    }

    #[test]
    #[timeout(15000)]
    fn resolve_schema_exposes_structured_regions_and_resolution_records() {
        let schema = heddle(&["schemas", "resolve"], None).expect("resolve schema");
        let schema: Value = serde_json::from_str(&schema).expect("resolve JSON Schema");
        let properties = &schema["properties"];
        assert_eq!(properties["conflicts"]["type"], "array", "{schema}");
        assert_eq!(properties["resolutions"]["type"], "array", "{schema}");
        let defs = schema["$defs"].as_object().expect("schema definitions");
        assert!(defs.contains_key("ConflictRegionReport"), "{schema}");
        assert!(defs.contains_key("ConflictResolutionReport"), "{schema}");
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
    fn test_resolve_with_ours() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["resolve", "file.txt", "--ours"], Some(temp.path()));
        assert!(result.is_ok(), "resolve --ours failed: {:?}", result.err());

        let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
        assert_eq!(content, "feature version", "should use our version");
        assert!(recent_oplog_entries(&temp).iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::ConflictResolved {
                conflict_id,
                resolution,
                mode: ConflictResolutionMode::Ours,
                ..
            } if conflict_id.starts_with("conflict-") && resolution == "ours"
        )));
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
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
        assert!(recent_oplog_entries(&temp).iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::ConflictResolved {
                conflict_id,
                resolution,
                mode: ConflictResolutionMode::Theirs,
                ..
            } if conflict_id.starts_with("conflict-") && resolution == "theirs"
        )));
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
    fn test_abort() {
        let temp = TempDir::new().unwrap();
        create_conflict(&temp);

        let result = heddle(&["abort"], Some(temp.path()));
        assert!(result.is_ok(), "abort failed: {:?}", result.err());
    }

    #[test]
    #[serial(inner_attrs = [timeout(15000)])]
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
    #[serial(inner_attrs = [timeout(15000)])]
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

        let result = heddle(&["maintenance", "fsck"], Some(temp.path()));
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

        // Capture stores the source blob in an authoritative snapshot pack.
        // Materialize every packed object as loose before removing the packs,
        // then corrupt the exact blob referenced by the current tree. This
        // keeps the fixture deterministic as the pack layout evolves.
        use objects::store::ObjectStore;

        let repo = Repository::open(temp.path()).unwrap();
        let store = repo.store();
        let state = repo.current_state().unwrap().unwrap();
        let tree = store.get_tree(&state.tree).unwrap().unwrap();
        let blob = tree
            .get("file.txt")
            .and_then(|entry| entry.blob_hash())
            .expect("captured file should reference a blob");
        for state_id in store.list_states().unwrap() {
            let state = store.get_state(&state_id).unwrap().unwrap();
            store.put_state(&state).unwrap();
        }
        for tree_id in store.list_trees().unwrap() {
            let serialized = store.get_tree_serialized(&tree_id).unwrap().unwrap();
            store.put_tree_serialized(&serialized, tree_id).unwrap();
        }
        for blob_id in store.list_blobs().unwrap() {
            store.promote_to_loose_uncompressed(&blob_id).unwrap();
        }
        let blob_path = store
            .loose_blob_path(&blob)
            .expect("source blob should have a loose copy");
        drop(repo);

        let packs_dir = temp.path().join(".heddle/packs");
        for entry in fs::read_dir(&packs_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
        fs::write(blob_path, b"corrupt blob").unwrap();

        let error = heddle(&["maintenance", "fsck", "--full"], Some(temp.path()))
            .expect_err("full fsck must fail when a source-object pack is corrupt");
        assert!(
            ["error", "mismatch", "invalid", "corrupt"]
                .iter()
                .any(|needle| error.to_ascii_lowercase().contains(needle)),
            "fsck should identify the corrupted pack: {error}"
        );
    }

    #[test]
    fn test_fsck_json_output() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(
            &["maintenance", "fsck", "--output", "json"],
            Some(temp.path()),
        );
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

        let result = heddle(&["maintenance", "fsck", "repair"], Some(temp.path()));
        assert!(
            result.is_err(),
            "fsck repair should require an explicit repair target"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("Usage: heddle maintenance fsck repair") && err.contains("Commands:"),
            "bare repair command should fail at CLI parsing, got: {err}"
        );
    }

    #[test]
    fn test_fsck_repair_git_json_surface() {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "content");

        let result = heddle(
            &[
                "maintenance",
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
            "maintenance fsck repair git --output json failed: {:?}",
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

        let result = heddle(&["maintenance", "fsck", "--full"], Some(temp.path()));
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

        let result = heddle(
            &["maintenance", "fsck", "--full", "--thorough"],
            Some(temp.path()),
        );
        assert!(
            result.is_ok(),
            "fsck after merge should pass: {:?}",
            result.err()
        );
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
    #[serial(inner_attrs = [timeout(15000)])]
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
    #[serial(inner_attrs = [timeout(30000)])]
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

        let result = heddle(&["maintenance", "fsck", "--full"], Some(temp.path()));
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

        let error = heddle(
            &["push", "origin", "--force-with-lease"],
            Some(local.path()),
        )
        .expect_err("force-with-lease requires an established tracking state");
        assert!(
            error.contains("lease") || error.contains("tracking") || error.contains("expected"),
            "missing tracking state should produce a lease diagnostic: {error}"
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

    #[cfg(unix)]
    #[test]
    fn bash_dynamic_completion_never_evaluates_ref_names() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let completion = heddle(&["shell", "completion", "bash"], Some(temp.path())).unwrap();
        let completion_path = temp.path().join("heddle-completion.bash");
        fs::write(&completion_path, completion).unwrap();

        let marker = temp.path().join("executed");
        let bin_dir = temp.path().join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let fake_heddle = bin_dir.join("heddle");
        fs::write(
            &fake_heddle,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'safe' '$(printf pwned > {})'\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_heddle, fs::Permissions::from_mode(0o755)).unwrap();

        let output = Command::new("bash")
            .arg("-c")
            .arg(
                r#"source "$1"
COMP_WORDS=(heddle thread show "")
COMP_CWORD=3
__heddle_complete_from threads
printf '%s\n' "${COMPREPLY[@]}""#,
            )
            .arg("bash")
            .arg(&completion_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("run generated Bash completion");

        assert!(
            output.status.success(),
            "generated completion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("$(printf pwned >"),
            "the malicious-looking but valid name should remain a literal completion candidate"
        );
        assert!(
            !marker.exists(),
            "dynamic completion must not evaluate candidate contents"
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
