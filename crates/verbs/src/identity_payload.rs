// SPDX-License-Identifier: Apache-2.0
//! Parse harness hook stdin into an identity-cursor patch.
//!
//! Hot path: stdin JSON only. No JSONL, no `session.get`, no disk glob.
//! Stamp what this event said. Missing fields stay unset so merge keeps
//! the last cursor value.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::harness_json::{first_value_string, value_string};
use crate::identity_cursor::{
    IdentityCursor, published_field, thought_level_from_payload, value_string_or_named,
};

/// Supported stamp harness names (install + stdin parsers).
pub fn stamp_harness_name(name: &str) -> Option<&'static str> {
    match name {
        "claude" | "claude-code" => Some("claude-code"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        "pi" => Some("pi"),
        _ => None,
    }
}

/// Parse a hook stdin body for `harness`. Empty / invalid JSON → empty patch.
pub fn cursor_patch_from_stdin(harness: &str, stdin: &str) -> IdentityCursor {
    let payload = match serde_json::from_str::<Value>(stdin.trim()) {
        Ok(value) if !value.is_null() => value,
        _ => return IdentityCursor::default(),
    };
    match stamp_harness_name(harness).unwrap_or(harness) {
        "claude-code" => claude_cursor_patch(&payload),
        "codex" => codex_cursor_patch(&payload),
        "opencode" => opencode_cursor_patch(&payload),
        "pi" => pi_cursor_patch(&payload),
        _ => IdentityCursor::default(),
    }
}

/// Claude PreToolUse / StatusLine / SessionStart stdin.
///
/// Live model is StatusLine `model.id`. Hooks otherwise carry optional model
/// on SessionStart only. Effort is `{level}` (not a string). Parent is
/// `agent_id`. Do not invent a model.
pub fn claude_cursor_patch(payload: &Value) -> IdentityCursor {
    IdentityCursor {
        provider: Some("anthropic".to_string()),
        model: value_string_or_named(payload, &["model"], &["id"])
            .or_else(|| value_string(payload, &["model", "display_name"])),
        thought_level: thought_level_from_payload(payload),
        session: first_value_string(payload, &[&["session_id"], &["sessionId"]]),
        parent: value_string(payload, &["agent_id"]),
    }
    .omit_unpublished()
}

/// Codex hook stdin: `model` + session. Effort only if present on stdin
/// (`reasoning_effort` or `turn_context`). Do not read `transcript_path`.
pub fn codex_cursor_patch(payload: &Value) -> IdentityCursor {
    IdentityCursor {
        provider: Some("openai".to_string()),
        model: value_string_or_named(payload, &["model"], &["id", "slug"]),
        thought_level: thought_level_from_payload(payload),
        session: first_value_string(
            payload,
            &[&["session_id"], &["sessionId"], &["conversation_id"]],
        ),
        parent: first_value_string(payload, &[&["parent_id"], &["parentId"], &["agent_id"]]),
    }
    .omit_unpublished()
}

/// OpenCode event payload. Use `event.type` (not `event.name`). Object
/// `model` exposes `id` / `providerID` / `variant`.
pub fn opencode_cursor_patch(payload: &Value) -> IdentityCursor {
    let nested = payload.get("event").unwrap_or(payload);
    let properties = nested
        .get("properties")
        .or_else(|| payload.get("properties"))
        .unwrap_or(payload);
    IdentityCursor {
        provider: value_string(properties, &["model", "providerID"])
            .or_else(|| value_string(properties, &["provider"]))
            .or_else(|| value_string(payload, &["model", "providerID"])),
        model: value_string_or_named(properties, &["model"], &["id"])
            .or_else(|| value_string_or_named(payload, &["model"], &["id"])),
        thought_level: value_string(properties, &["model", "variant"])
            .or_else(|| thought_level_from_payload(properties))
            .or_else(|| thought_level_from_payload(payload)),
        session: first_value_string(
            payload,
            &[&["sessionID"], &["session_id"], &["session", "id"]],
        )
        .or_else(|| first_value_string(properties, &[&["sessionID"], &["session_id"]])),
        parent: first_value_string(payload, &[&["parentID"], &["parent_id"]])
            .or_else(|| first_value_string(properties, &[&["parentID"], &["parent_id"]])),
    }
    .omit_unpublished()
}

/// Pi payload or already-shaped cursor JSON (`PI_*` maps at the caller).
pub fn pi_cursor_patch(payload: &Value) -> IdentityCursor {
    IdentityCursor {
        provider: first_value_string(payload, &[&["provider"], &["PI_PROVIDER"]]),
        model: first_value_string(payload, &[&["model"], &["PI_MODEL"]]),
        thought_level: first_value_string(
            payload,
            &[&["thought_level"], &["PI_REASONING_LEVEL"], &["variant"]],
        ),
        session: first_value_string(
            payload,
            &[&["session"], &["PI_SESSION_ID"], &["session_id"]],
        ),
        parent: first_value_string(payload, &[&["parent"], &["PI_PARENT_ID"]]),
    }
    .omit_unpublished()
}

