// SPDX-License-Identifier: Apache-2.0
//! Process-level crash recovery tests for timeline materialization.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use objects::object::{ChangeId, ContentHash};
use repo::{
    BranchCreatedV1, NativeToolCallRefV1, Repository, TimelineBranchId, TimelineBranchReason,
    TimelineMaterializationRecoveryStatus, TimelineMaterializeMode, TimelineMaterializeStatus,
    TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineSeekSelector, TimelineStepId,
    TimelineStore, TimelineToolCallStatus, TimelineToolPayloadMetadata, TimelineView,
    ToolCallFinishedV1, ToolCallStartedV1,
};
use tempfile::TempDir;

const CHILD_TEST: &str = "timeline_materialize_fault_child_process";
const CHILD_REPO_ENV: &str = "HEDDLE_TIMELINE_FAULT_REPO";
const CHILD_PHASE_ENV: &str = "HEDDLE_TIMELINE_FAULT_PHASE";
const FAULT_POINT: &str = "timeline_materialize_after_goto_before_cursor_move";

#[test]
#[ignore = "fault-injection: spawns child processes with HEDDLE_FAULT_INJECT"]
fn timeline_materialize_after_goto_crash_retries_cursor_move() {
    let fixture = create_fixture();
    let repo_path = fixture.temp.path();

    let crashed = run_child(repo_path, "crash", Some(FAULT_POINT));
    assert_intentional_crash(crashed, FAULT_POINT);

    let repo = Repository::open(repo_path).expect("open repo after crash");
    let store = TimelineStore::open(repo.heddle_dir()).expect("open timeline after crash");
    assert_eq!(repo.head().expect("head after crash"), Some(fixture.state1));
    assert_eq!(
        fs::read_to_string(repo_path.join("tracked.txt")).expect("read checkout after crash"),
        "one\n"
    );
    assert!(
        store
            .read_materialization_recovery("main")
            .expect("read staged recovery")
            .is_some(),
        "crash must leave a recovery sidecar staged"
    );
    let view = TimelineView::rebuild(&store).expect("rebuild timeline after crash");
    assert_eq!(
        view.status("main")
            .expect("timeline status after crash")
            .current_step_id,
        Some(step("tls-two")),
        "the crash happened before cursor_moved, so the logical cursor must still be old"
    );

    let recovered = run_child(repo_path, "recover", None);
    assert!(
        recovered.status.success(),
        "clean retry child should complete recovery: stdout={} stderr={}",
        String::from_utf8_lossy(&recovered.stdout),
        String::from_utf8_lossy(&recovered.stderr)
    );

    let repo = Repository::open(repo_path).expect("open repo after recovery");
    let store = TimelineStore::open(repo.heddle_dir()).expect("open timeline after recovery");
    assert_eq!(
        repo.head().expect("head after recovery"),
        Some(fixture.state1)
    );
    assert!(
        store
            .read_materialization_recovery("main")
            .expect("read recovery after retry")
            .is_none(),
        "successful retry must clear the recovery sidecar"
    );
    let view = TimelineView::rebuild(&store).expect("rebuild timeline after recovery");
    let status = view.status("main").expect("timeline status after recovery");
    assert_eq!(status.current_step_id, Some(step("tls-one")));
    assert_eq!(status.current_state, Some(fixture.state1));
}

#[test]
#[ignore = "child process entrypoint for timeline materialization fault tests"]
fn timeline_materialize_fault_child_process() {
    let Some(repo_path) = std::env::var_os(CHILD_REPO_ENV).map(PathBuf::from) else {
        return;
    };
    let phase = std::env::var(CHILD_PHASE_ENV).expect("child phase env");
    let repo = Repository::open(&repo_path).expect("open repo in child");
    let store = TimelineStore::open(repo.heddle_dir()).expect("open timeline in child");

    let outcome = repo
        .materialize_timeline_cursor(
            &store,
            "main",
            &TimelineSeekSelector::StepId(step("tls-one")),
            TimelineMaterializeMode::FailIfDirty,
            10,
        )
        .expect("materialize timeline cursor in child");

    if phase == "recover" {
        assert_eq!(outcome.status, TimelineMaterializeStatus::AlreadyAtTarget);
        assert_eq!(
            outcome.recovery.status,
            TimelineMaterializationRecoveryStatus::CursorRecorded
        );
        assert!(
            outcome.recovery.cursor_operation_id.is_some(),
            "retry should record the pending cursor move"
        );
    }
}

struct Fixture {
    temp: TempDir,
    state1: ChangeId,
}

