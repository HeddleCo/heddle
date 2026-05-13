// SPDX-License-Identifier: Apache-2.0
//! Crash-mid-write integration tests (R7 + R9 from the OSS-launch
//! plan).
//!
//! W2b shipped the rollback machinery — atomic mapping persistence,
//! mirror Drop guard, HEAD/index restore on failure, snapshot
//! state-then-ref ordering. These tests crash the heddle process at
//! the load-bearing transition points (via `HEDDLE_FAULT_INJECT`)
//! and verify the next clean process recovers without corruption.
//!
//! Both tests spawn child processes — the parent test sets
//! `HEDDLE_FAULT_INJECT`, runs the child, observes the intentional
//! panic, then runs a fresh child without the env var and asserts
//! the recovery contract.

use std::process::Command;

use super::*;

/// R9: bridge mapping persistence.
///
/// `bridge import` writes the heddle↔git mapping to disk via a
/// tmp-rename-rename pattern (`bridge-mapping.json.tmp` →
/// `bridge-mapping.json`). The fault checkpoint
/// `mapping_after_tmp_before_commit` panics in the gap between those
/// two operations, leaving the sidecar in a state where the .tmp
/// file exists but the canonical file does not (or is stale).
///
/// `recover_mapping_tmp` (in `load_mapping_from_disk`) is the recovery
/// path: on the next load, if a .tmp exists, it gets atomically
/// renamed into place. This test verifies that contract end-to-end:
/// crash, observe the on-disk shape, run a clean import, observe the
/// recovered shape.
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn bridge_recovers_from_crash_after_tmp_before_commit() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");

    // Build a small synthetic upstream so the mapping has real
    // entries to write (not just empty tables, which would
    // short-circuit the fault checkpoint). Reuses the helpers from
    // `cli_integration.rs` so the fixture shape matches the other
    // bridge tests.
    let origin_repo = gix::init_bare(&origin).expect("init origin");
    let blob = origin_repo.write_blob(b"fn a() {}\n").unwrap().detach();
    let mut tree_editor = origin_repo
        .edit_tree(origin_repo.empty_tree().id)
        .expect("tree editor");
    tree_editor
        .upsert("core.rs", gix::object::tree::EntryKind::Blob, blob)
        .unwrap();
    let tree_oid = tree_editor.write().unwrap().detach();
    let _commit =
        git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_oid, "seed", &[]);

    heddle_output_with_env(
        &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
        Some(temp.path()),
        &[],
    )
    .expect("initial clone succeeds");

    // ── Phase 1: spawn the import with fault injection armed ──
    //
    // The process should panic with our intentional message rather
    // than completing the bridge import. We explicitly assert the
    // panic message so a regression that silently no-ops the
    // checkpoint surfaces here, not three commits downstream.
    let crashed = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "bridge",
            "git",
            "import",
            "--path",
            origin.to_str().unwrap(),
        ])
        .current_dir(&work)
        .env("HEDDLE_FAULT_INJECT", "mapping_after_tmp_before_commit")
        .env("HEDDLE_CONFIG", work.join(".heddle-user/config.toml"))
        .output()
        .expect("spawn child");
    assert!(
        !crashed.status.success(),
        "child should panic, got success: stdout={} stderr={}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    let stderr = String::from_utf8_lossy(&crashed.stderr);
    assert!(
        stderr.contains("HEDDLE_FAULT_INJECT")
            && stderr.contains("mapping_after_tmp_before_commit"),
        "child should report the intentional panic: stderr={stderr}"
    );

    // ── Phase 2: observe the intermediate on-disk shape ──
    //
    // After the crash we expect either the .tmp to exist, or a
    // partial canonical file (depending on whether the crash
    // happened before or after the rename). Both shapes are valid
    // pre-recovery states; what matters is the recovery primitive
    // accepts both.
    let mapping_dir = work.join(".heddle").join("git-bridge");
    let canonical = mapping_dir.join("bridge-mapping.json");
    let tmp = mapping_dir.join("bridge-mapping.json.tmp");
    assert!(
        tmp.exists() || canonical.exists(),
        "after crash, at least one of the mapping files must exist; \
         dir contents: {:?}",
        std::fs::read_dir(&mapping_dir)
            .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .unwrap_or_default()
    );

    // ── Phase 3: clean re-run recovers ──
    //
    // No fault injection this time. The bridge load path runs
    // `recover_mapping_tmp`, atomically renames any leftover .tmp
    // into the canonical position, and proceeds with a normal
    // import. Final assertion: the canonical mapping file exists,
    // is non-empty, and parses as the expected shape.
    let recovered = heddle_output_with_env(
        &[
            "bridge",
            "git",
            "import",
            "--path",
            origin.to_str().unwrap(),
        ],
        Some(&work),
        &[],
    )
    .expect("recovery import succeeds");
    assert!(
        recovered.status.success(),
        "post-crash import should succeed cleanly: stderr={}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(
        canonical.exists(),
        "canonical mapping file must exist after recovery"
    );
    let body = std::fs::read_to_string(&canonical).expect("read mapping");
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("recovered mapping must parse as JSON");
    assert!(
        parsed.get("entries").is_some(),
        "recovered mapping must have entries field: {body}"
    );
    assert!(
        !tmp.exists(),
        "post-recovery, the .tmp must be gone (renamed into canonical)"
    );
}

