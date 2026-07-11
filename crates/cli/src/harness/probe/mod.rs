// SPDX-License-Identifier: Apache-2.0
use std::collections::BTreeMap;

use anyhow::Result;
use heddle_core::decide_harness_probe;
use wire::{TranscriptAttachmentRef, UsageTotals};

use crate::attribution::clean_attribution_value;

mod claude_code;
mod codex;
mod opencode;

pub(crate) use claude_code::ClaudeCodeProbe;
pub(crate) use codex::CodexProbe;
pub(crate) use opencode::OpenCodeProbe;

#[derive(Debug, Clone, Default)]
pub(crate) struct HarnessProbeInput {
    pub argv: Option<Vec<String>>,
    pub env_hints: BTreeMap<String, String>,
    pub explicit_harness: Option<String>,
    pub explicit_provider: Option<String>,
    pub explicit_model: Option<String>,
    pub explicit_thinking_level: Option<String>,
    pub explicit_policy: Option<String>,
    pub probe_metadata: BTreeMap<String, String>,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_policy: Option<String>,
    #[allow(dead_code)]
    pub repo_root: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HarnessAttachHints {
    pub root_actor: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HarnessProbeResult {
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub policy: Option<String>,
    pub native_actor_key: Option<String>,
    pub native_parent_actor_key: Option<String>,
    pub native_instance_key: Option<String>,
    #[allow(dead_code)]
    pub thread_hint: Option<String>,
    pub usage_totals: UsageTotals,
    pub touched_paths: Vec<String>,
    pub transcript_refs: Vec<TranscriptAttachmentRef>,
    pub attach_hints: HarnessAttachHints,
    pub confidence: Option<f32>,
    pub probe_source: Option<String>,
}

pub(crate) trait HarnessActorProbe {
    fn harness_name(&self) -> &'static str;
    fn matches(&self, input: &HarnessProbeInput) -> bool;
    fn probe(&self, input: &HarnessProbeInput) -> Result<HarnessProbeResult>;
}

pub(crate) fn probe_harness_actor(input: &HarnessProbeInput) -> Result<HarnessProbeResult> {
    let probes: [&dyn HarnessActorProbe; 3] = [&CodexProbe, &OpenCodeProbe, &ClaudeCodeProbe];
    if let Some(explicit) = input.explicit_harness.as_deref()
        && let Some(probe) = probes
            .into_iter()
            .find(|probe| probe.harness_name() == explicit)
    {
        return probe.probe(input);
    }
    let probes: [&dyn HarnessActorProbe; 3] = [&CodexProbe, &OpenCodeProbe, &ClaudeCodeProbe];
    if let Some(probe) = probes.into_iter().find(|probe| probe.matches(input)) {
        return probe.probe(input);
    }
    Ok(generic_probe(input))
}

fn generic_probe(input: &HarnessProbeInput) -> HarnessProbeResult {
    let decision = decide_harness_probe(
        input.explicit_harness.as_deref(),
        input.argv.as_deref(),
        &input.env_hints,
    );
    let fingerprint = decision.fingerprint;
    let probe_source = if input.explicit_harness.is_some() {
        ProbeSource::ExplicitPayload
    } else {
        ProbeSource::ArgvEnv
    };
    HarnessProbeResult {
        harness: fingerprint.harness,
        provider: input
            .explicit_provider
            .clone()
            .or(fingerprint.provider)
            .or_else(|| input.current_provider.clone()),
        model: input
            .explicit_model
            .clone()
            .or(fingerprint.model)
            .or_else(|| input.current_model.clone()),
        thinking_level: input
            .explicit_thinking_level
            .clone()
            .or(fingerprint.thinking_level),
        policy: input
            .explicit_policy
            .clone()
            .or(fingerprint.policy)
            .or_else(|| input.current_policy.clone()),
        confidence: Some(decision.confidence),
        probe_source: Some(probe_source.as_str().to_string()),
        ..HarnessProbeResult::default()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProbeSource {
    ExplicitPayload,
    AppProtocol,
    HookPayload,
    StatusPayload,
    SseOrRest,
    ArgvEnv,
    #[allow(dead_code)]
    ConfigOverride,
}

impl ProbeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitPayload => "explicit_payload",
            Self::AppProtocol => "app_protocol",
            Self::HookPayload => "hook_payload",
            Self::StatusPayload => "status_payload",
            Self::SseOrRest => "sse_or_rest",
            Self::ArgvEnv => "argv_env",
            Self::ConfigOverride => "config_override",
        }
    }
}

pub(crate) fn attribution_env_hint(
    env_hints: &BTreeMap<String, String>,
    key: &str,
) -> Option<String> {
    env_hints
        .get(key)
        .cloned()
        .and_then(clean_attribution_value)
}

pub(crate) fn argv_value(argv: &[String], flag: &str) -> Option<String> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&(flag.to_string() + "=")) {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn csv_paths(value: Option<&String>) -> Vec<String> {
    value
        .map(|raw| {
            raw.split(',')
                .map(|path| path.trim().replace('\\', "/"))
                .filter(|path| !path.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_u64(value: Option<&String>) -> Option<u64> {
    value.and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_thread_env_identifies_codex_actor() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("CODEX_THREAD_ID".to_string(), "thread-123".to_string());
        env_hints.insert("CODEX_MODEL".to_string(), "gpt-5.5".to_string());
        env_hints.insert("CODEX_REASONING_EFFORT".to_string(), "xhigh".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("codex"));
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(result.thinking_level.as_deref(), Some("xhigh"));
        assert_eq!(
            result.native_actor_key.as_deref(),
            Some("codex:thread:thread-123")
        );
    }

    #[test]
    fn codex_current_provider_wins_before_default_provider() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("CODEX_THREAD_ID".to_string(), "thread-123".to_string());
        env_hints.insert("OPENAI_MODEL".to_string(), "gpt-5.3-codex".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            current_provider: Some("openai-compatible".to_string()),
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("codex"));
        assert_eq!(result.provider.as_deref(), Some("openai-compatible"));
        assert_eq!(result.model.as_deref(), Some("gpt-5.3-codex"));
    }

    #[test]
    fn generic_harness_probe_keeps_heddle_agent_env_identity() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("HEDDLE_AGENT_PROVIDER".to_string(), "custom-ai".to_string());
        env_hints.insert("HEDDLE_AGENT_MODEL".to_string(), "custom-model".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness, None);
        assert_eq!(result.provider.as_deref(), Some("custom-ai"));
        assert_eq!(result.model.as_deref(), Some("custom-model"));
    }

    #[test]
    fn explicit_heddle_agent_env_wins_over_detected_claude_identity() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("CLAUDECODE".to_string(), "1".to_string());
        env_hints.insert("HEDDLE_AGENT_PROVIDER".to_string(), "openai".to_string());
        env_hints.insert("HEDDLE_AGENT_MODEL".to_string(), "gpt-5-codex".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            probe_metadata: BTreeMap::from([(
                "model".to_string(),
                "claude-opus-4-8[1m]".to_string(),
            )]),
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("claude-code"));
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.model.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn blank_heddle_agent_env_falls_through_to_detected_claude_identity() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("CLAUDECODE".to_string(), "1".to_string());
        env_hints.insert("HEDDLE_AGENT_PROVIDER".to_string(), "anthropic".to_string());
        env_hints.insert("HEDDLE_AGENT_MODEL".to_string(), String::new());
        env_hints.insert("HEDDLE_AGENT_POLICY".to_string(), "unknown".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            explicit_harness: Some("claude-code".to_string()),
            probe_metadata: BTreeMap::from([
                ("model".to_string(), "claude-opus-4-8[1m]".to_string()),
                ("session_id".to_string(), "claude-sess-1".to_string()),
            ]),
            current_policy: Some("detected-policy".to_string()),
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("claude-code"));
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-opus-4-8[1m]"));
        assert_eq!(result.policy.as_deref(), Some("detected-policy"));
    }

    #[test]
    fn argv_parent_hint_identifies_claude_code_actor() {
        let result = probe_harness_actor(&HarnessProbeInput {
            argv: Some(vec![
                "/home/user/.local/bin/claude".to_string(),
                "--model".to_string(),
                "claude-opus-4-7".to_string(),
            ]),
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("claude-code"));
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn claude_code_env_model_fills_detected_harness_identity() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("CLAUDECODE".to_string(), "1".to_string());
        env_hints.insert(
            "HEDDLE_AGENT_MODEL".to_string(),
            "claude-opus-4-7".to_string(),
        );
        env_hints.insert("THINKING_LEVEL".to_string(), "xhigh".to_string());

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            current_provider: Some("anthropic".to_string()),
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("claude-code"));
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(result.thinking_level.as_deref(), Some("xhigh"));
    }

    #[test]
    fn opencode_env_model_fills_detected_harness_identity() {
        let mut env_hints = BTreeMap::new();
        env_hints.insert("OPENCODE_CLIENT".to_string(), "desktop".to_string());
        env_hints.insert("OPENCODE_PROVIDER".to_string(), "anthropic".to_string());
        env_hints.insert(
            "OPENCODE_MODEL".to_string(),
            "claude-sonnet-4-6".to_string(),
        );

        let result = probe_harness_actor(&HarnessProbeInput {
            env_hints,
            repo_root: "/tmp/repo".to_string(),
            ..HarnessProbeInput::default()
        })
        .expect("probe should succeed");

        assert_eq!(result.harness.as_deref(), Some("opencode"));
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet-4-6"));
    }
}
