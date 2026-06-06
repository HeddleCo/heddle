// SPDX-License-Identifier: Apache-2.0
//! Crash-mid-write integration tests (R7 + R9 from the OSS-launch
//! plan).
//!
//! W2b landed the rollback machinery — atomic mapping persistence,
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

/// heddle#550 Finding 1: object durability must be ordered BEFORE the
/// recoverable mapping that references those objects.
///
/// `import_all` writes imported objects into a snapshot batch, runs the
/// `flush_snapshot_write_batch` durability barrier (`syncfs` + promote),
/// and only then writes the heddle↔git mapping sidecar
/// (`bridge-mapping.json.tmp` → `bridge-mapping.json`). The sidecar is
/// recoverable on the next load (a leftover `.tmp` is renamed into
/// place), so if it were written BEFORE the flush, a crash in the gap
/// would leave a recoverable mapping pointing at objects whose data was
/// never flushed.
///
/// The `import_after_flush_before_mapping` checkpoint sits in exactly
/// that gap — after the flush, before the mapping tmp write. Crashing
/// there must leave NO mapping tmp on disk and must NOT have advanced the
/// committed mapping: proof that the flush ran first. A clean re-run then
/// recovers and records the new commit.
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn import_flushes_objects_before_writing_referencing_mapping() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");

    // Seed origin with commit A and clone it (the clone imports A and
    // writes the initial mapping sidecar).
    let origin_repo = gix::init_bare(&origin).expect("init origin");
    let blob_a = origin_repo.write_blob(b"fn a() {}\n").unwrap().detach();
    let mut tree_a = origin_repo
        .edit_tree(origin_repo.empty_tree().id)
        .expect("tree editor");
    tree_a
        .upsert("core.rs", gix::object::tree::EntryKind::Blob, blob_a)
        .unwrap();
    let tree_a_oid = tree_a.write().unwrap().detach();
    let commit_a =
        git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_a_oid, "seed", &[]);

    heddle_output_with_env(
        &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
        Some(temp.path()),
        &[],
    )
    .expect("initial clone succeeds");

    let mapping_dir = work.join(".heddle").join("git-bridge");
    let canonical = mapping_dir.join("bridge-mapping.json");
    let tmp = mapping_dir.join("bridge-mapping.json.tmp");
    let mapping_after_clone =
        std::fs::read_to_string(&canonical).expect("clone writes the initial mapping");

    // Add a NEW commit B to origin so the next import does real object
    // work (a fresh state + tree + blob flow through the batch flush).
    let blob_b = origin_repo.write_blob(b"fn b() {}\n").unwrap().detach();
    let mut tree_b = origin_repo
        .edit_tree(origin_repo.empty_tree().id)
        .expect("tree editor");
    tree_b
        .upsert("core.rs", gix::object::tree::EntryKind::Blob, blob_b)
        .unwrap();
    let tree_b_oid = tree_b.write().unwrap().detach();
    let commit_b = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        tree_b_oid,
        "second",
        &[commit_a],
    );

    // ── Phase 1: import B, crashing in the post-flush / pre-mapping gap ──
    let crashed = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["bridge", "git", "import", "--path", origin.to_str().unwrap()])
        .current_dir(&work)
        .env("HEDDLE_FAULT_INJECT", "import_after_flush_before_mapping")
        .env("HEDDLE_CONFIG", work.join(".heddle-user/config.toml"))
        .output()
        .expect("spawn child");
    assert_intentional_snapshot_crash(crashed, "import_after_flush_before_mapping");

    // ── Phase 2: the flush ran, but the mapping was NOT touched ──
    //
    // No mapping tmp may exist (write_mapping_tmp_to_disk never ran), and
    // the committed mapping must still be exactly what the clone left —
    // it must NOT yet reference commit B. Either failure means the
    // referencing artifact was made durable before the object flush.
    assert!(
        !tmp.exists(),
        "no mapping tmp may exist after a crash before the mapping write — \
         its presence means write_mapping_tmp ran before flush",
    );
    let mapping_after_crash =
        std::fs::read_to_string(&canonical).expect("committed mapping survives the crash");
    assert_eq!(
        mapping_after_crash, mapping_after_clone,
        "the post-flush crash must not have advanced the committed mapping",
    );
    assert!(
        !mapping_after_crash.contains(&commit_b.to_string()),
        "committed mapping must not reference commit B before the mapping write",
    );

    // ── Phase 3: a clean re-run recovers and records commit B ──
    let recovered = heddle_output_with_env(
        &["bridge", "git", "import", "--path", origin.to_str().unwrap()],
        Some(&work),
        &[],
    )
    .expect("recovery import succeeds");
    assert!(
        recovered.status.success(),
        "post-crash import should succeed cleanly: stderr={}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(!tmp.exists(), "recovery leaves no leftover mapping tmp");
    let recovered_mapping = std::fs::read_to_string(&canonical).expect("read recovered mapping");
    assert!(
        recovered_mapping.contains(&commit_b.to_string()),
        "recovered mapping must now reference commit B",
    );
}

