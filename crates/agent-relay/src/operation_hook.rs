// SPDX-License-Identifier: Apache-2.0
//! Producing-operation observations from synchronous, typed harness events.
//! Never consults the mutable current cursor or parses shell commands.
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
};
use verbs::{IdentityCursor, OperationEventPhase, first_value_string, value_string};

pub fn record_harness_operation(
    root: &Path,
    harness: &str,
    event: Option<&str>,
    payload: &Value,
) -> io::Result<()> {
    let event = event.map(str::to_owned).or_else(|| {
        first_value_string(
            payload,
            &[
                &["hook_event_name"],
                &["hook_event"],
                &["heddle_hook_event"],
                &["event", "type"],
            ],
        )
    });
    let phase = match (harness, event.as_deref()) {
        ("codex" | "claude-code" | "claude", Some("PreToolUse"))
        | ("opencode", Some("tool.execute.before")) => OperationEventPhase::Before,
        // Codex's native apply_patch handler emits its PostToolUse payload
        // only after execute_verified_patch succeeds; Bash is still opaque.
        // Upstream openai/codex main, inspected 2026-10-02:
        // core/src/tools/registry.rs and core/src/tools/handlers/apply_patch.rs.
        ("codex" | "claude-code" | "claude", Some("PostToolUse"))
        | ("opencode", Some("tool.execute.after")) => OperationEventPhase::After,
        ("claude-code" | "claude", Some("PostToolUseFailure")) => OperationEventPhase::Failed,
        _ => return Ok(()),
    };
    let patch = operation_cursor(harness, payload);
    let paths = mutation_paths(root, harness, payload);
    verbs::record_operation_event(root, &patch, phase, &paths)?;
    if payload.get("heddle_model_source").and_then(Value::as_str) == Some("assistant_message")
        && let Some(identity) = patch.attribution_evidence.filter(|identity| {
            let exact_message = if harness == "opencode" {
                identity.scope.message_id.is_some()
            } else if matches!(harness, "claude-code" | "claude") {
                payload.get("type").and_then(Value::as_str) == Some("assistant")
                    && identity.scope.response_id.is_some()
                    && identity.response.model.is_some()
            } else {
                false
            };
            exact_message && identity.validate().is_ok()
        })
    {
        // The same causal observation was joined from an assistant event and
        // observed at a tool hook. Preserve both collector origins.
        verbs::record_attribution_observation(
            root,
            &verbs::AttributionObservation {
                method: objects::object::AttributionCollectionMethod::EventStream,
                identity,
                phase,
                paths,
            },
        )?;
    }
    Ok(())
}

fn operation_cursor(harness: &str, payload: &Value) -> IdentityCursor {
    let mut patch = match harness {
        "codex" => verbs::codex_cursor_patch(payload),
        "claude-code" | "claude" => verbs::claude_cursor_patch(payload),
        "opencode" => verbs::opencode_cursor_patch(payload),
        _ => return IdentityCursor::default(),
    };
    if harness == "opencode"
        && let Some(evidence) = patch.attribution_evidence.as_mut()
    {
        // Known plugin SDK spellings only. A generic `id` is not a tool ID.
        evidence.scope.tool_call_id = first_value_string(
            payload,
            &[
                &["callID"],
                &["toolCallID"],
                &["tool_call_id"],
                &["call_id"],
            ],
        )
        .filter(|id| verbs::published_field(Some(id)).is_some());
        evidence.scope.message_id = first_value_string(payload, &[&["messageID"], &["message_id"]])
            .filter(|id| verbs::published_field(Some(id)).is_some());
        if evidence.scope.message_id.is_some()
            && payload.get("heddle_model_source").and_then(Value::as_str)
                == Some("assistant_message")
        {
            for claim in [
                &mut evidence.selected.provider,
                &mut evidence.selected.model,
                &mut evidence.selected.thought_level,
            ]
            .into_iter()
            .flatten()
            {
                claim.source = objects::object::AttributionSource::Request;
                claim.basis = objects::object::AttributionBasis::RequestReported;
            }
        }
    }
    patch
}

