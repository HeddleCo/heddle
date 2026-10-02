// SPDX-License-Identifier: Apache-2.0
//! Typed probe observations; defaults are labelled legacy, never execution proof.
use super::{HarnessProbeInput, HarnessProbeResult, argv_matches_harness, argv_value};
use objects::object::{
    AttributionBasis as Basis, AttributionClaim as Claim, AttributionEvidenceV1 as Evidence,
    AttributionSource as Source, HarnessVersionScope,
};
use verbs::HarnessKind;

fn claim(value: Option<String>, basis: Basis, source: Source) -> Option<Claim> {
    let value = Claim::new(value?, basis, source);
    Evidence {
        harness: Some(value.clone()),
        ..Default::default()
    }
    .validate()
    .ok()
    .map(|_| value)
}
fn id(value: Option<String>) -> Option<String> {
    claim(value, Basis::Observed, Source::HarnessHook).map(|claim| claim.value)
}

pub(super) fn from_probe(
    input: &HarnessProbeInput,
    result: &HarnessProbeResult,
) -> Option<Evidence> {
    let metadata = &input.probe_metadata;
    let harness = result.harness.as_deref();
    let transcript = harness == Some("codex")
        && metadata.get("session_source").map(String::as_str) == Some("rollout_session_meta");
    let source = match result.probe_source.as_deref() {
        Some("session_transcript") => Source::Transcript,
        Some("hook_payload") => Source::HarnessHook,
        Some("status_payload") => Source::StatusLine,
        Some("app_protocol" | "sse_or_rest") => Source::Request,
        Some("explicit_payload") => Source::ExplicitArgument,
        _ => Source::Process,
    };
    let mut evidence = Evidence {
        harness: claim(result.harness.clone(), Basis::Observed, source),
        ..Default::default()
    };
    evidence.selected.model = if input.explicit_model.is_some() {
        claim(
            input.explicit_model.clone(),
            Basis::Explicit,
            Source::ExplicitArgument,
        )
    } else if let Some(model) = metadata.get("model").or_else(|| {
        (harness == Some("opencode"))
            .then(|| metadata.get("agent_model"))
            .flatten()
    }) {
        let basis = if source == Source::StatusLine {
            Basis::Configured
        } else {
            Basis::RequestReported
        };
        claim(
            Some(model.clone()),
            basis,
            if transcript {
                Source::Transcript
            } else {
                source
            },
        )
    } else {
        claim(result.model.clone(), Basis::Legacy, Source::Legacy)
    };
    // Match the adapter's actual precedence, and label each field's source
    // separately. A harness fingerprint is not evidence of a backend provider.
    evidence.selected.provider = claim(
        input.explicit_provider.clone(),
        Basis::Explicit,
        Source::ExplicitArgument,
    )
    .or_else(|| {
        claim(
            input.env_hints.get("HEDDLE_AGENT_PROVIDER").cloned(),
            Basis::Explicit,
            Source::Environment,
        )
    })
    .or_else(|| {
        claim(
            metadata
                .get("model_provider")
                .or_else(|| metadata.get("provider"))
                .cloned(),
            if transcript {
                Basis::Configured
            } else {
                Basis::RequestReported
            },
            if transcript {
                Source::SessionMetadata
            } else {
                source
            },
        )
    })
    .or_else(|| {
        (harness == Some("opencode"))
            .then(|| {
                claim(
                    input.env_hints.get("OPENCODE_PROVIDER").cloned(),
                    Basis::Configured,
                    Source::Environment,
                )
            })
            .flatten()
    })
    .or_else(|| {
        claim(
            input.current_provider.clone(),
            Basis::Legacy,
            Source::Legacy,
        )
    });
    evidence.selected.thought_level = claim(
        input.explicit_thinking_level.clone(),
        Basis::Explicit,
        Source::ExplicitArgument,
    )
    .or_else(|| {
        claim(
            metadata
                .get("model_reasoning_effort")
                .or_else(|| metadata.get("reasoning_effort"))
                .or_else(|| metadata.get("effort"))
                .cloned(),
            if source == Source::StatusLine {
                Basis::Configured
            } else {
                Basis::RequestReported
            },
            if transcript {
                Source::Transcript
            } else {
                source
            },
        )
    })
    .or_else(|| {
        (harness == Some("claude-code") && argv_matches_harness(input, HarnessKind::ClaudeCode))
            .then(|| {
                claim(
                    argv_value(input.argv.as_deref().unwrap_or_default(), "--effort"),
                    Basis::Configured,
                    Source::Process,
                )
            })
            .flatten()
    })
    .or_else(|| {
        claim(
            result.thinking_level.clone(),
            Basis::Configured,
            Source::Environment,
        )
    });

    let argv = input.argv.as_deref().unwrap_or_default();
    evidence.scope.harness_session_id = id(match harness {
        Some("codex") => metadata
            .get("thread_id")
            .or_else(|| metadata.get("session_id"))
            .cloned()
            .or_else(|| input.env_hints.get("CODEX_THREAD_ID").cloned()),
        Some("claude-code") => metadata
            .get("session_id")
            .cloned()
            .or_else(|| input.env_hints.get("CLAUDE_CODE_SESSION_ID").cloned())
            .or_else(|| {
                argv_matches_harness(input, HarnessKind::ClaudeCode)
                    .then(|| argv_value(argv, "--session-id"))
                    .flatten()
            }),
        Some("opencode") => metadata.get("session_id").cloned().or_else(|| {
            argv_matches_harness(input, HarnessKind::OpenCode)
                .then(|| argv_value(argv, "--session"))
                .flatten()
        }),
        _ => metadata.get("session_id").cloned(),
    });
    evidence.scope.actor_id =
        id(metadata.get("agent_id").cloned()).or(evidence.scope.harness_session_id.clone());
    evidence.scope.parent_actor_id = id(metadata
        .get("parent_thread_id")
        .or_else(|| metadata.get("parent_id"))
        .cloned());
    evidence.scope.parent_harness_session_id = if metadata.contains_key("agent_id") {
        evidence.scope.harness_session_id.clone()
    } else {
        evidence.scope.parent_actor_id.clone()
    };
    evidence.scope.turn_id = id(metadata.get("turn_id").cloned());
    evidence.scope.request_id = id(metadata.get("request_id").cloned());
    evidence.scope.message_id = id(metadata.get("message_id").cloned());
    if transcript {
        evidence.harness_version = claim(
            metadata.get("cli_version").cloned(),
            Basis::Observed,
            Source::SessionMetadata,
        );
        evidence.harness_version_scope = evidence
            .harness_version
            .as_ref()
            .map(|_| HarnessVersionScope::SessionCreation);
    }
    evidence.validate().ok().map(|_| evidence)
}
