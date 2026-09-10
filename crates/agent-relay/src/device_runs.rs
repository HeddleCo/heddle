//! Project real harness sessions and deliver supported controls at hook edges.
//! Claude output semantics: https://code.claude.com/docs/en/hooks#json-output
use anyhow::{Context, Result};
use api::heddle::api::v2alpha1 as v2;
use objects::object::ContentHash;
use repo::{
    ActorPresenceStatus, Repository, device_runs::RunStore,
    thread_replication::checkout::ThreadCheckout,
};
use serde_json::{Value, json};
use wire::SessionReportEnvelope;

// A display name is not an account identifier. Unenrolled local sessions have
// no hosted principal yet; enrollment supplies the independently verified pin.
fn admitted_principal_id() -> Result<String> {
    let home = repo::identity::heddle_home_dir();
    if !home.join("state/device-rpc/authority.bin").try_exists()? {
        return Ok(String::new());
    }
    let authority = repo::device_authority::load(&home, chrono::Utc::now().timestamp())?;
    Ok(authority
        .owner
        .owner
        .context("admitted account missing")?
        .id)
}

pub(crate) fn publish(
    repo: &Repository,
    report: &SessionReportEnvelope,
    status: &ActorPresenceStatus,
) -> Result<()> {
    let spool = v2::SpoolRef {
        id: repo.native_spool_id()?.to_string(),
    };
    let store = RunStore::open(repo.heddle_dir())?;
    let reference = v2::RecordRef {
        spool: Some(spool.clone()),
        id: report.heddle_session_id.clone(),
    };
    let checkout = if repo
        .root()
        .join(".heddle/thread-checkout.json")
        .try_exists()?
    {
        Some(ThreadCheckout::open(repo.root())?)
    } else {
        None
    };
    let thread = checkout
        .as_ref()
        .map(|checkout| checkout.binding.thread)
        .or_else(|| {
            report
                .thread_id
                .as_ref()
                .and_then(|id| ContentHash::from_hex(id).ok())
        })
        .map(|id| v2::ThreadRef {
            spool: Some(spool.clone()),
            id: Some(v2::ThreadId {
                value: id.as_bytes().to_vec(),
            }),
        });
    let state = match status {
        ActorPresenceStatus::Active => v2::operation_record::State::Running,
        ActorPresenceStatus::Complete | ActorPresenceStatus::Merged => {
            v2::operation_record::State::Completed
        }
        ActorPresenceStatus::Abandoned => v2::operation_record::State::Canceled,
    };
    let harness = report.harness.harness.clone().unwrap_or_default();
    let supported_controls = if harness == "claude-code" && *status == ActorPresenceStatus::Active {
        vec![
            v2::control_run_request::Action::Stop as i32,
            v2::control_run_request::Action::Steer as i32,
        ]
    } else {
        Vec::new()
    };
    let record = v2::RunRecord {
        r#ref: Some(reference),
        thread,
        checkout: match (&checkout, hosted_client::network::persisted_node_id()?) {
            (Some(checkout), Some(endpoint)) => Some(v2::CheckoutRef {
                spool: Some(spool.clone()),
                device: Some(v2::EndpointRef {
                    public_key: endpoint.as_bytes().to_vec(),
                    kind: v2::EndpointKind::Device as i32,
                }),
                id: checkout.binding.id.clone(),
            }),
            _ => None,
        },
        principal_id: admitted_principal_id()?,
        agent_id: report.agent_session_id.clone().unwrap_or_default(),
        harness,
        model: report.harness.model.clone().unwrap_or_default(),
        state: state as i32,
        supported_controls,
        ..Default::default()
    };
    // Identical report flushes should not wake every device observation again.
    let mut previous = store.run(&report.heddle_session_id)?;
    if let Some(previous) = previous.as_mut() {
        previous.version.clear();
        previous.pending_permissions.clear();
        previous.artifacts.clear();
    }
    if previous.as_ref() != Some(&record) {
        store.put_run(record)?;
    }
    if report.harness.harness.as_deref() == Some("claude-code")
        && let Some(key) = report.native_actor_key.as_deref()
    {
        store.bind_harness(
            key,
            &report.heddle_session_id,
            *status == ActorPresenceStatus::Active,
        )?;
    }
    let last = store.last_timeline_position(&report.heddle_session_id)?;
    let next = last.map_or(0, |position| position.saturating_add(1));
    let run_ref = v2::RecordRef {
        spool: Some(spool.clone()),
        id: report.heddle_session_id.clone(),
    };
    let emit = |position: u64, kind: &str, summary: String| -> Result<()> {
        store.put_timeline(&v2::TimelineRecord {
            r#ref: Some(v2::RecordRef {
                spool: Some(spool.clone()),
                id: format!("{}:{position}", report.heddle_session_id),
            }),
            run: Some(run_ref.clone()),
            position,
            kind: kind.into(),
            summary,
            ..Default::default()
        })
    };
    if next == 0 {
        emit(0, "session_opened", "Harness session opened".into())?;
    }
    // Skip directly to the first unseen checkpoint. A report flush never
    // rewrites the retained history and never republishes transcript contents.
    for (index, checkpoint) in report
        .progress
        .iter()
        .enumerate()
        .skip(next.saturating_sub(1) as usize)
    {
        let kind = checkpoint.status.as_deref().unwrap_or("progress");
        let summary = format!("{}: {} paths", kind, checkpoint.touched_paths.len());
        emit(index as u64 + 1, kind, summary)?;
    }
    let closing = report.progress.len() as u64 + 1;
    if report.closed_at.is_some() && closing >= next {
        emit(closing, "session_closed", "Harness session closed".into())?;
    }
    if report.closed_at.is_some() {
        retain_final_report(repo, &run_ref, report)?;
    }
    Ok(())
}

