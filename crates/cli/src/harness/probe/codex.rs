// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use super::{
    HarnessActorProbe, HarnessAttachHints, HarnessProbeInput, HarnessProbeResult, ProbeSource,
    argv_value, attribution_env_hint, csv_paths, parse_u64,
};

pub(crate) struct CodexProbe;

impl HarnessActorProbe for CodexProbe {
    fn harness_name(&self) -> &'static str {
        "codex"
    }

    fn matches(&self, input: &HarnessProbeInput) -> bool {
        input.explicit_harness.as_deref() == Some(self.harness_name())
            || input.probe_metadata.contains_key("thread_id")
            || input.probe_metadata.contains_key("client_name")
            || input.env_hints.contains_key("CODEX_SANDBOX")
            || input.env_hints.contains_key("CODEX_THREAD_ID")
            || input.env_hints.contains_key("CODEX_CI")
            || input
                .argv
                .as_ref()
                .and_then(|argv| argv.first())
                .is_some_and(|program| program.to_ascii_lowercase().contains("codex"))
    }

    fn probe(&self, input: &HarnessProbeInput) -> Result<HarnessProbeResult> {
        let metadata = &input.probe_metadata;
        let argv = input.argv.as_deref().unwrap_or(&[]);
        let thread_id = metadata
            .get("thread_id")
            .cloned()
            .or_else(|| input.env_hints.get("CODEX_THREAD_ID").cloned());
        let client_name = metadata
            .get("client_name")
            .cloned()
            .or_else(|| metadata.get("client").cloned())
            .or_else(|| {
                input
                    .env_hints
                    .get("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
                    .cloned()
            });
        let model = input
            .explicit_model
            .clone()
            .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_MODEL"))
            .or_else(|| metadata.get("model").cloned())
            .or_else(|| argv_value(argv, "--model"))
            .or_else(|| input.env_hints.get("CODEX_MODEL").cloned())
            .or_else(|| input.env_hints.get("OPENAI_MODEL").cloned())
            .or_else(|| input.current_model.clone());
        let provider = input
            .explicit_provider
            .clone()
            .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_PROVIDER"))
            .or_else(|| metadata.get("model_provider").cloned())
            .or_else(|| input.current_provider.clone())
            .or(Some("openai".to_string()));
        let thinking_level = metadata
            .get("model_reasoning_effort")
            .cloned()
            .or_else(|| metadata.get("reasoning_effort").cloned())
            .or_else(|| input.env_hints.get("CODEX_REASONING_EFFORT").cloned())
            .or_else(|| input.env_hints.get("OPENAI_REASONING_EFFORT").cloned());
        let probe_source = if thread_id.is_some() {
            ProbeSource::AppProtocol
        } else if client_name.is_some() {
            ProbeSource::HookPayload
        } else {
            ProbeSource::ArgvEnv
        };
        Ok(HarnessProbeResult {
            harness: Some("codex".to_string()),
            provider,
            model,
            thinking_level,
            policy: input
                .explicit_policy
                .clone()
                .or_else(|| attribution_env_hint(&input.env_hints, "HEDDLE_AGENT_POLICY"))
                .or_else(|| input.current_policy.clone()),
            native_actor_key: thread_id.map(|id| format!("codex:thread:{id}")),
            native_parent_actor_key: None,
            native_instance_key: client_name.map(|id| format!("codex:client:{id}")),
            usage_totals: wire::UsageTotals {
                input_tokens: parse_u64(metadata.get("input_tokens")),
                output_tokens: parse_u64(metadata.get("output_tokens")),
                reasoning_tokens: parse_u64(metadata.get("reasoning_tokens")),
                cache_creation_tokens: None,
                cache_read_tokens: None,
                tool_calls: metadata.get("tool_calls").and_then(|v| v.parse().ok()),
                cost_micros_usd: parse_u64(metadata.get("cost_micros_usd")),
            },
            touched_paths: csv_paths(metadata.get("touched_paths")),
            transcript_refs: Vec::new(),
            attach_hints: HarnessAttachHints { root_actor: true },
            confidence: Some(if matches!(probe_source, ProbeSource::AppProtocol) {
                0.98
            } else if matches!(probe_source, ProbeSource::HookPayload) {
                0.85
            } else {
                0.55
            }),
            probe_source: Some(probe_source.as_str().to_string()),
            ..HarnessProbeResult::default()
        })
    }
}
