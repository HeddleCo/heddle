// SPDX-License-Identifier: Apache-2.0
//! Allowlisted harness identity observations. Never copies a payload bag.
use crate::{IdentityCursor, first_value_string, value_string};
use objects::object::{
    AttributionBasis as Basis, AttributionClaim as Claim, AttributionEvidenceV1 as Evidence,
    AttributionSource as Source, HarnessVersionScope,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn claim(value: Option<String>, basis: Basis, source: Source) -> Option<Claim> {
    let claim = Claim::new(value?, basis, source);
    // Reuse canonical identity admission; invalid source values become unknown.
    let probe = Evidence {
        harness: Some(claim.clone()),
        ..Default::default()
    };
    probe.validate().ok().map(|_| claim)
}
fn id(value: Option<String>) -> Option<String> {
    claim(value, Basis::Observed, Source::HarnessHook).map(|c| c.value)
}

pub(crate) fn from_payload(harness: &str, payload: &Value, cursor: &IdentityCursor) -> Evidence {
    let status = harness == "claude-code"
        && payload.get("version").is_some()
        && payload.pointer("/model/id").is_some();
    let source = if status {
        Source::StatusLine
    } else {
        Source::HarnessHook
    };
    let mut evidence = Evidence {
        harness: claim(Some(harness.into()), Basis::Observed, source),
        ..Default::default()
    };
    let mut model = cursor.model.clone();
    let mut provider = first_value_string(payload, &[&["model_provider"], &["provider"]]);
    let mut selected_source = source;
    let mut basis = if status {
        Basis::Configured
    } else {
        Basis::RequestReported
    };
    if harness == "opencode" {
        let props = payload
            .pointer("/event/properties")
            .or_else(|| payload.get("properties"));
        let assistant = props
            .and_then(|p| p.get("info"))
            .filter(|info| info.get("role").and_then(Value::as_str) == Some("assistant"));
        provider = cursor.provider.clone();
        if let Some(info) = assistant {
            selected_source = Source::Request;
            evidence.scope.message_id = id(value_string(info, &["id"]));
            evidence.scope.actor_id = id(value_string(info, &["sessionID"]));
        }
        if let Some(info) = props.and_then(|p| p.get("info"))
            && info.get("role").is_none()
            && matches!(
                crate::opencode_event_type(payload).as_deref(),
                Some("session.created" | "session.updated")
            )
        {
            evidence.harness_version = claim(
                value_string(info, &["version"]),
                Basis::Observed,
                Source::SessionMetadata,
            );
            evidence.harness_version_scope = evidence
                .harness_version
                .as_ref()
                .map(|_| HarnessVersionScope::SessionCreation);
        }
    } else if harness == "claude-code" {
        if payload.get("hook_event_name").and_then(Value::as_str) == Some("PostModelSwitch") {
            model = value_string(payload, &["to_model"]);
            basis = Basis::Configured;
        }
        if status {
            evidence.harness_version =
                claim(value_string(payload, &["version"]), Basis::Observed, source);
            evidence.harness_version_scope = evidence
                .harness_version
                .as_ref()
                .map(|_| HarnessVersionScope::CurrentInvocation);
        }
        // Only an assistant response's model is response evidence. Status/hook
        // selection is retained independently, including fallback differences.
        if payload.get("type").and_then(Value::as_str) == Some("assistant") {
            evidence.response.model = claim(
                value_string(payload, &["message", "model"]),
                Basis::ResponseReported,
                Source::Response,
            );
            evidence.scope.response_id = id(value_string(payload, &["message", "id"]));
        }
    } else if harness == "pi" {
        provider = cursor.provider.clone();
        basis = Basis::Explicit;
    }
    if harness == "opencode"
        && let Some(version) = value_string(payload, &["heddle_harness_version"])
    {
        evidence.harness_version = claim(Some(version), Basis::Observed, Source::HarnessHook);
        evidence.harness_version_scope = evidence
            .harness_version
            .as_ref()
            .map(|_| HarnessVersionScope::CurrentInvocation);
    }
    evidence.selected.provider = claim(provider, basis, selected_source);
    evidence.selected.model = claim(model, basis, selected_source);
    evidence.selected.thought_level = claim(cursor.thought_level.clone(), basis, selected_source);
    evidence.scope.harness_session_id = id(cursor.session.clone());
    evidence.scope.actor_id = id(value_string(payload, &["agent_id"]))
        .or(evidence.scope.actor_id)
        .or_else(|| id(cursor.session.clone()));
    evidence.scope.parent_actor_id = id(first_value_string(
        payload,
        &[&["parent_thread_id"], &["parent_actor_id"]],
    ));
    if payload.get("agent_id").is_some() && matches!(harness, "codex" | "claude-code") {
        evidence.scope.parent_harness_session_id = id(cursor.session.clone());
    } else if harness == "codex" {
        evidence.scope.parent_harness_session_id = id(first_value_string(
            payload,
            &[&["parent_id"], &["parentId"]],
        ))
        .filter(|parent| Some(parent) != evidence.scope.harness_session_id.as_ref());
    } else {
        evidence.scope.parent_harness_session_id = id(cursor.parent.clone())
            .filter(|parent| Some(parent) != evidence.scope.harness_session_id.as_ref());
    }
    evidence.scope.harness_instance_id = id(first_value_string(
        payload,
        &[&["harness_instance_id"], &["invocation_id"]],
    ));
    evidence.scope.turn_id = id(first_value_string(
        payload,
        &[&["turn_id"], &["turn-id"], &["prompt_id"]],
    ));
    evidence.scope.root_turn_id = id(value_string(payload, &["root_turn_id"]));
    evidence.scope.tool_call_id = id(first_value_string(
        payload,
        &[
            &["tool_use_id"],
            &["callID"],
            &["call_id"],
            &["tool_call_id"],
        ],
    ));
    evidence.scope.request_id = id(value_string(payload, &["request_id"]));
    evidence.scope.attempt_id = id(value_string(payload, &["attempt"]));
    evidence
}

pub(crate) fn environment_harness(env: &BTreeMap<String, String>) -> Option<&'static str> {
    if env.contains_key("CODEX_THREAD_ID") {
        Some("codex")
    } else if [
        "CLAUDECODE",
        "CLAUDE_CODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_EFFORT",
    ]
    .iter()
    .any(|key| env.contains_key(*key))
    {
        Some("claude-code")
    } else if env.contains_key("OPENCODE_CLIENT") {
        Some("opencode")
    } else if ["PI_SESSION_ID", "PI_MODEL", "PI_REASONING_LEVEL"]
        .iter()
        .any(|key| env.contains_key(*key))
    {
        Some("pi")
    } else {
        None
    }
}

