// SPDX-License-Identifier: Apache-2.0
//! Attribution resolution shared by hosted sync and the CLI verbs.
//!
//! `resolve_attribution` mirrors the snapshot path's principal precedence
//! (env > repo > user > Unknown) and attaches the ambient agent from the same
//! env/repo lookup `Repository::resolve_agent` performs. The only piece it
//! cannot own itself is detecting the wrapping harness (codex/claude/...);
//! the CLI installs that probe once at startup via [`install_harness_probe`].
//! An absent probe degrades exactly like "no harness detected".

use std::sync::OnceLock;

use anyhow::Result;
use objects::object::{Attribution, Principal};
use repo::Repository;

type HarnessProbe = fn(&Repository) -> (Option<String>, Option<String>);

static HARNESS_PROBE: OnceLock<HarnessProbe> = OnceLock::new();

/// Install the CLI's process-harness detector. Call once at startup.
pub fn install_harness_probe(probe: HarnessProbe) {
    let _ = HARNESS_PROBE.set(probe);
}

fn current_process_harness(repo: &Repository) -> (Option<String>, Option<String>) {
    HARNESS_PROBE
        .get()
        .map_or_else(|| (None, None), |probe| probe(repo))
}

/// Treat the `"unknown"` harness placeholder and empty/whitespace
/// strings as absent so they don't beat real attribution values in
/// precedence chains.
pub fn clean_attribution_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(value)
    }
}

pub fn resolve_principal(repo: &Repository, user_config: &config::UserConfig) -> Result<Principal> {
    Ok(verbs::resolve_principal(repo, user_config.principal_pair())?.principal)
}

/// Resolve the human + agent attribution for a non-capture command (context,
/// fork, collapse, etc.).
///
/// Differs from the snapshot path in two ways — both intentional: it does not
/// honor explicit `--agent-*` flag overrides (other commands don't expose
/// those), and it does not consult the active `heddle agent provenance` chain.
/// Use the snapshot path's full `resolve_*` for capture flows.
pub fn resolve_attribution(
    repo: &Repository,
    user_config: &config::UserConfig,
) -> Result<Attribution> {
    let principal = resolve_principal(repo, user_config)?;
    let (harness_provider_raw, harness_model_raw) = current_process_harness(repo);
    let harness_provider = harness_provider_raw.and_then(clean_attribution_value);
    let harness_model = harness_model_raw.and_then(clean_attribution_value);
    let agent_provider = std::env::var("HEDDLE_AGENT_PROVIDER")
        .ok()
        .and_then(clean_attribution_value)
        .or(harness_provider)
        .or_else(|| {
            user_config
                .agent
                .provider
                .clone()
                .and_then(clean_attribution_value)
        })
        .or_else(|| {
            repo.config()
                .agent
                .provider
                .clone()
                .and_then(clean_attribution_value)
        });
    let agent_model = std::env::var("HEDDLE_AGENT_MODEL")
        .ok()
        .and_then(clean_attribution_value)
        .or(harness_model)
        .or_else(|| {
            user_config
                .agent
                .model
                .clone()
                .and_then(clean_attribution_value)
        })
        .or_else(|| {
            repo.config()
                .agent
                .model
                .clone()
                .and_then(clean_attribution_value)
        });
    match (agent_provider, agent_model) {
        (Some(provider), Some(model)) => {
            let agent = objects::object::Agent::new(provider, model);
            Ok(Attribution::with_agent(principal, agent))
        }
        _ => Ok(Attribution::human(principal)),
    }
}