fn current_main_tip(repo: &std::path::Path) -> String {
    let log: Value = serde_json::from_str(
        &heddle(&["--output", "json", "log", "main", "-n", "5"], Some(repo)).expect("log"),
    )
    .unwrap();
    log["states"][0]["change_id"]
        .as_str()
        .expect("tip change_id")
        .to_string()
}

fn current_head_tip(repo: &std::path::Path) -> String {
    let log: Value = serde_json::from_str(
        &heddle(&["--output", "json", "log", "--limit", "1"], Some(repo)).expect("log"),
    )
    .unwrap();
    log["states"][0]["change_id"]
        .as_str()
        .expect("tip change_id")
        .to_string()
}

fn assert_intentional_snapshot_crash(crashed: std::process::Output, checkpoint: &str) {
    assert!(
        !crashed.status.success(),
        "child should panic, got success: stdout={} stderr={}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    let stderr = String::from_utf8_lossy(&crashed.stderr);
    assert!(
        stderr.contains("HEDDLE_FAULT_INJECT") && stderr.contains(checkpoint),
        "child should report the intentional panic at {checkpoint}: stderr={stderr}"
    );
}

fn crash_capture_at(repo: &std::path::Path, checkpoint: &str, message: &str) {
    let crashed = heddle_output_with_env(
        &["capture", "-m", message],
        Some(repo),
        &[("HEDDLE_FAULT_INJECT", checkpoint)],
    )
    .expect("spawn child");
    assert_intentional_snapshot_crash(crashed, checkpoint);
}

fn crash_goto_at(repo: &std::path::Path, checkpoint: &str, target: &str) {
    let crashed = heddle_output_with_env(
        &["goto", target],
        Some(repo),
        &[("HEDDLE_FAULT_INJECT", checkpoint)],
    )
    .expect("spawn child");
    assert_intentional_snapshot_crash(crashed, checkpoint);
}

fn init_repo_with_baseline() -> (TempDir, String) {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    // Take a clean baseline snapshot so we have a known prior tip
    // to compare against post-crash.
    std::fs::write(temp.path().join("base.txt"), "baseline").unwrap();
    heddle(&["capture", "-m", "baseline"], Some(temp.path())).expect("baseline snapshot");

    let baseline_tip = current_main_tip(temp.path());
    (temp, baseline_tip)
}

/// R7/O1: pre-commit snapshot crash is invisible.
///
/// `repo::Repository::snapshot_with_attribution` stages snapshot objects before
/// appending the atomic `TransactionCommit` marker. A crash at
/// `snapshot_after_stage_before_atomic_commit` leaves staged content behind, but
/// without the commit marker it is not authoritative. The next Heddle read must
/// keep the thread tip on the prior baseline and must not resurrect the staged
/// but uncommitted content.
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn snapshot_atomicity_before_commit_crash_stays_on_baseline() {
    let (temp, baseline_tip) = init_repo_with_baseline();

    // ── Phase 1: snapshot with pre-commit fault injection armed ──
    std::fs::write(temp.path().join("base.txt"), "would-be-captured").unwrap();
    crash_capture_at(
        temp.path(),
        "snapshot_after_stage_before_atomic_commit",
        "the capture that crashes before commit",
    );

    // ── Phase 2: invariant — no TransactionCommit marker landed, so the
    //              prior tip remains authoritative after reconcile-on-read.
    let post_crash_tip = current_main_tip(temp.path());
    assert_eq!(
        post_crash_tip, baseline_tip,
        "thread tip must still point at the baseline state — anything else \
         is a half-written advance and a real atomicity bug",
    );

    let reread_tip = current_main_tip(temp.path());
    assert_eq!(
        reread_tip, baseline_tip,
        "a second read must not resurrect the staged-but-uncommitted snapshot",
    );
}

/// R7/O1: post-commit snapshot crash is recovered exactly once.
///
/// After `TransactionCommit` is durable, the oplog is the commit point and the
/// thread ref is only a materialized view. A crash at
/// `snapshot_after_atomic_commit_before_ref_publish` leaves the ref stale, but
/// the next Heddle read reconciles from the committed oplog tail and republishes
/// the capture. Re-reading must be idempotent: no second logical snapshot is
/// applied.
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn snapshot_atomicity_after_commit_crash_recovers_once() {
    let (temp, baseline_tip) = init_repo_with_baseline();

    // ── Phase 1: snapshot with post-commit fault injection armed ──
    std::fs::write(temp.path().join("base.txt"), "committed-before-ref-publish").unwrap();
    crash_capture_at(
        temp.path(),
        "snapshot_after_atomic_commit_before_ref_publish",
        "the capture that commits before crashing",
    );

    // ── Phase 2: the first Heddle read reconciles the committed oplog record
    //              and advances the materialized thread ref.
    let recovered_tip = current_main_tip(temp.path());
    assert_ne!(
        recovered_tip, baseline_tip,
        "post-commit crash recovery must advance the tip from the baseline",
    );

    let reread_tip = current_main_tip(temp.path());
    assert_eq!(
        reread_tip, recovered_tip,
        "a second read must not apply the committed snapshot a second time",
    );

    let retry_read_tip = current_main_tip(temp.path());
    assert_eq!(
        retry_read_tip, recovered_tip,
        "retrying reconcile-on-read must not advance the tip again",
    );
}

/// Goto is record-first: a crash after its `OpRecord::Goto` commits but before
/// HEAD is published must reconstruct detached HEAD from the record on the next
/// read. Re-reading must be idempotent and must not move the still-attached
/// source thread.
#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn goto_after_commit_crash_recovers_detached_head_once() {
    let (temp, baseline_tip) = init_repo_with_baseline();

    std::fs::write(temp.path().join("base.txt"), "second").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("second snapshot");
    let second_tip = current_main_tip(temp.path());
    assert_ne!(
        second_tip, baseline_tip,
        "fixture must have a distinct second tip"
    );

    crash_goto_at(
        temp.path(),
        "goto_after_oplog_commit_before_ref_publish",
        &baseline_tip,
    );

    let recovered_head = current_head_tip(temp.path());
    assert_eq!(
        recovered_head, baseline_tip,
        "post-commit goto crash recovery must detach HEAD to the committed target",
    );
    assert_eq!(
        current_head_tip(temp.path()),
        recovered_head,
        "a second HEAD read must not apply the committed goto a second time",
    );
    assert_eq!(
        current_main_tip(temp.path()),
        second_tip,
        "goto recovery must not move the source thread ref",
    );
}
