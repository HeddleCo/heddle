// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::Value;
use verbs::HarnessKind;

use super::{
    HarnessActorProbe, HarnessAttachHints, HarnessProbeInput, HarnessProbeResult, ProbeSource,
    argv_matches_harness, argv_value, attribution_env_hint, csv_paths, parse_u64,
};

pub(crate) struct CodexProbe;

/// Resolve the effective model from the durable rollout for the Codex thread
/// that launched this command. Missing or mismatched session evidence stays
/// empty so attribution never turns a provider-only detection into a guess.
pub(crate) fn codex_session_probe_metadata(
    env_hints: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let Some(thread_id) = env_hints.get("CODEX_THREAD_ID") else {
        return BTreeMap::new();
    };
    let Some(codex_home) = codex_home(env_hints) else {
        return BTreeMap::new();
    };
    let Some(path) = codex_rollout_path(&codex_home, thread_id) else {
        return BTreeMap::new();
    };
    read_codex_session_metadata(&path, thread_id).unwrap_or_default()
}

fn codex_home(env_hints: &BTreeMap<String, String>) -> Option<PathBuf> {
    env_hints
        .get("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".codex")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| Path::new(&home).join(".codex")))
}

fn codex_rollout_path(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    ingest::transcript::locator::codex_sessions(codex_home, None)
        .ok()?
        .into_iter()
        .rev()
        .find(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.ends_with(thread_id))
        })
}

fn read_codex_session_metadata(
    path: &Path,
    thread_id: &str,
) -> std::io::Result<BTreeMap<String, String>> {
    let reader = BufReader::new(File::open(path)?);
    let mut matched_session = false;
    let mut metadata = BTreeMap::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = event.get("payload").and_then(Value::as_object);
        match event.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                matched_session = payload
                    .and_then(|payload| payload.get("id"))
                    .and_then(Value::as_str)
                    == Some(thread_id);
                if let Some(provider) = payload
                    .and_then(|payload| payload.get("model_provider"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    metadata.insert("model_provider".to_string(), provider.to_string());
                }
            }
            Some("turn_context") => {
                if let Some(model) = payload
                    .and_then(|payload| payload.get("model"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    metadata.insert("model".to_string(), model.to_string());
                }
                if let Some(effort) = payload
                    .and_then(|payload| payload.get("effort"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    metadata.insert("model_reasoning_effort".to_string(), effort.to_string());
                }
            }
            _ => {}
        }
    }

    if matched_session {
        Ok(metadata)
    } else {
        Ok(BTreeMap::new())
    }
}

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
            || argv_matches_harness(input, HarnessKind::Codex)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_thread_resolves_latest_model_from_its_rollout() {
        let temp = tempfile::TempDir::new().unwrap();
        let thread_id = "019fbc09-7051-79e1-b13c-3a55b72fa811";
        let session_dir = temp.path().join("sessions/2026/08/01");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join(format!("rollout-2026-08-01T08-36-03-{thread_id}.jsonl")),
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"model_provider\":\"openai\"}}}}\n",
                    "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.6-terra\",\"effort\":\"medium\"}}}}\n",
                    "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.6-sol\",\"effort\":\"high\"}}}}\n"
                ),
                thread_id
            ),
        )
        .unwrap();

        let env_hints = BTreeMap::from([
            ("CODEX_THREAD_ID".to_string(), thread_id.to_string()),
            ("CODEX_HOME".to_string(), temp.path().display().to_string()),
        ]);

        let metadata = codex_session_probe_metadata(&env_hints);
        assert_eq!(
            metadata.get("model_provider").map(String::as_str),
            Some("openai")
        );
        assert_eq!(
            metadata.get("model").map(String::as_str),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            metadata.get("model_reasoning_effort").map(String::as_str),
            Some("high")
        );
    }

    #[test]
    fn codex_session_metadata_requires_a_thread_marker() {
        assert!(codex_session_probe_metadata(&BTreeMap::new()).is_empty());
    }
}
