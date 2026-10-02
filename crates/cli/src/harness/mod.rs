// SPDX-License-Identifier: Apache-2.0
//! CLI glue for the agent relay (`agent-relay` crate).
//!
//! The relay/probe machinery lives in `agent-relay` so the cli ↔ harness
//! dependency cycle stays cut: this module is the thin seam that adapts the
//! CLI-owned pieces the relay needs — snapshot capture and thread-checkout
//! materialization — behind the `HarnessCliBridge` port, and re-exports the
//! probe entry points so command call sites (and the hosted client's
//! startup-installed harness probe) don't churn.

use std::path::Path;

use agent_relay::{HarnessCliBridge, RelayCapture};
pub use agent_relay::{current_process_harness_hint, probe_current_process_harness};
use anyhow::{Context, Result};
use config::UserConfig;
use objects::object::StateId;
use repo::Repository;

pub(crate) fn relay_harness_event(
    repo: &Repository,
    harness: &str,
    event: &str,
    payload: &str,
) -> Result<()> {
    agent_relay::relay_harness_event(cli_bridge(), repo, harness, event, payload)
}

fn cli_bridge() -> std::sync::Arc<dyn HarnessCliBridge> {
    std::sync::Arc::new(CliAgentBridge)
}

struct CliAgentBridge;

impl HarnessCliBridge for CliAgentBridge {
    fn capture_snapshot(
        &self,
        repo: &Repository,
        user_config: &UserConfig,
        capture: RelayCapture,
    ) -> Result<String> {
        crate::cli::commands::snapshot::create_snapshot_with_agent_options(
            repo,
            user_config,
            Some(capture.intent),
            None,
            verbs::CaptureAgentOptions {
                environment_policy: std::env::var("HEDDLE_AGENT_POLICY").ok(),
                default_policy: user_config.agent.default_policy.clone(),
                ..Default::default()
            },
        )?;
        Ok(repo
            .head()?
            .context("captured state missing from HEAD")?
            .to_string_full())
    }

    fn prepare_worktree_target(
        &self,
        repo: &Repository,
        path: &Path,
        self_thread: Option<&str>,
    ) -> Result<std::path::PathBuf> {
        Ok(
            crate::cli::commands::worktree_cmd::helpers::prepare_worktree_target(
                repo,
                path,
                self_thread,
            )?
            .path,
        )
    }