pub(crate) fn from_environment(
    env: &BTreeMap<String, String>,
    cursor: &IdentityCursor,
) -> Option<Evidence> {
    let harness = environment_harness(env)?;
    let mut evidence = Evidence {
        harness: claim(Some(harness.into()), Basis::Observed, Source::Environment),
        ..Default::default()
    };
    evidence.scope.harness_session_id = id(cursor
        .session
        .clone()
        .or_else(|| env.get("CODEX_THREAD_ID").cloned()));
    evidence.scope.actor_id = evidence.scope.harness_session_id.clone();
    if harness == "pi" {
        evidence.scope.parent_harness_session_id = id(cursor.parent.clone());
        evidence.selected.provider = claim(
            env.get("PI_PROVIDER").cloned(),
            Basis::Configured,
            Source::Environment,
        );
        evidence.selected.model = claim(
            env.get("PI_MODEL").cloned(),
            Basis::Configured,
            Source::Environment,
        );
    }
    evidence.selected.thought_level = claim(
        cursor.thought_level.clone(),
        Basis::Configured,
        Source::Environment,
    );
    Some(evidence)
}

/// A new supplied identity boundary cannot inherit a previous request's model.
/// Missing fields can augment a known scope, but never bridge two known IDs.
pub(crate) fn same_model_scope(current: Option<&Evidence>, patch: Option<&Evidence>) -> bool {
    let (Some(current), Some(patch)) = (current, patch) else {
        return true;
    };
    if patch
        .harness
        .as_ref()
        .is_some_and(|claim| claim.source == Source::Transcript)
        || current.harness.as_ref().map(|c| &c.value) != patch.harness.as_ref().map(|c| &c.value)
    {
        return false;
    }
    let a = &current.scope;
    let b = &patch.scope;
    [
        (&a.actor_id, &b.actor_id),
        (&a.harness_instance_id, &b.harness_instance_id),
        (&a.turn_id, &b.turn_id),
        (&a.message_id, &b.message_id),
        (&a.request_id, &b.request_id),
        (&a.attempt_id, &b.attempt_id),
    ]
    .iter()
    .all(|(old, new)| new.is_none() || new == old)
}

