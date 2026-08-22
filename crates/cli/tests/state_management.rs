// SPDX-License-Identifier: Apache-2.0
//! Integration tests for the retained state-management commands.

use std::{fs, str};

use serde_json::Value;
use tempfile::TempDir;

#[path = "support/mod.rs"]
mod cli_test_support;

#[path = "state_management/merge_store_integrity.rs"]
mod merge_store_integrity;
#[path = "state_management/missing_tree_integrity.rs"]
mod missing_tree_integrity;
#[path = "state_management/revert.rs"]
mod revert;
#[path = "state_management/thread_integration.rs"]
mod thread_integration;

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    cli_test_support::heddle_env(args, cwd, &[])
}

fn heddle_output(
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> Result<std::process::Output, String> {
    cli_test_support::heddle_output_env(args, cwd, &[])
}

/// Remove one tree from the native object store while preserving every other
/// packed object as a loose copy. Snapshot commits are authoritative packs, so
/// corruption fixtures cannot assume the target tree has a loose file.
fn delete_tree_object(repo_root: &std::path::Path, target_hex: &str) -> bool {
    use objects::store::ObjectStore;
    use repo::Repository;

    let repo = Repository::open(repo_root).unwrap();
    let store = repo.store();
    let tree_ids = store.list_trees().unwrap();
    let target_present = tree_ids.iter().any(|id| id.to_hex() == target_hex);

    for state_id in store.list_states().unwrap() {
        let state = store.get_state(&state_id).unwrap().unwrap();
        store.put_state(&state).unwrap();
    }
    for blob_id in store.list_blobs().unwrap() {
        store.promote_to_loose_uncompressed(&blob_id).unwrap();
    }
    for tree_id in tree_ids {
        if tree_id.to_hex() == target_hex {
            continue;
        }
        let serialized = store.get_tree_serialized(&tree_id).unwrap().unwrap();
        store.put_tree_serialized(&serialized, tree_id).unwrap();
    }
    drop(repo);

    let packs_dir = repo_root.join(".heddle/packs");
    if packs_dir.exists() {
        for entry in fs::read_dir(&packs_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    }

    let loose_tree = repo_root
        .join(".heddle/objects/trees")
        .join(&target_hex[..2])
        .join(&target_hex[2..]);
    match fs::remove_file(&loose_tree) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to delete loose tree at {loose_tree:?}: {error}"),
    }
    let cached_tree = repo_root.join(".heddle/state/worktree-current-tree.bin");
    match fs::remove_file(&cached_tree) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove tree cache at {cached_tree:?}: {error}"),
    }
    target_present
}

fn status_json(path: &std::path::Path) -> Value {
    let output = heddle(&["status", "--output", "json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

pub(crate) fn assert_json_recovery_advice_fields(envelope: &Value, context: &str) {
    for field in [
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "recovery_commands",
        "hint",
    ] {
        assert!(
            envelope[field]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
                || envelope[field]
                    .as_array()
                    .is_some_and(|value| !value.is_empty()),
            "JSON recovery advice should expose `{field}` through structured fields: {context}"
        );
    }
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            !error.contains("Unsafe:")
                && !error.contains("Would change:")
                && !error.contains("Preserved:")
                && !error.contains("Primary recovery:")
                && !error.contains("Other recovery:")
        }),
        "JSON `error` should stay concise; recovery detail belongs in structured fields: {context}"
    );
    assert!(
        envelope
            .get("primary_command_template")
            .is_some_and(|template| template.is_null() || template.is_object()),
        "JSON recovery advice should expose `primary_command_template` as object or null: {context}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(|templates| templates.iter().all(|template| template.is_object())),
        "JSON recovery advice should expose `recovery_action_templates` as an array of template objects: {context}"
    );
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

fn assert_file_not_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(!path.exists(), "{}: {:?}", msg, path);
}

fn current_head_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&heddle(&["--output", "json", "show", "HEAD"], Some(path)).unwrap())
        .expect("show HEAD should return JSON")
}

#[test]
fn capture_without_message_refuses_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "text", "capture"], Some(temp.path())).unwrap();

    assert!(!output.status.success(), "capture without -m must fail");
    assert!(
        str::from_utf8(&output.stderr)
            .unwrap_or("")
            .contains("Next: heddle capture -m \"...\""),
        "text refusal should include the direct next command: {}",
        str::from_utf8(&output.stderr).unwrap_or("")
    );
    assert_eq!(current_head_json(temp.path()), before);
}

#[test]
fn capture_without_message_json_refusal_is_structured_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "json", "capture"], Some(temp.path())).unwrap();

    assert!(!output.status.success(), "capture without -m must fail");
    assert!(
        output.stdout.is_empty(),
        "failed JSON command should not emit stdout"
    );
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("stderr should be a JSON envelope");
    assert_eq!(envelope["kind"], "missing_capture_intent");
    assert_eq!(envelope["primary_command"], "heddle capture -m \"...\"");
    assert_json_recovery_advice_fields(&envelope, "capture without message");
    assert_eq!(current_head_json(temp.path()), before);
}

#[test]
fn commit_in_native_repo_redirects_to_capture_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "text", "commit"], Some(temp.path())).unwrap();

    assert!(
        !output.status.success(),
        "commit in a native repo must refuse"
    );
    assert!(
        str::from_utf8(&output.stderr)
            .unwrap_or("")
            .contains("Next: heddle capture -m \"...\""),
        "text refusal should include the direct next command: {}",
        str::from_utf8(&output.stderr).unwrap_or("")
    );
    assert_eq!(current_head_json(temp.path()), before);
}