fn create_fixture() -> Fixture {
    let temp = TempDir::new().expect("temp repo");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    let store = TimelineStore::open(repo.heddle_dir()).expect("open timeline store");
    let state0 = repo.head().expect("initial head").expect("initial state");
    let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
    let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
    write_timeline(&store, state0, state1, state2);
    Fixture { temp, state1 }
}

fn run_child(repo_path: &Path, phase: &str, fault: Option<&str>) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("current test binary"));
    command
        .arg("--ignored")
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_REPO_ENV, repo_path)
        .env(CHILD_PHASE_ENV, phase)
        .env_remove("HEDDLE_FAULT_INJECT");
    if let Some(fault) = fault {
        command.env("HEDDLE_FAULT_INJECT", fault);
    }
    command.output().expect("spawn child test process")
}

fn assert_intentional_crash(output: Output, checkpoint: &str) {
    assert!(
        !output.status.success(),
        "child should panic, got success: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HEDDLE_FAULT_INJECT") && stderr.contains(checkpoint),
        "child should report the intentional panic at {checkpoint}: stderr={stderr}"
    );
}

fn step(id: &str) -> TimelineStepId {
    TimelineStepId::new(id)
}

fn branch(id: &str) -> TimelineBranchId {
    TimelineBranchId::new(id)
}

fn native(call: &str) -> NativeToolCallRefV1 {
    NativeToolCallRefV1 {
        harness: "opencode".to_string(),
        session_id: Some("session-1".to_string()),
        message_id: Some("message-1".to_string()),
        tool_call_id: call.to_string(),
    }
}

fn write_state(repo: &Repository, root: &Path, path: &str, content: &str) -> ChangeId {
    fs::write(root.join(path), content).expect("write fixture file");
    repo.snapshot(Some(path.to_string()), None)
        .expect("capture fixture state")
        .change_id
}

fn write_timeline(store: &TimelineStore, state0: ChangeId, state1: ChangeId, state2: ChangeId) {
    store
        .write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                parent_branch_id: None,
                from_step_id: None,
                from_state: state0,
                reason: TimelineBranchReason::ExplicitFork,
                created_at_ms: 1,
            }),
            Vec::new(),
        ))
        .expect("write branch operation");

    store
        .write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
                thread: "main".to_string(),
                step_id: step("tls-one"),
                branch_id: branch("tlb-main"),
                parent_step_id: None,
                native: native("call-1"),
                tool_name: "shell".to_string(),
                before_state: state0,
                payload: Some(TimelineToolPayloadMetadata {
                    summary: Some("write first version".to_string()),
                    hash: Some(ContentHash::compute_typed(
                        "timeline-tool-payload",
                        b"call-1",
                    )),
                }),
                started_at_ms: 2,
            }),
            Vec::new(),
        ))
        .expect("write first start operation");

    store
        .write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                thread: "main".to_string(),
                step_id: step("tls-one"),
                branch_id: branch("tlb-main"),
                native: native("call-1"),
                status: TimelineToolCallStatus::Succeeded,
                before_state: state0,
                after_state: state1,
                capture_state: Some(state1),
                capture_oplog_batch_id: Some(1),
                changed: true,
                touched_paths: vec!["tracked.txt".to_string()],
                payload: None,
                finished_at_ms: 3,
            }),
            Vec::new(),
        ))
        .expect("write first finish operation");

    store
        .write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
                thread: "main".to_string(),
                step_id: step("tls-two"),
                branch_id: branch("tlb-main"),
                parent_step_id: Some(step("tls-one")),
                native: native("call-2"),
                tool_name: "shell".to_string(),
                before_state: state1,
                payload: Some(TimelineToolPayloadMetadata {
                    summary: Some("write second version".to_string()),
                    hash: Some(ContentHash::compute_typed(
                        "timeline-tool-payload",
                        b"call-2",
                    )),
                }),
                started_at_ms: 4,
            }),
            Vec::new(),
        ))
        .expect("write second start operation");

    store
        .write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                thread: "main".to_string(),
                step_id: step("tls-two"),
                branch_id: branch("tlb-main"),
                native: native("call-2"),
                status: TimelineToolCallStatus::Succeeded,
                before_state: state1,
                after_state: state2,
                capture_state: Some(state2),
                capture_oplog_batch_id: Some(2),
                changed: true,
                touched_paths: vec!["tracked.txt".to_string()],
                payload: None,
                finished_at_ms: 5,
            }),
            Vec::new(),
        ))
        .expect("write second finish operation");
}