pub(crate) fn merge(
    current: Option<&Evidence>,
    patch: Option<&Evidence>,
    same_actor: bool,
) -> Option<Evidence> {
    let Some(patch) = patch else {
        return same_actor.then(|| current.cloned()).flatten();
    };
    let Some(current) = current.filter(|_| same_actor) else {
        return Some(patch.clone());
    };
    if current.harness.as_ref().map(|c| &c.value) != patch.harness.as_ref().map(|c| &c.value) {
        return Some(patch.clone());
    }
    if patch
        .scope
        .actor_id
        .as_ref()
        .is_some_and(|id| current.scope.actor_id.as_ref() != Some(id))
        || patch
            .scope
            .harness_instance_id
            .as_ref()
            .is_some_and(|id| current.scope.harness_instance_id.as_ref() != Some(id))
    {
        return Some(patch.clone());
    }
    let mut next = patch.clone();
    // Session-creation evidence survives resume. Current/installed versions
    // cannot be promoted across an unobserved invocation boundary.
    let same_invocation = current.scope.harness_instance_id.is_some()
        && current.scope.harness_instance_id == patch.scope.harness_instance_id;
    if current.harness_version_scope == Some(HarnessVersionScope::SessionCreation)
        || same_invocation
    {
        next.harness_version = next.harness_version.or(current.harness_version.clone());
        next.harness_version_scope = next.harness_version_scope.or(current.harness_version_scope);
    }
    next.scope.harness_session_id = next
        .scope
        .harness_session_id
        .or(current.scope.harness_session_id.clone());
    next.scope.actor_id = next.scope.actor_id.or(current.scope.actor_id.clone());
    next.scope.parent_actor_id = next
        .scope
        .parent_actor_id
        .or(current.scope.parent_actor_id.clone());
    if same_model_scope(Some(current), Some(patch)) {
        macro_rules! merge_field {
            ($group:ident, $field:ident) => {
                next.$group.$field = next.$group.$field.or(current.$group.$field.clone());
            };
        }
        merge_field!(selected, provider);
        merge_field!(selected, model);
        merge_field!(selected, version);
        merge_field!(selected, thought_level);
        let same_selection = patch.selected.model.as_ref().is_none_or(|model| {
            current
                .selected
                .model
                .as_ref()
                .is_some_and(|old| old.value == model.value)
        });
        let same_response = patch.scope.response_id.is_none()
            || patch.scope.response_id == current.scope.response_id;
        if same_selection && same_response {
            merge_field!(response, provider);
            merge_field!(response, model);
            merge_field!(response, version);
            merge_field!(response, thought_level);
            merge_field!(scope, response_id);
        }
        merge_field!(scope, turn_id);
        merge_field!(scope, root_turn_id);
        merge_field!(scope, message_id);
        merge_field!(scope, request_id);
        merge_field!(scope, attempt_id);
        merge_field!(scope, tool_call_id);
    }
    Some(next)
}