fn mutation_paths(root: &Path, harness: &str, payload: &Value) -> Vec<PathBuf> {
    let tool = first_value_string(payload, &[&["tool_name"], &["tool", "name"], &["tool"]]);
    let input = payload
        .get("tool_input")
        .or_else(|| payload.get("args"))
        .or_else(|| payload.pointer("/tool/input"));
    let Some(input) = input else {
        return Vec::new();
    };
    let paths = match (harness, tool.as_deref()) {
        ("claude-code" | "claude", Some("Write" | "Edit" | "MultiEdit")) => {
            value_string(input, &["file_path"]).into_iter().collect()
        }
        ("claude-code" | "claude", Some("NotebookEdit")) => value_string(input, &["notebook_path"])
            .into_iter()
            .collect(),
        ("opencode", Some("write" | "edit" | "multiedit")) => {
            value_string(input, &["filePath"]).into_iter().collect()
        }
        ("opencode", Some("apply_patch")) => value_string(input, &["patchText"])
            .map(|patch| patch_paths(&patch))
            .unwrap_or_default(),
        ("codex", Some("apply_patch")) => value_string(input, &["command"])
            .map(|patch| patch_paths(&patch))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let cwd = match value_string(payload, &["cwd"]).map(PathBuf::from) {
        Some(path) if path.is_absolute() && path.starts_with(root) => Some(path),
        Some(_) => return Vec::new(),
        None => None,
    };
    paths
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.as_deref().unwrap_or(root).join(path)
            }
        })
        .collect()
}

