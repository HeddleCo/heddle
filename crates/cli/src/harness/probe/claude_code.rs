// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use super::{
    HarnessActorProbe, HarnessAttachHints, HarnessProbeInput, HarnessProbeResult, ProbeSource,
    argv_value, attribution_env_hint, csv_paths, parse_u64,
};

pub(crate) struct ClaudeCodeProbe;

impl HarnessActorProbe for ClaudeCodeProbe {
    fn harness_name(&self) -> &'static str {
        "claude-code"
    }

    fn matches(&self, input: &HarnessProbeInput) -> bool {
        input.explicit_harness.as_deref() == Some(self.harness_name())
            || input.probe_metadata.contains_key("session_id")
            || input.probe_metadata.contains_key("agent_id")
            || input.env_hints.contains_key("CLAUDECODE")
            || input
                .argv
                .as_ref()
                .and_then(|argv| argv.first())
                .is_some_and(|program| program.to_ascii_lowercase().contains("claude"))
    }

    fn probe(&self, input: &HarnessProbeInput) -> Result<HarnessProbeResult> {
        let metadata = &input.probe_metadata;
        let argv = input.argv.as_deref().unwrap_or(&[]);
        let session_id = metadata
            .get("session_id")
            .cloned()
            .or_else(|| argv_value(argv, "--session-id"));
        let agent_id = metadata.get("agent_id").cloned();
        let transcript_path = metadata
            .get("transcript_path")
            .cloned()
            .or_else(|| input.env_hints.get("CLAUDE_TRANSCRIPT_PATH").cloned());
        let probe_source = if metadata.get("hook_event").is_some() {
            ProbeSource::HookPayload
        } else if metadata.get("status_line").is_some() {
            ProbeSource::StatusPayload
        } else if session_id.is_some() {
            ProbeSource::AppProtocol
        } else {
            ProbeSource::ArgvEnv
        };
        Ok(HarnessProbeResult {
            harness: Some("claude-code".to_string()),
            provider: input
                .explicit_provider
                .clone()
                .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_PROVIDER"))
                .or_else(|| input.current_provider.clone())
                .or(Some("anthropic".to_string())),
            model: input
                .explicit_model
                .clone()
                .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_MODEL"))
                .or_else(|| metadata.get("model").cloned())
                .or_else(|| argv_value(argv, "--model"))
                .or_else(|| input.env_hints.get("CLAUDE_MODEL").cloned())
                .or_else(|| input.env_hints.get("ANTHROPIC_MODEL").cloned())
                .or_else(|| input.env_hints.get("MODEL").cloned())
                .or_else(|| input.current_model.clone()),
            thinking_level: metadata
                .get("effort")
                .cloned()
                .or_else(|| argv_value(argv, "--effort"))
                .or_else(|| input.env_hints.get("THINKING_LEVEL").cloned()),
            native_actor_key: agent_id
                .clone()
                .map(|id| format!("claude-code:agent:{id}"))
                .or_else(|| {
                    session_id
                        .clone()
                        .map(|id| format!("claude-code:session:{id}"))
                }),
            native_parent_actor_key: agent_id
                .as_ref()
                .and(session_id.as_ref())
                .map(|session| format!("claude-code:session:{session}")),
            native_instance_key: transcript_path
                .clone()
                .map(|path| format!("claude-code:transcript:{path}")),
            usage_totals: wire::UsageTotals {
                input_tokens: parse_u64(metadata.get("input_tokens")),
                output_tokens: parse_u64(metadata.get("output_tokens")),
                reasoning_tokens: parse_u64(metadata.get("reasoning_tokens")),
                cache_creation_tokens: None,
                cache_read_tokens: None,
                tool_calls: metadata.get("tool_calls").and_then(|v| v.parse().ok()),
                cost_micros_usd: parse_u64(metadata.get("cost_micros_usd")),
            },
            policy: input
                .explicit_policy
                .clone()
                .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_POLICY"))
                .or_else(|| input.current_policy.clone()),
            touched_paths: csv_paths(metadata.get("touched_paths")),
            transcript_refs: transcript_path
                .map(|path| {
                    vec![wire::TranscriptAttachmentRef {
                        attachment_id: path,
                        kind: Some("transcript_path".to_string()),
                        summary: None,
                    }]
                })
                .unwrap_or_default(),
            attach_hints: HarnessAttachHints {
                root_actor: agent_id.is_none(),
            },
            confidence: Some(if metadata.get("hook_event").is_some() {
                0.96
            } else if metadata.get("status_line").is_some() {
                0.9
            } else if session_id.is_some() {
                0.75
            } else {
                0.55
            }),
            probe_source: Some(probe_source.as_str().to_string()),
            ..HarnessProbeResult::default()
        })
    }
}