    fn write_isolated_checkout(
        &self,
        repo: &Repository,
        path: &Path,
        base_state: &StateId,
        thread: Option<&str>,
    ) -> Result<()> {
        crate::cli::commands::worktree_cmd::helpers::write_isolated_checkout(
            repo, path, base_state, thread,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use objects::store::ObjectStore;
    use serde_json::Value;

    use super::*;

    // Capture-through-relay behavior: these exercise the real CLI capture
    // implementation behind the bridge, so they live here rather than in
    // agent-relay's unit suite.

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn relay_claude_stop_captures_state_with_agent_attribution() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();

        // Establish HEAD with an initial snapshot.
        std::fs::write(repo_root.join("seed.txt"), b"hello").unwrap();
        let _ = repo.snapshot(Some("seed".into()), None).unwrap();

        // Make a dirty change that the Stop hook should capture.
        std::fs::write(repo_root.join("seed.txt"), b"hello, heddle").unwrap();

        drop(repo);

        let fresh_repo = Repository::open(temp.path()).unwrap();
        let user_config = UserConfig {
            principal: Some(config::config::UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            ..UserConfig::default()
        };
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(fresh_repo, user_config, cli_bridge());
        let payload = serde_json::json!({
            "session_id": "claude-sess-123",
            "transcript_path": "/tmp/claude/x.jsonl",
            "model": {
                "id": "claude-opus-4-7",
                "display_name": "Claude Opus 4.7",
            },
            "last_assistant_message": "hook-driven capture test",
            "hook_event_name": "Stop",
        });
        runtime.relay("claude-code", "Stop", &payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let head_id = verify.head().unwrap().expect("HEAD after Stop capture");
        let state = verify
            .store()
            .get_state(&head_id)
            .unwrap()
            .expect("state for HEAD");
        let evidence = repo::load_attribution_evidence(verify.store(), &state)
            .unwrap()
            .unwrap();
        let model = evidence
            .selected
            .model
            .as_ref()
            .expect("selected model on state");
        assert_eq!(model.value, "claude-opus-4-7");
        assert_eq!(
            model.basis,
            objects::object::AttributionBasis::RequestReported
        );
        assert_eq!(
            model.source,
            objects::object::AttributionSource::HarnessHook
        );
        assert!(
            evidence.selected.provider.is_none(),
            "a Claude harness does not prove its backend provider"
        );
        assert!(evidence.response.model.is_none());
        assert_eq!(
            evidence.scope.harness_session_id.as_deref(),
            Some("claude-sess-123")
        );
        assert_ne!(
            evidence.scope.heddle_session_id,
            evidence.scope.harness_session_id
        );
        assert_eq!(
            state.intent.as_deref(),
            Some("hook-driven capture test"),
            "intent should be pulled from payload message",
        );
    }

    #[test]
    fn relay_opencode_tool_execute_after_captures_dirty_worktree() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("tracked.txt"), b"one\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        let user_config = UserConfig {
            principal: Some(config::config::UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            ..UserConfig::default()
        };
        let mut runtime = agent_relay::HarnessBridgeRuntime::new(repo, user_config, cli_bridge());
        let payload = opencode_tool_payload("call-2");

        runtime
            .relay("opencode", "tool.execute.before", &payload)
            .unwrap();
        std::fs::write(root.join("tracked.txt"), b"two\n").unwrap();
        runtime
            .relay("opencode", "tool.execute.after", &payload)
            .unwrap();

        let head = runtime.repo.head().unwrap().expect("capture advanced HEAD");
        assert_ne!(head, seed.state_id);
        let state = runtime.repo.store().get_state(&head).unwrap().unwrap();
        let evidence = repo::load_attribution_evidence(runtime.repo.store(), &state)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.selected.model.as_ref().unwrap().value, "gpt-5.4");
        assert_eq!(
            evidence.selected.model.as_ref().unwrap().basis,
            objects::object::AttributionBasis::RequestReported
        );
        assert_eq!(
            evidence.selected.model.as_ref().unwrap().source,
            objects::object::AttributionSource::HarnessHook
        );
        assert_eq!(
            evidence.scope.harness_session_id.as_deref(),
            Some("opencode-session")
        );
        let store = repo::TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = repo::TimelineView::rebuild(&store).unwrap();
        let steps = view.steps_for_thread("main");
        assert_eq!(steps.len(), 1, "before/after should merge by native id");
        let step = steps[0];
        assert_eq!(step.operation_ids.len(), 2);
        assert_eq!(
            step.status,
            Some(objects::object::TimelineToolCallStatus::Succeeded)
        );
        assert_eq!(step.before_state, Some(seed.state_id));
        assert_eq!(step.after_state, Some(head));
        assert_eq!(step.capture_state, Some(head));
        assert_eq!(step.changed, Some(true));
        assert!(step.touched_paths.contains(&"tracked.txt".to_string()));
        assert!(
            step.labels
                .contains(&objects::object::TimelineLabel::RepoReversible)
        );
        assert!(
            step.labels
                .contains(&objects::object::TimelineLabel::ExternalSideEffectsUnknown)
        );
        assert!(!step.payload_summary.as_deref().unwrap().contains("SECRET"));
        assert!(step.payload_hash.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn relay_opencode_tool_execute_after_records_capture_failed_without_ambient_paths() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("seed.txt"), b"seed\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(repo, UserConfig::default(), cli_bridge());
        let mut payload = opencode_tool_payload("call-capture-failed");
        payload["tool"]["input"]["file_path"] = serde_json::json!("hinted.txt");
        let hooks_dir = root.join(".heddle/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-snapshot");
        std::fs::write(&hook_path, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();

        runtime
            .relay("opencode", "tool.execute.before", &payload)
            .unwrap();
        std::fs::write(root.join("ambient.txt"), b"dirty but uncaptured\n").unwrap();
        runtime
            .relay("opencode", "tool.execute.after", &payload)
            .unwrap();

        assert_eq!(
            runtime.repo.head().unwrap(),
            Some(seed.state_id),
            "capture failure must not advance HEAD"
        );
        let store = repo::TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = repo::TimelineView::rebuild(&store).unwrap();
        let steps = view.steps_for_thread("main");
        assert_eq!(steps.len(), 1, "before/after should merge by native id");
        let step = steps[0];
        assert_eq!(step.operation_ids.len(), 2);
        assert_eq!(step.before_state, Some(seed.state_id));
        assert_eq!(step.after_state, Some(seed.state_id));
        assert_eq!(step.capture_state, None);
        assert_eq!(step.changed, Some(false));
        assert!(
            step.labels
                .contains(&objects::object::TimelineLabel::CaptureFailed)
        );
        assert!(
            !step
                .labels
                .contains(&objects::object::TimelineLabel::RepoReversible),
            "failed captures are not repo-reversible"
        );
        assert_eq!(step.touched_paths, vec!["hinted.txt"]);
    }

    #[test]
    fn relay_claude_subagent_stop_marks_child_entry_complete() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();
        drop(repo);

        // Start: create the child entry.
        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(fresh, UserConfig::default(), cli_bridge());
        let start_payload = serde_json::json!({
            "session_id": "parent-sess",
            "agent_id": "worker-1",
            "model": {"id": "claude-sonnet-4-6"},
        });
        runtime
            .relay("claude-code", "SubagentStart", &start_payload)
            .unwrap();
        drop(runtime);

        // Dirty the worktree so SubagentStop also captures a state.
        std::fs::write(
            repo_root.join("child-output.txt"),
            b"subagent produced this",
        )
        .unwrap();

        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(fresh, UserConfig::default(), cli_bridge());
        let stop_payload = serde_json::json!({
            "session_id": "parent-sess",
            "agent_id": "worker-1",
            "model": {
                "id": "claude-sonnet-4-6",
                "display_name": "Claude Sonnet 4.6",
            },
        });
        runtime
            .relay("claude-code", "SubagentStop", &stop_payload)
            .unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let registry = repo::ActorPresenceStore::new(verify.heddle_dir());
        let child = registry
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.native_actor_key.as_deref() == Some("claude-code:agent:worker-1"))
            .expect("child entry should still exist");
        assert_eq!(
            child.status,
            repo::ActorPresenceStatus::Complete,
            "SubagentStop should mark the child entry Complete",
        );
    }

    #[test]
    fn relay_claude_subagent_start_creates_child_entry_with_parent_key() {
        let (temp, repo) = init_repo();
        drop(repo);
        let fresh_repo = Repository::open(temp.path()).unwrap();
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(fresh_repo, UserConfig::default(), cli_bridge());
        let payload = serde_json::json!({
            "session_id": "parent-claude-sess",
            "agent_id": "child-subagent-xyz",
            "model": {"id": "claude-sonnet-4-6"},
        });
        runtime
            .relay("claude-code", "SubagentStart", &payload)
            .unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let registry = repo::ActorPresenceStore::new(verify.heddle_dir());
        let child = registry
            .find_active_by_native_actor_key("claude-code:agent:child-subagent-xyz")
            .unwrap()
            .expect("subagent ActorPresence should exist after SubagentStart");
        assert_eq!(
            child.native_parent_actor_key.as_deref(),
            Some("claude-code:session:parent-claude-sess"),
            "subagent must carry parent session linkage",
        );
        assert_eq!(child.status, repo::ActorPresenceStatus::Active);
    }

    #[test]
    fn opencode_child_session_creates_distinct_actor_with_parent_key() {
        let (temp, repo) = init_repo();
        drop(repo);

        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime =
            agent_relay::HarnessBridgeRuntime::new(fresh, UserConfig::default(), cli_bridge());
        let root_payload = serde_json::json!({"sessionID": "root-1"});
        runtime
            .relay("opencode", "tool.execute.before", &root_payload)
            .unwrap();
        let child_payload = serde_json::json!({"sessionID": "child-1", "parentID": "root-1"});
        runtime
            .relay("opencode", "tool.execute.before", &child_payload)
            .unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let registry = repo::ActorPresenceStore::new(verify.heddle_dir());
        let entries = registry.list().unwrap();
        let root = entries
            .iter()
            .find(|e| e.native_actor_key.as_deref() == Some("opencode:session:root-1"))
            .expect("root actor should exist");
        let child = entries
            .iter()
            .find(|e| e.native_actor_key.as_deref() == Some("opencode:session:child-1"))
            .expect("child actor should exist");
        assert_ne!(root.session_id, child.session_id);
        assert_eq!(
            child.native_parent_actor_key.as_deref(),
            Some("opencode:session:root-1")
        );
    }

    #[test]
    fn codex_operation_capture_keeps_each_producing_model_after_switch() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("file.txt"), "seed").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let user_config = UserConfig {
            principal: Some(config::config::UserPrincipalConfig {
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
            }),
            ..Default::default()
        };
        let mut runtime = agent_relay::HarnessBridgeRuntime::new(repo, user_config, cli_bridge());
        for (turn, model, contents) in [
            ("turn-one", "model-one", "one"),
            ("turn-two", "model-two", "two"),
        ] {
            let payload = serde_json::json!({
                "session_id": "codex-session", "turn_id": turn, "tool_use_id": turn,
                "model": model, "model_provider": "route", "tool_name": "apply_patch",
                "tool_input": {"command": "*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n+new\n*** End Patch"}
            });
            runtime.relay("codex", "PreToolUse", &payload).unwrap();
            std::fs::write(root.join("file.txt"), contents).unwrap();
            // A newer cursor must not be used to label the original completion.
            let mut later = payload.clone();
            later["model"] = serde_json::json!("later-model");
            later["turn_id"] = serde_json::json!("later-turn");
            verbs::stamp_identity_cursor(runtime.repo.root(), &verbs::codex_cursor_patch(&later))
                .unwrap();
            runtime.relay("codex", "PostToolUse", &payload).unwrap();
        }
        runtime
            .relay(
                "codex",
                "Stop",
                &serde_json::json!({
                    "session_id":"codex-session", "turn_id":"turn-three", "model":"latest-model",
                    "model_provider":"route", "last_assistant_message":"mixed capture"
                }),
            )
            .unwrap();
        let head = runtime.repo.head().unwrap().unwrap();
        let state = runtime.repo.store().get_state(&head).unwrap().unwrap();
        let evidence = repo::load_attribution_evidence(runtime.repo.store(), &state)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.operations.len(), 2);
        let models: Vec<_> = evidence
            .operations
            .iter()
            .map(|op| op.identity.selected.model.as_ref().unwrap().value.as_str())
            .collect();
        assert_eq!(models, ["model-one", "model-two"]);
        assert!(evidence.operations.iter().all(
            |op| op.resolution == objects::object::AttributionOperationResolution::ContentBound
        ));
        assert!(
            evidence
                .operations
                .iter()
                .all(|op| op.identity.response.model.is_none())
        );
        // Real source-only pack export/import preserves the committed record,
        // including both causal model observations and file transition hashes.
        let spool = tempfile::tempdir().unwrap();
        let index_path = spool.path().join("source.idx");
        let builder = objects::store::StreamingPackBuilder::new(
            std::io::Cursor::new(Vec::new()),
            index_path.clone(),
            objects::store::CompressionConfig::default(),
            spool.path().join("buckets"),
        )
        .unwrap();
        let (pack, _) = objects::store::pack::build_source_pack(
            builder,
            runtime.repo.store(),
            &state,
            100,
            1024 * 1024,
        )
        .unwrap();
        let reader = objects::store::PackReader::from_bytes(
            pack.into_inner(),
            std::fs::read(index_path).unwrap(),
        )
        .unwrap();
        reader
            .validate_source_closure(&state, 100, 1024 * 1024)
            .unwrap();
        let (_receiver_dir, receiver) = init_repo();
        reader
            .visit_objects(|_, kind, bytes| {
                use objects::store::pack::ObjectType;
                match kind {
                    ObjectType::State => {
                        receiver
                            .store()
                            .put_state(
                                &objects::object::State::decode_current_msgpack(bytes).unwrap(),
                            )
                            .unwrap();
                    }
                    ObjectType::Tree => {
                        receiver
                            .store()
                            .put_tree(&objects::object::Tree::decode_canonical(bytes).unwrap())
                            .unwrap();
                    }
                    ObjectType::Blob => {
                        receiver
                            .store()
                            .put_blob(&objects::object::Blob::new(bytes.to_vec()))
                            .unwrap();
                    }
                    _ => panic!("unexpected selected source object"),
                }
                Ok(())
            })
            .unwrap();
        let received_state = receiver.store().get_state(&state.id()).unwrap().unwrap();
        assert_eq!(received_state.id(), state.id());
        assert_eq!(
            received_state.attribution_evidence,
            state.attribution_evidence
        );
        let received = repo::load_attribution_evidence(receiver.store(), &received_state)
            .unwrap()
            .unwrap();
        assert_eq!(received, evidence);
        assert_eq!(
            received.operations[0].changes[0].after,
            received.operations[1].changes[0].before
        );
    }

    #[test]
    fn codex_rejected_patch_stays_unresolved_after_another_edit() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("file.txt"), "seed").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let config = UserConfig {
            principal: Some(config::config::UserPrincipalConfig {
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
            }),
            ..Default::default()
        };
        let mut runtime = agent_relay::HarnessBridgeRuntime::new(repo, config, cli_bridge());
        runtime.relay("codex", "PreToolUse", &serde_json::json!({
            "session_id":"session", "turn_id":"turn", "tool_use_id":"rejected-call",
            "model":"selected", "model_provider":"route", "tool_name":"apply_patch",
            "tool_input":{"command":"*** Begin Patch\n*** Update File: file.txt\n@@\n-missing-line\n+new\n*** End Patch"}
        })).unwrap();
        // Rejection has no successful PostToolUse. A later external write
        // must never be credited to the pending rejected invocation.
        std::fs::write(root.join("file.txt"), "somebody else's edit").unwrap();
        runtime.relay("codex", "Stop", &serde_json::json!({
            "session_id":"session", "turn_id":"turn", "model":"selected", "model_provider":"route"
        })).unwrap();
        let state = runtime
            .repo
            .store()
            .get_state(&runtime.repo.head().unwrap().unwrap())
            .unwrap()
            .unwrap();
        let evidence = repo::load_attribution_evidence(runtime.repo.store(), &state)
            .unwrap()
            .unwrap();
        assert!(evidence.operations_incomplete);
        assert_eq!(evidence.operations.len(), 1);
        assert_eq!(
            evidence.operations[0].resolution,
            objects::object::AttributionOperationResolution::Unresolved
        );
        assert!(evidence.operations[0].changes.is_empty());
    }

    fn opencode_tool_payload(call_id: &str) -> Value {
        serde_json::json!({
            "sessionID": "opencode-session",
            "messageID": "message-1",
            "toolCallID": call_id,
            "model": "gpt-5.4",
            "provider": "openai",
            "tool": {
                "name": "bash",
                "input": {
                    "command": "echo SECRET",
                    "file_path": "tracked.txt"
                }
            },
            "status": "success"
        })
    }
}
