//! Exact, single-invocation Claude permission requests, retained only locally.
use std::{
    io::Write,
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1 as v2;
use objects::object::ContentHash;
use repo::{Repository, device_runs::RunStore};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(45);

// Sorting explicitly keeps the digest stable even if serde_json's optional
// preserve_order feature is enabled elsewhere in the workspace.
fn sorted(value: Value, depth: usize) -> Result<Value> {
    if depth > 32 {
        bail!("permission input nesting exceeds limit");
    }
    Ok(match value {
        Value::Object(values) => {
            let entries = values
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>();
            let mut result = serde_json::Map::new();
            for (key, value) in entries {
                result.insert(key, sorted(value, depth + 1)?);
            }
            Value::Object(result)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| sorted(value, depth + 1))
                .collect::<Result<_>>()?,
        ),
        scalar => scalar,
    })
}

fn intent(run: &str, id: &str, payload: &Value) -> Result<(String, Vec<u8>, Vec<u8>)> {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .context("permission tool name required")?;
    if tool.is_empty() || tool.len() > 256 {
        bail!("invalid permission tool name");
    }
    let input = payload
        .get("tool_input")
        .filter(|value| value.is_object())
        .context("permission tool input required")?;
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .context("permission working directory required")?;
    let canonical = serde_json::to_vec(&sorted(
        json!({
            "format": "heddle.claude-permission.v1", "run_id": run,
            "permission_id": id, "tool_name": tool, "tool_input": input, "cwd": cwd,
        }),
        0,
    )?)?;
    if canonical.len() > 64 * 1024 {
        bail!("permission input exceeds 64 KiB");
    }
    let digest = ContentHash::compute_typed("heddle.claude-permission.v1", &canonical)
        .as_bytes()
        .to_vec();
    Ok((tool.into(), canonical, digest))
}

/// A missing remote answer preserves the harness's own permission flow. The
/// callback is synchronous because the harness is waiting for this hook's
/// stdout. Filesystem notifications wake the read; idle time runs no DB polls.
pub(crate) fn claude_permission(
    repo: &Repository,
    run: &str,
    payload: &Value,
    output: &mut impl Write,
) -> Result<()> {
    if hosted_client::network::running_device_node_id()?.is_none() {
        return Ok(());
    }
    permission_roundtrip(repo, run, payload, WAIT, output)
}