/// R7: snapshot atomicity under SIGKILL-equivalent.
///
/// `repo::Repository::snapshot_with_attribution` is the workhorse for
/// `heddle capture`, `heddle agent capture`, etc.
/// Its atomicity contract: between `store.put_state(&state)` and
/// `refs.set_thread(&thread, &state.change_id)` is a small window
/// where the state is durable on disk but no ref points to it. A
/// crash there must result in: state object exists (orphan, harmless
/// — gc will eventually collect it) AND the prior ref is unchanged
/// (no half-applied advance).
///
/// This is the agent-API version of the same contract: a SIGKILL'd
/// `heddle agent capture` either fully landed or nothing — never a
/// half-write where the user's worktree thinks the change happened
/// but the ref disagrees.
///
/// We use `HEDDLE_FAULT_INJECT=snapshot_after_state_before_ref` to
/// hit the crash deterministically rather than relying on real
/// signal-based timing (which would be flaky).
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn snapshot_atomicity_under_simulated_sigkill() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    // Take a clean baseline snapshot so we have a known prior tip
    // to compare against post-crash.
    std::fs::write(temp.path().join("base.txt"), "baseline").unwrap();
    heddle(&["capture", "-m", "baseline"], Some(temp.path())).expect("baseline snapshot");

    let log_before: Value = serde_json::from_str(
        &heddle(&["--json", "log", "main", "-n", "5"], Some(temp.path())).expect("log"),
    )
    .unwrap();
    let baseline_tip = log_before["states"][0]["change_id"]
        .as_str()
        .expect("baseline tip change_id")
        .to_string();

    // ── Phase 1: snapshot with fault injection armed ──
    std::fs::write(temp.path().join("base.txt"), "would-be-captured").unwrap();
    let crashed = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["capture", "-m", "the capture that crashes"])
        .current_dir(temp.path())
        .env("HEDDLE_FAULT_INJECT", "snapshot_after_state_before_ref")
        .env(
            "HEDDLE_CONFIG",
            temp.path().join(".heddle-user/config.toml"),
        )
        .output()
        .expect("spawn child");
    assert!(
        !crashed.status.success(),
        "child should panic, got success: stderr={}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    let stderr = String::from_utf8_lossy(&crashed.stderr);
    assert!(
        stderr.contains("HEDDLE_FAULT_INJECT")
            && stderr.contains("snapshot_after_state_before_ref"),
        "child should report the intentional panic: stderr={stderr}"
    );

    // ── Phase 2: invariant — the prior tip is still the active
    //              ref. The captured state may exist as an orphan
    //              object (this is fine — gc collects it later)
    //              but no ref advanced and no thread is in a
    //              half-applied state.
    let log_after: Value = serde_json::from_str(
        &heddle(&["--json", "log", "main", "-n", "5"], Some(temp.path())).expect("post-crash log"),
    )
    .unwrap();
    let post_crash_tip = log_after["states"][0]["change_id"]
        .as_str()
        .expect("post-crash tip change_id");
    assert_eq!(
        post_crash_tip, baseline_tip,
        "thread tip must still point at the baseline state — anything else \
         is a half-written advance and a real atomicity bug"
    );

    // ── Phase 3: a fresh capture (without fault injection) lands
    //              cleanly. The worktree has the would-be-captured
    //              content; we just need to make sure we can record
    //              it now.
    let recovered_capture = heddle(
        &["--json", "capture", "-m", "post-recovery capture"],
        Some(temp.path()),
    )
    .expect("post-recovery capture succeeds");
    let recovered: Value = serde_json::from_str(&recovered_capture).unwrap();
    assert_eq!(recovered["intent"], "post-recovery capture");
    let new_tip = recovered["change_id"]
        .as_str()
        .expect("post-recovery change_id");
    assert_ne!(
        new_tip, baseline_tip,
        "the recovered capture must produce a fresh state, not silently \
         accept the orphaned mid-crash state"
    );
}