/// Child-process fallback env. Claude: session + effort only — never
/// `$ANTHROPIC_MODEL`. Pi: model + reasoning + session.
pub fn cursor_patch_from_child_env(env: &BTreeMap<String, String>) -> IdentityCursor {
    let claude_session = env.get("CLAUDE_CODE_SESSION_ID").cloned();
    let claude_effort = env.get("CLAUDE_EFFORT").cloned();
    let pi_model = env.get("PI_MODEL").cloned();
    let pi_effort = env.get("PI_REASONING_LEVEL").cloned();
    let pi_session = env.get("PI_SESSION_ID").cloned();
    let provider = if pi_model.is_some() {
        env.get("PI_PROVIDER").cloned()
    } else if claude_session.is_some() || claude_effort.is_some() {
        Some("anthropic".to_string())
    } else {
        None
    };
    IdentityCursor {
        provider,
        model: pi_model,
        thought_level: claude_effort.or(pi_effort),
        session: claude_session.or(pi_session),
        parent: env.get("PI_PARENT_ID").cloned(),
    }
    .omit_unpublished()
}

/// SessionEnd / session.deleted / session.closed expire the live cursor.
pub fn cursor_event_expires(payload: &Value, event_hint: Option<&str>) -> bool {
    expire_event_name(event_hint)
        || first_value_string(
            payload,
            &[
                &["hook_event_name"],
                &["hook_event"],
                &["event", "type"],
                &["type"],
                &["event", "name"],
                &["name"],
            ],
        )
        .is_some_and(|name| expire_event_name(Some(&name)))
}

fn expire_event_name(name: Option<&str>) -> bool {
    let Some(name) = published_field(name) else {
        return false;
    };
    matches!(
        name,
        "SessionEnd" | "session.end" | "session.deleted" | "session.closed" | "session.idle"
    ) || name.eq_ignore_ascii_case("sessionend")
}

/// OpenCode plugin should key on `event.type`, not `event.name`.
pub fn opencode_event_type(payload: &Value) -> Option<String> {
    first_value_string(
        payload,
        &[&["event", "type"], &["type"], &["event", "name"], &["name"]],
    )
    .and_then(|s| published_field(Some(&s)).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_end_payload_expires_cursor() {
        assert!(cursor_event_expires(
            &json!({"hook_event_name": "SessionEnd", "session_id": "s1"}),
            None
        ));
        assert!(cursor_event_expires(
            &json!({"event": {"type": "session.deleted"}}),
            None
        ));
        assert!(cursor_event_expires(&json!({}), Some("SessionEnd")));
        assert!(!cursor_event_expires(
            &json!({"hook_event_name": "PreToolUse"}),
            None
        ));
        assert!(!cursor_event_expires(
            &json!({"event": {"type": "session.updated"}}),
            None
        ));
    }

    #[test]
    fn claude_effort_level_object_and_no_anthropic_model_invent() {
        let patch = claude_cursor_patch(&json!({
            "session_id": "sess-claude",
            "agent_id": "agent-9",
            "effort": {"level": "max"}
        }));
        assert_eq!(patch.provider.as_deref(), Some("anthropic"));
        assert!(patch.model.is_none());
        assert_eq!(patch.thought_level.as_deref(), Some("max"));
        assert_eq!(patch.session.as_deref(), Some("sess-claude"));
        assert_eq!(patch.parent.as_deref(), Some("agent-9"));
    }

    #[test]
    fn claude_status_line_model_id() {
        let patch = claude_cursor_patch(&json!({
            "model": {"id": "claude-opus-4-7"}
        }));
        assert_eq!(patch.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn child_env_uses_claude_effort_not_anthropic_model() {
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_MODEL".into(), "claude-sonnet-4-6".into());
        env.insert("CLAUDE_CODE_SESSION_ID".into(), "sess-env".into());
        env.insert("CLAUDE_EFFORT".into(), "high".into());
        let patch = cursor_patch_from_child_env(&env);
        assert!(patch.model.is_none());
        assert_eq!(patch.session.as_deref(), Some("sess-env"));
        assert_eq!(patch.thought_level.as_deref(), Some("high"));
    }

    #[test]
    fn opencode_nested_event_properties() {
        let payload = json!({
            "event": {
                "type": "session.updated",
                "properties": {
                    "model": {
                        "id": "claude-sonnet-4-6",
                        "providerID": "anthropic",
                        "variant": "max"
                    },
                    "sessionID": "ses-nested"
                }
            }
        });
        let patch = opencode_cursor_patch(&payload);
        assert_eq!(patch.provider.as_deref(), Some("anthropic"));
        assert_eq!(patch.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(patch.thought_level.as_deref(), Some("max"));
        assert_eq!(patch.session.as_deref(), Some("ses-nested"));
    }

    #[test]
    fn opencode_object_model_and_event_type() {
        let payload = json!({
            "event": {"type": "session.updated"},
            "model": {
                "id": "claude-sonnet-4-6",
                "providerID": "anthropic",
                "variant": "max"
            },
            "sessionID": "ses-1"
        });
        assert_eq!(
            opencode_event_type(&payload).as_deref(),
            Some("session.updated")
        );
        let patch = opencode_cursor_patch(&payload);
        assert_eq!(patch.provider.as_deref(), Some("anthropic"));
        assert_eq!(patch.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(patch.thought_level.as_deref(), Some("max"));
        assert_eq!(patch.session.as_deref(), Some("ses-1"));
    }

    #[test]
    fn codex_model_from_stdin_skips_missing_effort() {
        let patch = cursor_patch_from_stdin(
            "codex",
            r#"{"model":"gpt-5.4","session_id":"c1","transcript_path":"/tmp/t.jsonl"}"#,
        );
        assert_eq!(patch.model.as_deref(), Some("gpt-5.4"));
        assert!(patch.thought_level.is_none());
        assert_eq!(patch.session.as_deref(), Some("c1"));
    }
}
