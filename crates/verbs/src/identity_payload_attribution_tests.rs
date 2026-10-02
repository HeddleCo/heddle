// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;

#[test]
fn claude_does_not_promote_display_label_to_model_id() {
    assert!(
        claude_cursor_patch(&json!({"model":{"display_name":"Claude Opus"}}))
            .model
            .is_none()
    );
    assert_eq!(
        claude_cursor_patch(&json!({"model":{"id":"claude-opus-4-7","display_name":"Opus"}}))
            .model
            .as_deref(),
        Some("claude-opus-4-7")
    );
}

#[test]
fn codex_notify_preserves_producing_thread_without_inventing_model() {
    let patch = codex_cursor_patch(&json!({
        "type":"agent-turn-complete", "thread-id":"child-thread", "turn-id":"turn-1",
        "input-messages":["private prompt"], "last-assistant-message":"private response"
    }));
    assert_eq!(patch.session.as_deref(), Some("child-thread"));
    assert!(patch.model.is_none());
    let serialized = serde_json::to_string(&patch).unwrap();
    assert!(!serialized.contains("private"));
    assert_eq!(
        patch
            .attribution_evidence
            .as_ref()
            .unwrap()
            .scope
            .turn_id
            .as_deref(),
        Some("turn-1")
    );
}

#[test]
fn codex_explicit_provider_survives_non_openai_backend() {
    let patch =
        codex_cursor_patch(&json!({"session_id":"s", "model_provider":"local", "model":"model-a"}));
    assert_eq!(patch.provider.as_deref(), Some("local"));
}

#[test]
fn opencode_assistant_message_uses_native_model_fields() {
    let patch = opencode_cursor_patch(
        &json!({"event":{"type":"message.updated","properties":{"info":{
            "id":"msg-1","sessionID":"ses-1","role":"assistant","parentID":"user-message",
            "modelID":"model-a","providerID":"provider-a","variant":"high","agent":"build"
        }}}}),
    );
    assert_eq!(patch.session.as_deref(), Some("ses-1"));
    assert_eq!(patch.model.as_deref(), Some("model-a"));
    assert_eq!(patch.provider.as_deref(), Some("provider-a"));
    assert_eq!(patch.thought_level.as_deref(), Some("high"));
    assert!(
        patch.parent.is_none(),
        "assistant parentID is a message, not a parent session"
    );
}

#[test]
fn opencode_user_configuration_does_not_become_message_model() {
    let patch = opencode_cursor_patch(
        &json!({"event":{"type":"message.updated","properties":{"info":{
            "id":"user-1","sessionID":"ses-1","role":"user",
            "model":{"providerID":"configured","modelID":"configured-model"}
        }}}}),
    );
    assert!(patch.model.is_none());
    assert!(patch.provider.is_none());
}

#[test]
fn opencode_session_lineage_is_distinct_from_message_parent() {
    let patch = opencode_cursor_patch(
        &json!({"event":{"type":"session.created","properties":{"info":{
            "id":"child-session","parentID":"root-session","version":"1.2.3"
        }}}}),
    );
    assert_eq!(patch.session.as_deref(), Some("child-session"));
    assert_eq!(patch.parent.as_deref(), Some("root-session"));
    assert!(patch.model.is_none());
}

#[test]
fn opencode_wrapped_event_preserves_flat_compatibility_provider() {
    let patch = opencode_cursor_patch(&json!({
        "provider":"local", "model":"model-a", "sessionID":"s",
        "event":{"type":"session.updated","properties":{}}
    }));
    assert_eq!(patch.provider.as_deref(), Some("local"));
    assert_eq!(patch.model.as_deref(), Some("model-a"));
}

#[test]
fn inherited_environment_markers_never_mix_harness_identity() {
    let env = BTreeMap::from([
        ("CODEX_THREAD_ID".into(), "codex-thread".into()),
        ("CLAUDE_CODE_SESSION_ID".into(), "claude-session".into()),
        ("CLAUDE_EFFORT".into(), "high".into()),
        ("PI_SESSION_ID".into(), "pi-session".into()),
        ("PI_MODEL".into(), "pi-model".into()),
        ("PI_PROVIDER".into(), "pi-provider".into()),
        ("PI_PARENT_ID".into(), "pi-parent".into()),
    ]);
    let cursor = cursor_patch_from_child_env(&env);
    assert_eq!(cursor.session.as_deref(), Some("codex-thread"));
    assert!(cursor.model.is_none());
    assert!(cursor.provider.is_none());
    assert!(cursor.thought_level.is_none());
    assert!(cursor.parent.is_none());
    let evidence = cursor.attribution_evidence.unwrap();
    assert_eq!(evidence.harness.unwrap().value, "codex");
    assert_eq!(
        evidence.scope.harness_session_id.as_deref(),
        Some("codex-thread")
    );
    assert!(evidence.selected.model.is_none());
}
