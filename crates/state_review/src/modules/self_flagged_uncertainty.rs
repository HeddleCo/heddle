// SPDX-License-Identifier: Apache-2.0
//! Self-flagged uncertainty: passthrough of agent-emitted flags from the
//! captured state's provenance. Capped per state by config so a noisy
//! agent can't drown other signals.
//!
//! This module is unique in that it doesn't compute anything: it surfaces
//! flags the agent itself deposited at capture time. The wire-up between
//! `State.provenance` and the typed flag list lands with the agent
//! integration — for first ship the module looks at `new.intent` for a
//! magic prefix `"self-flag:"` so ambient agents can emit flags through
//! existing capture metadata without a new schema bump.

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "self_flagged_uncertainty";
const FLAG_PREFIX: &str = "self-flag:";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    _ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.self_flagged_uncertainty.enabled {
        return Vec::new();
    }
    let cap = cfg.self_flagged_uncertainty.max_per_state as usize;
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());
    let intent = new.intent.as_deref().unwrap_or("");
    let mut out = Vec::new();
    for line in intent.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(FLAG_PREFIX) else {
            continue;
        };
        if out.len() >= cap {
            break;
        }
        // Format: `self-flag:[<file>:<symbol>] message`. The bracketed
        // anchor is optional; without it the flag attaches to the whole
        // change.
        let (anchor, message) = parse_flag_body(rest.trim());
        let reason = if message.is_empty() {
            "agent flagged uncertainty about its own output".to_string()
        } else {
            truncate_reason(&format!("agent self-flag: {message}"))
        };
        out.push(RiskSignal {
            kind: RiskSignalKind::SelfFlaggedUncertainty,
            anchor,
            reason,
            producer: ProducerId::new(MODULE_ID, VERSION),
            computed_at,
            computed_against: Some(new.change_id),
        });
    }
    out
}

fn parse_flag_body(body: &str) -> (SignalAnchor, &str) {
    if let Some(rest) = body.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let anchor_str = &rest[..end];
        let message = rest[end + 1..].trim_start();
        if let Some((file, symbol)) = anchor_str.split_once(':') {
            return (SignalAnchor::symbol(file, symbol), message);
        }
        return (SignalAnchor::file(anchor_str), message);
    }
    // No bracketed anchor — file-level placeholder. Use a sentinel "*"
    // path so the budgeter can group across the change.
    (SignalAnchor::file("*"), body)
}

use crate::truncate_reason;

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, ContentHash, Principal};

    use super::*;

    fn state_with_intent(intent: &str) -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
        .with_intent(intent)
    }

    #[test]
    fn fires_on_self_flag_prefix() {
        let new = state_with_intent(
            "rewrote auth\nself-flag:[src/auth.rs:verify] not certain about edge case",
        );
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&state_with_intent(""), &new, &cfg, &ctx);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, RiskSignalKind::SelfFlaggedUncertainty);
        assert_eq!(signals[0].anchor.file, "src/auth.rs");
        assert_eq!(signals[0].anchor.symbol.as_deref(), Some("verify"));
        assert!(signals[0].reason.contains("not certain"));
    }

    #[test]
    fn quiet_when_no_self_flag() {
        let new = state_with_intent("plain capture without flags");
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&state_with_intent(""), &new, &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn cap_enforced() {
        let intent = (0..10)
            .map(|i| format!("self-flag:[a.rs:fn{i}] bug {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = state_with_intent(&intent);
        let cfg = ReviewSignalsConfig::default(); // default cap = 5
        let ctx = SemanticContext::new();
        let signals = run(&state_with_intent(""), &new, &cfg, &ctx);
        assert_eq!(signals.len(), 5);
    }

    #[test]
    fn disabled_module_returns_empty() {
        let new = state_with_intent("self-flag:[a:b] uncertain");
        let mut cfg = ReviewSignalsConfig::default();
        cfg.self_flagged_uncertainty.enabled = false;
        let ctx = SemanticContext::new();
        let signals = run(&state_with_intent(""), &new, &cfg, &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn flag_without_anchor_uses_wildcard_file() {
        let new = state_with_intent("self-flag:something is wrong");
        let cfg = ReviewSignalsConfig::default();
        let ctx = SemanticContext::new();
        let signals = run(&state_with_intent(""), &new, &cfg, &ctx);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].anchor.file, "*");
    }
}