fn permission_roundtrip(
    repo: &Repository,
    run: &str,
    payload: &Value,
    wait: Duration,
    output: &mut impl Write,
) -> Result<()> {
    let store = RunStore::open(repo.heddle_dir())?;
    let id = uuid::Uuid::now_v7().to_string();
    let (tool, canonical, digest) = intent(run, &id, payload)?;
    let deadline = Instant::now() + wait;
    let expires = chrono::Utc::now() + chrono::Duration::seconds(wait.as_secs().max(1) as i64);
    let (tx, rx) = mpsc::sync_channel(1);
    let _watch = repo::device_watch::watch_filtered(
        repo.heddle_dir(),
        |path| {
            path.file_name()
                .is_some_and(|name| name == "device-runs.sqlite3.changed")
        },
        move |event| {
            let _ = tx.try_send(event);
        },
    )?;
    store.request_permission_record(
        run,
        &v2::RunPermission {
            id: id.clone(),
            request_digest: digest.clone(),
            harness: "claude-code".into(),
            tool_name: tool,
            canonical_input_json: canonical,
            expires_at: Some(prost_types::Timestamp {
                seconds: expires.timestamp(),
                nanos: 0,
            }),
        },
    )?;
    let result = (|| -> Result<()> {
        loop {
            if let Some(allow) = store.permission_decision(run, &id, &digest)? {
                serde_json::to_writer(
                    &mut *output,
                    &json!({"hookSpecificOutput": {
                        "hookEventName": "PermissionRequest", "decision": {
                            "behavior": if allow { "allow" } else { "deny" },
                        }
                    }}),
                )?;
                output.write_all(b"\n")?;
                output.flush()?;
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match rx.recv_timeout(remaining) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => bail!("permission observation failed: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("permission observation closed"),
            }
        }
    })();
    // The hook invocation has ended even when stdout fails. Never let a later
    // approval of this record authorize another invocation of the same tool.
    let closed = store.close_permission(run, &id, &digest);
    result?;
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Repository, RunStore) {
        let dir = tempfile::tempdir().expect("repository directory");
        let repo = Repository::init_default(dir.path()).expect("repository");
        let store = RunStore::open(repo.heddle_dir()).expect("store");
        store
            .put_run(v2::RunRecord {
                r#ref: Some(v2::RecordRef {
                    id: "run-a".into(),
                    spool: None,
                }),
                state: v2::operation_record::State::Running as i32,
                harness: "claude-code".into(),
                ..Default::default()
            })
            .expect("run");
        (dir, repo, store)
    }

    #[test]
    fn explicit_decision_wakes_hook_and_retires_exact_invocation() {
        let (_dir, repo, store) = fixture();
        let home = repo.heddle_dir().to_owned();
        let (ready_tx, ready_rx) = mpsc::channel();
        let consumer = std::thread::spawn(move || {
            let remote = RunStore::open(&home).expect("remote store");
            let (tx, rx) = mpsc::sync_channel(1);
            let _watch = repo::device_watch::watch(&home, move |event| {
                let _ = tx.try_send(event);
            })
            .expect("watch");
            ready_tx.send(()).expect("ready");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let run = remote.run("run-a").expect("read run").expect("run");
                if let Some(permission) = run.pending_permissions.first() {
                    let intent: Value = serde_json::from_slice(&permission.canonical_input_json)
                        .expect("intent JSON");
                    assert_eq!(intent["tool_input"]["path"], "src/lib.rs");
                    let decision = v2::DecideRunPermissionRequest {
                        client_operation_id: "decide-a".into(),
                        run: run.r#ref,
                        permission_id: permission.id.clone(),
                        request_digest: permission.request_digest.clone(),
                        allow: true,
                    };
                    remote
                        .decide_permission(&decision, "owner")
                        .expect("explicit decision");
                    return decision;
                }
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("permission notification")
                    .expect("watch event");
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("consumer ready");
        let mut output = Vec::new();
        permission_roundtrip(
            &repo,
            "run-a",
            &json!({"tool_name":"Read", "tool_input":{"path":"src/lib.rs"}, "cwd":"/repo"}),
            Duration::from_secs(5),
            &mut output,
        )
        .expect("hook");
        let mut decision = consumer.join().expect("consumer");
        let response: Value = serde_json::from_slice(&output).expect("hook JSON");
        assert_eq!(
            response["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert!(
            store
                .run("run-a")
                .expect("run")
                .expect("run")
                .pending_permissions
                .is_empty()
        );
        assert_eq!(
            store
                .permission_decision("run-a", &decision.permission_id, &decision.request_digest)
                .expect("closed"),
            None
        );
        decision.client_operation_id = "later-decision".into();
        assert!(
            store.decide_permission(&decision, "owner").is_err(),
            "completed invocation cannot be approved again"
        );
    }

    #[test]
    fn unanswered_permission_returns_to_harness_without_allowing() {
        let (_dir, repo, store) = fixture();
        let mut output = Vec::new();
        permission_roundtrip(
            &repo,
            "run-a",
            &json!({"tool_name":"Read", "tool_input":{"path":"src/lib.rs"}, "cwd":"/repo"}),
            Duration::from_millis(80),
            &mut output,
        )
        .expect("timeout");
        assert!(
            output.is_empty(),
            "timeout must not invent a permission decision"
        );
        assert!(
            store
                .run("run-a")
                .expect("run")
                .expect("run")
                .pending_permissions
                .is_empty(),
            "expired invocation retired"
        );
    }

    #[test]
    fn permission_digest_binds_invocation_and_exact_input() {
        let payload = json!({"tool_name":"Read", "tool_input":{"path":"src/lib.rs", "offset":1}, "cwd":"/repo"});
        let (_, canonical, digest) = intent("run-a", "request-a", &payload).expect("intent");
        let reordered: Value = serde_json::from_str(
            r#"{"cwd":"/repo","tool_input":{"offset":1,"path":"src/lib.rs"},"tool_name":"Read"}"#,
        )
        .expect("json");
        assert_eq!(
            intent("run-a", "request-a", &reordered).expect("intent").1,
            canonical
        );
        assert_ne!(
            intent("run-b", "request-a", &payload).expect("intent").2,
            digest
        );
        assert_ne!(
            intent("run-a", "request-b", &payload).expect("intent").2,
            digest
        );
        let mut changed = payload;
        changed["tool_input"]["offset"] = json!(2);
        assert_ne!(
            intent("run-a", "request-a", &changed).expect("intent").2,
            digest
        );
    }
    #[test]
    fn permission_input_is_bounded() {
        assert!(
            intent(
                "run",
                "request",
                &json!({"tool_name":"Read", "tool_input":{"path":"x".repeat(65536)}, "cwd":"/repo"})
            )
            .is_err()
        );
        assert!(
            intent(
                "run",
                "request",
                &json!({"tool_name":"Read", "tool_input":null, "cwd":"/repo"})
            )
            .is_err()
        );
    }
}
