// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use super::{
    HarnessActorProbe, HarnessAttachHints, HarnessProbeInput, HarnessProbeResult, ProbeSource,
    argv_value, csv_paths, parse_u64,
};

pub(crate) struct OpenCodeProbe;

impl HarnessActorProbe for OpenCodeProbe {
    fn harness_name(&self) -> &'static str {
        "opencode"
    }

    fn matches(&self, input: &HarnessProbeInput) -> bool {
        input.explicit_harness.as_deref() == Some(self.harness_name())
            || input.probe_metadata.contains_key("session_id")
            || input.env_hints.contains_key("OPENCODE_CLIENT")
            || input
                .argv
                .as_ref()
                .and_then(|argv| argv.first())
                .is_some_and(|program| program.to_ascii_lowercase().contains("opencode"))
    }

    fn probe(&self, input: &HarnessProbeInput) -> Result<HarnessProbeResult> {
        let metadata = &input.probe_metadata;
        let argv = input.argv.as_deref().unwrap_or(&[]);
        let session_id = metadata
            .get("session_id")
            .cloned()
            .or_else(|| argv_value(argv, "--session"));
        let parent_id = metadata.get("parent_id").cloned();
        let client_name = metadata
            .get("client_name")
            .cloned()
            .or_else(|| input.env_hints.get("OPENCODE_CLIENT").cloned())
            .unwrap_or_else(|| "cli".to_string());
        let server_origin = metadata.get("server_origin").cloned();
        let native_instance_key = Some(match server_origin {
            Some(origin) => format!("opencode:client:{client_name}@{origin}"),
            None => format!("opencode:client:{client_name}"),
        });
        let provider = metadata.get("provider").cloned();
        let model = metadata
            .get("model")
            .cloned()
            .or_else(|| metadata.get("agent_model").cloned())
            .or_else(|| argv_value(argv, "--model"));
        let probe_source = if metadata.get("hook_event").is_some() {
            ProbeSource::HookPayload
        } else if session_id.is_some() {
            ProbeSource::SseOrRest
        } else {
            ProbeSource::ArgvEnv
        };
        Ok(HarnessProbeResult {
            harness: Some("opencode".to_string()),
            provider,
            model,
            native_actor_key: session_id
                .clone()
                .map(|id| format!("opencode:session:{id}")),
            native_parent_actor_key: parent_id.map(|id| format!("opencode:session:{id}")),
            native_instance_key,
            usage_totals: proto::UsageTotals {
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
            attach_hints: HarnessAttachHints {
                root_actor: metadata.get("parent_id").is_none(),
            },
            confidence: Some(if metadata.get("hook_event").is_some() {
                0.95
            } else if session_id.is_some() {
                0.9
            } else {
                0.5
            }),
            probe_source: Some(probe_source.as_str().to_string()),
            ..HarnessProbeResult::default()
        })
    }
}