/// Decode only apply_patch's framed path declarations, never shell/heredoc text.
fn patch_paths(patch: &str) -> Vec<String> {
    let lines: Vec<_> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Vec::new();
    }
    let mut paths = Vec::new();
    let mut file = false;
    for line in &lines[1..lines.len() - 1] {
        if let Some(path) = [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        {
            if path.trim().is_empty() || path.contains('\0') {
                return Vec::new();
            }
            paths.push(path.to_string());
            file = true;
        } else if !file
            || !(line.starts_with(['+', '-', ' '])
                || line.starts_with("@@")
                || *line == "*** End of File")
        {
            return Vec::new();
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn replay_index_symlink_remains_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".heddle")).unwrap();
        let target = root.join("untouched.txt");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(
            &target,
            root.join(".heddle/operations.operations-seen.sqlite3"),
        )
        .unwrap();
        let payload = serde_json::json!({
            "session_id":"session", "prompt_id":"turn", "tool_use_id":"call",
            "tool_name":"Edit", "tool_input":{"file_path":"file.txt"}
        });
        assert!(
            record_harness_operation(root, "claude-code", Some("PreToolUse"), &payload).is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn exact_claude_response_join_preserves_model_basis_and_collector_origins() {
        for has_response_id in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::create_dir(root.join(".heddle")).unwrap();
            std::fs::write(root.join("file.txt"), "before").unwrap();
            let mut payload = serde_json::json!({
                "session_id":"session", "prompt_id":"turn", "tool_use_id":"call",
                "tool_name":"Edit", "tool_input":{"file_path":"file.txt"},
                "type":"assistant", "message":{"id":"response", "model":"actual-response-model"},
                "request_id":"request", "heddle_model_source":"assistant_message"
            });
            if !has_response_id {
                payload["message"].as_object_mut().unwrap().remove("id");
            }
            record_harness_operation(root, "claude-code", Some("PreToolUse"), &payload).unwrap();
            std::fs::write(root.join("file.txt"), "after").unwrap();
            record_harness_operation(root, "claude-code", Some("PostToolUse"), &payload).unwrap();
            let journal: Value =
                serde_json::from_slice(&std::fs::read(root.join(".heddle/operations")).unwrap())
                    .unwrap();
            let operation = &journal["completed"][0][1];
            assert_eq!(operation["resolution"], "content_bound");
            assert!(operation["identity"]["selected"]["model"].is_null());
            assert_eq!(
                operation["identity"]["response"]["model"]["value"],
                "actual-response-model"
            );
            assert_eq!(
                operation["identity"]["response"]["model"]["basis"],
                "response_reported"
            );
            assert_eq!(operation["identity"]["scope"]["request_id"], "request");
            let expected = if has_response_id {
                serde_json::json!(["hook", "event_stream"])
            } else {
                serde_json::json!(["hook"])
            };
            assert_eq!(operation["identity"]["collection_methods"], expected);
        }
    }

    #[test]
    fn delayed_completion_freezes_before_identity_and_duplicate_hooks_are_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".heddle")).unwrap();
        std::fs::write(root.join("file.txt"), "before").unwrap();
        let payload = serde_json::json!({
            "session_id":"session", "turn_id":"turn", "tool_use_id":"call",
            "model":"producing-model", "provider":"route", "tool_name":"Edit",
            "tool_input":{"file_path":"file.txt"}
        });
        record_harness_operation(root, "claude-code", Some("PreToolUse"), &payload).unwrap();
        record_harness_operation(root, "claude-code", Some("PreToolUse"), &payload).unwrap();
        std::fs::write(root.join("file.txt"), "after").unwrap();
        let mut later = payload.clone();
        later["model"] = serde_json::json!("later-model");
        later["turn_id"] = serde_json::json!("later-turn");
        verbs::stamp_identity_cursor(root, &verbs::claude_cursor_patch(&later)).unwrap();
        record_harness_operation(root, "claude-code", Some("PostToolUse"), &payload).unwrap();
        record_harness_operation(root, "claude-code", Some("PostToolUse"), &payload).unwrap();
        let journal: Value =
            serde_json::from_slice(&std::fs::read(root.join(".heddle/operations")).unwrap())
                .unwrap();
        let operations = journal["completed"].as_array().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(
            operations[0][1]["identity"]["selected"]["model"]["value"],
            "producing-model"
        );
        assert_eq!(operations[0][1]["resolution"], "content_bound");
        assert_eq!(operations[0][1]["changes"][0]["path"], "file.txt");
    }

    #[test]
    fn exact_opencode_message_join_keeps_hook_and_event_stream_origins() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".heddle")).unwrap();
        std::fs::write(root.join("file.txt"), "before").unwrap();
        let payload = serde_json::json!({
            "sessionID":"session", "callID":"call", "messageID":"message", "tool":"write",
            "model":"selected", "provider":"route", "heddle_model_source":"assistant_message",
            "args":{"filePath":"file.txt"}
        });
        record_harness_operation(root, "opencode", Some("tool.execute.before"), &payload).unwrap();
        std::fs::write(root.join("file.txt"), "after").unwrap();
        record_harness_operation(root, "opencode", Some("tool.execute.after"), &payload).unwrap();
        let journal: Value =
            serde_json::from_slice(&std::fs::read(root.join(".heddle/operations")).unwrap())
                .unwrap();
        let operation = &journal["completed"][0][1];
        assert_eq!(
            operation["identity"]["collection_methods"],
            serde_json::json!(["hook", "event_stream"])
        );
        assert_eq!(
            operation["identity"]["selected"]["model"]["source"],
            "request"
        );
    }

    #[test]
    fn file_tools_have_exact_paths_but_shell_commands_never_do() {
        let root = Path::new("/repo");
        let edit = serde_json::json!({"tool_name":"Edit","tool_input":{"file_path":"src/a.rs"},"cwd":"/repo/sub"});
        assert_eq!(
            mutation_paths(root, "claude-code", &edit),
            vec![PathBuf::from("/repo/sub/src/a.rs")]
        );
        let shell = serde_json::json!({"tool_name":"Bash","tool_input":{"command":"echo model-x > a.rs","file_path":"fake.rs"}});
        assert!(mutation_paths(root, "codex", &shell).is_empty());
        let outside = serde_json::json!({"tool_name":"Edit","tool_input":{"file_path":"a.rs"},"cwd":"/elsewhere"});
        assert!(mutation_paths(root, "claude-code", &outside).is_empty());
    }
    #[test]
    fn apply_patch_paths_are_structural_and_include_moves() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n@@\n-old\n+new\n*** Add File: c.rs\n+*** Update File: not-a-path\n*** End Patch";
        assert_eq!(patch_paths(patch), vec!["a.rs", "b.rs", "c.rs"]);
        assert!(patch_paths(&format!("apply_patch <<EOF\n{patch}\nEOF")).is_empty());
        assert!(patch_paths("*** Begin Patch\nnot patch syntax\n*** End Patch").is_empty());
    }
    #[test]
    fn opencode_native_apply_patch_paths_use_patch_text() {
        let payload = serde_json::json!({"tool":"apply_patch","args":{"patchText":"*** Begin Patch\n*** Update File: fixture.txt\n@@\n-alpha\n+bravo\n*** End Patch"},"cwd":"/repo"});
        assert_eq!(
            mutation_paths(Path::new("/repo"), "opencode", &payload),
            vec![PathBuf::from("/repo/fixture.txt")]
        );
        let shell = serde_json::json!({"tool":"bash","args":payload["args"]});
        assert!(mutation_paths(Path::new("/repo"), "opencode", &shell).is_empty());
    }
    #[test]
    fn opencode_plugin_ids_are_exact_and_not_generic_object_ids() {
        let payload = serde_json::json!({"sessionID":"s","callID":"call","messageID":"message","tool":"write","args":{"filePath":"a"}});
        let evidence = operation_cursor("opencode", &payload)
            .attribution_evidence
            .unwrap();
        assert_eq!(evidence.scope.tool_call_id.as_deref(), Some("call"));
        assert_eq!(evidence.scope.message_id.as_deref(), Some("message"));
        assert!(evidence.selected.model.is_none());
        let unknown = operation_cursor("opencode", &serde_json::json!({"id":"not-a-call"}));
        assert!(
            unknown
                .attribution_evidence
                .unwrap()
                .scope
                .tool_call_id
                .is_none()
        );
    }
}