/// Preserve the harness report only under explicit raw-retention policy. Its
/// transcript locators remain data: they never authorize opening local paths.
fn retain_final_report(
    repo: &Repository,
    run: &v2::RecordRef,
    report: &impl serde::Serialize,
) -> Result<()> {
    let spool = run
        .spool
        .as_ref()
        .context("retained report requires Spool")?;
    let store = RunStore::open(repo.heddle_dir())?;
    let Some(policy) = store.policy(&spool.id)? else {
        return Ok(());
    };
    if !policy.retain_raw || policy.raw_retention_seconds == 0 {
        return Ok(());
    }
    // ArtifactStore repeats policy and exact Run authorization transactionally.
    // This preflight avoids serializing raw material when retention is disabled.
    let bytes = serde_json::to_vec(report)?;
    repo::device_artifacts::ArtifactStore::open(repo.heddle_dir())?.retain(
        run,
        "session-report",
        "application/json",
        &bytes,
        chrono::Utc::now().timestamp(),
    )?;
    Ok(())
}

/// Emit one combined hook result. Delivery acknowledges the control request,
/// while the Run state continues to come from actual session lifecycle events.
/// Never infer process termination from successfully writing stdout.
pub(crate) fn claude_controls(
    repo: &Repository,
    run_id: &str,
    event: &str,
    additional_context: impl FnOnce() -> Result<Option<String>>,
    output: &mut impl std::io::Write,
) -> Result<bool> {
    if !matches!(event, "PreToolUse" | "UserPromptSubmit") {
        return Ok(false);
    }
    let Some(store) = RunStore::open_existing(repo.heddle_dir())? else {
        return Ok(false);
    };
    let controls = store.pending_controls(run_id, 32)?;
    if controls.is_empty() {
        return Ok(false);
    }
    store.run(run_id)?.context("controlled run missing")?;
    let mut context = additional_context()?.unwrap_or_default();
    let mut stop = false;
    let mut delivered = Vec::new();
    for control in controls {
        match v2::control_run_request::Action::try_from(control.action) {
            Ok(v2::control_run_request::Action::Steer) => {
                if context
                    .len()
                    .saturating_add(control.instruction.len())
                    .saturating_add(2)
                    > 128 * 1024
                {
                    break;
                }
                if !context.is_empty() {
                    context.push_str("\n\n");
                }
                context.push_str(&control.instruction);
                delivered.push(control.id);
            }
            Ok(v2::control_run_request::Action::Stop) => {
                stop = true;
                delivered.push(control.id);
                break;
            }
            _ => break,
        }
    }
    if delivered.is_empty() {
        return Ok(false);
    }
    let mut response = json!({"hookSpecificOutput": {
        "hookEventName": event, "additionalContext": context,
    }});
    if stop {
        response["continue"] = Value::Bool(false);
        response["stopReason"] =
            Value::String("Stopped by an authorized Heddle run control.".into());
    }
    serde_json::to_writer(&mut *output, &response)?;
    output.write_all(b"\n")?;
    output.flush()?;
    for id in delivered {
        store.acknowledge_control_delivery(&id)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_report_artifacts_require_opt_in_and_preserve_exact_retry() {
        use std::io::Read;
        let directory = tempfile::tempdir().expect("repository");
        let repo = Repository::init_default(directory.path()).expect("repository");
        let runs = RunStore::open(repo.heddle_dir()).expect("runs");
        let reference = v2::RecordRef {
            spool: Some(v2::SpoolRef {
                id: uuid::Uuid::now_v7().to_string(),
            }),
            id: uuid::Uuid::now_v7().to_string(),
        };
        runs.put_run(v2::RunRecord {
            r#ref: Some(reference.clone()),
            ..Default::default()
        })
        .expect("persist independently bound Run");
        let report = json!({"closed": true, "transcript_refs": [{"path": "/must/not/be/opened"}]});
        retain_final_report(&repo, &reference, &report)
            .expect("disabled retention skips raw bytes");
        assert!(!repo.heddle_dir().join("retained-artifacts").exists());
        runs.put_policy(
            &v2::PutRunPolicyRequest {
                client_operation_id: uuid::Uuid::now_v7().to_string(),
                policy: Some(v2::RunPolicy {
                    spool: reference.spool.clone(),
                    retain_raw: true,
                    raw_retention_seconds: 60,
                    ..Default::default()
                }),
                ..Default::default()
            },
            "owning-human",
        )
        .expect("explicit retention opt-in");
        retain_final_report(&repo, &reference, &report).expect("retained report");
        let artifacts =
            repo::device_artifacts::ArtifactStore::open(repo.heddle_dir()).expect("catalog");
        let first = artifacts
            .for_run(&reference, chrono::Utc::now().timestamp())
            .expect("discovery");
        assert_eq!(first.len(), 1);
        retain_final_report(&repo, &reference, &report).expect("exact producer retry");
        assert_eq!(
            artifacts
                .for_run(&reference, chrono::Utc::now().timestamp())
                .expect("discovery"),
            first
        );
        let mut retained = artifacts
            .read(
                first[0].r#ref.as_ref().expect("artifact ref"),
                chrono::Utc::now().timestamp(),
            )
            .expect("authorized retained bytes");
        let mut bytes = Vec::new();
        retained.file.read_to_end(&mut bytes).expect("read bytes");
        assert_eq!(
            bytes,
            serde_json::to_vec(&report).expect("original report bytes")
        );
    }

    fn fixture() -> (tempfile::TempDir, Repository, RunStore, v2::RunRecord) {
        let directory = tempfile::tempdir().expect("repository directory");
        let repo = Repository::init_default(directory.path()).expect("initialize repository");
        let store = RunStore::open(repo.heddle_dir()).expect("run store");
        let run = store
            .put_run(v2::RunRecord {
                r#ref: Some(v2::RecordRef {
                    id: "run-1".into(),
                    spool: None,
                }),
                harness: "claude-code".into(),
                state: v2::operation_record::State::Running as i32,
                supported_controls: vec![
                    v2::control_run_request::Action::Steer as i32,
                    v2::control_run_request::Action::Stop as i32,
                ],
                ..Default::default()
            })
            .expect("active run");
        (directory, repo, store, run)
    }

    fn enqueue(
        store: &RunStore,
        run: &v2::RunRecord,
        id: &str,
        action: v2::control_run_request::Action,
    ) {
        store
            .enqueue_control(
                &v2::ControlRunRequest {
                    client_operation_id: id.into(),
                    run: run.r#ref.clone(),
                    expected_version: run.version.clone(),
                    action: action as i32,
                    instruction: "Review the capture before landing.".into(),
                },
                "human-account",
            )
            .expect("enqueue control");
    }

    #[test]
    fn tool_edge_selects_the_exact_agent_without_rewriting_session_progress() {
        let (_directory, repo, store, run) = fixture();
        store
            .bind_harness("claude-code:session:parent", "run-1", true)
            .expect("bind parent");
        enqueue(
            &store,
            &run,
            "stop-parent",
            v2::control_run_request::Action::Stop,
        );
        let mut output = Vec::new();
        claude_tool_edge(
            &repo,
            "PreToolUse",
            &json!({"session_id":"parent","agent_id":"other","tool_name":"Bash","tool_input":{}}),
            &mut output,
        )
        .expect("other agent");
        assert!(output.is_empty());
        assert_eq!(
            store
                .pending_controls("run-1", 32)
                .expect("pending parent")
                .len(),
            1
        );
        claude_tool_edge(
            &repo,
            "PreToolUse",
            &json!({"session_id":"parent","tool_name":"Bash","tool_input":{}}),
            &mut output,
        )
        .expect("parent edge");
        let response: Value = serde_json::from_slice(&output).expect("stop response");
        assert_eq!(response["continue"], false);
        assert!(
            store
                .pending_controls("run-1", 32)
                .expect("delivered")
                .is_empty()
        );
        assert_eq!(
            store.last_timeline_position("run-1").expect("timeline"),
            None,
            "a tool boundary must not manufacture progress records"
        );
    }

    #[test]
    fn hook_delivery_preserves_annotation_context_and_observed_run_state() {
        let (_directory, repo, store, run) = fixture();
        enqueue(
            &store,
            &run,
            "z-steer",
            v2::control_run_request::Action::Steer,
        );
        enqueue(
            &store,
            &run,
            "a-stop",
            v2::control_run_request::Action::Stop,
        );
        let mut output = Vec::new();
        assert!(
            claude_controls(
                &repo,
                "run-1",
                "PreToolUse",
                || Ok(Some("Existing invariant".into())),
                &mut output
            )
            .expect("deliver")
        );
        let response: Value = serde_json::from_slice(&output).expect("single hook response");
        assert_eq!(response["continue"], false);
        assert_eq!(
            response["hookSpecificOutput"]["additionalContext"],
            "Existing invariant\n\nReview the capture before landing."
        );
        assert!(
            store
                .pending_controls("run-1", 32)
                .expect("pending")
                .is_empty()
        );
        assert_eq!(
            store.run("run-1").expect("read").expect("run").state,
            run.state,
            "hook output is not process-termination evidence"
        );
    }

    #[test]
    fn failed_hook_delivery_does_not_acknowledge_control() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "harness disconnected",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (_directory, repo, store, run) = fixture();
        enqueue(&store, &run, "stop", v2::control_run_request::Action::Stop);
        assert!(claude_controls(&repo, "run-1", "PreToolUse", || Ok(None), &mut Broken).is_err());
        assert_eq!(
            store.pending_controls("run-1", 32).expect("pending").len(),
            1
        );
        assert_eq!(
            store.run("run-1").expect("read").expect("run").state,
            run.state
        );
    }
}

/// A tool-edge hook reads the indexed binding and pending command queue only.
/// It does not reopen the session, scan actor files, or rewrite its progress.
pub(crate) fn claude_tool_edge(
    repo: &Repository,
    event: &str,
    payload: &Value,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let key = crate::probe::claude_actor_key(
        payload.get("session_id").and_then(Value::as_str),
        payload.get("agent_id").and_then(Value::as_str),
    );
    let run = match (key, RunStore::open_existing(repo.heddle_dir())?) {
        (Some(key), Some(store)) => store.harness_run(&key)?,
        _ => None,
    };
    if event == "PermissionRequest" {
        if let Some(run) = run {
            crate::run_permissions::claude_permission(repo, &run, payload, output)?;
        }
        return Ok(());
    }
    if let Some(run) = run {
        if claude_controls(
            repo,
            &run,
            event,
            || crate::claude_hook::pre_tool_use_context(repo, payload),
            output,
        )? {
            return Ok(());
        }
    }
    if let Some(context) = crate::claude_hook::pre_tool_use_context(repo, payload)? {
        serde_json::to_writer(
            &mut *output,
            &json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":context}}),
        )?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
