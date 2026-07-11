// SPDX-License-Identifier: Apache-2.0
//! Pure harness session / segment policy (no FS, registry, or process I/O).
//!
//! Owns:
//! - harness kind fingerprinting from argv / env hint maps
//! - session attach-vs-create decision given caller-gathered probe facts
//! - segment rotation when provider or model changes
//!
//! Process detection, registry lookups, session store I/O, and path
//! canonicalization remain CLI-owned. Callers pass pure facts in and apply
//! the returned decision.

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Harness kind / fingerprint
// ---------------------------------------------------------------------------

/// Known coding-agent harnesses detectable from argv/env hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessKind {
    ClaudeCode,
    Codex,
    OpenCode,
    Aider,
    #[default]
    Unknown,
}

impl HarnessKind {
    /// Stable harness name used in probe/report fields, when known.
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("claude-code"),
            Self::Codex => Some("codex"),
            Self::OpenCode => Some("opencode"),
            Self::Aider => Some("aider"),
            Self::Unknown => None,
        }
    }

    /// Default provider associated with the harness when env does not override.
    pub fn default_provider(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("anthropic"),
            Self::Codex => Some("openai"),
            Self::OpenCode | Self::Aider | Self::Unknown => None,
        }
    }

    /// Parse a harness name string (explicit payload / config).
    pub fn parse_name(name: &str) -> Self {
        match name {
            "claude-code" => Self::ClaudeCode,
            "codex" => Self::Codex,
            "opencode" => Self::OpenCode,
            "aider" => Self::Aider,
            _ => Self::Unknown,
        }
    }
}

/// Pure fingerprint of harness identity derived from argv/env maps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessFingerprint {
    pub kind: HarnessKind,
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub policy: Option<String>,
}

/// Pure probe decision for generic (non-harness-specific) detection.
///
/// Full per-harness probes (hook payloads, native actor keys) stay CLI-owned;
/// this captures the argv/env / explicit-name path used by the generic probe.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessProbeDecision {
    pub fingerprint: HarnessFingerprint,
    /// Baseline confidence for the generic path (1.0 explicit, 0.4 argv/env).
    pub confidence: f32,
    /// Stable probe-source label (`explicit_payload` or `argv_env`).
    pub probe_source: &'static str,
}

/// Detect harness kind from program name + env key presence (pure map).
///
/// Priority matches historical `fingerprint_from_hints`: claude-code, then
/// codex, then opencode, then aider.
pub fn detect_harness_kind(
    program: Option<&str>,
    env_hints: &BTreeMap<String, String>,
) -> HarnessKind {
    let program = program.map(|p| p.to_ascii_lowercase()).unwrap_or_default();

    if program.contains("claude")
        || env_hints.contains_key("CLAUDECODE")
        || env_hints.contains_key("CLAUDE_CODE")
    {
        HarnessKind::ClaudeCode
    } else if program.contains("codex")
        || env_hints.contains_key("CODEX_SANDBOX")
        || env_hints.contains_key("CODEX_THREAD_ID")
        || env_hints.contains_key("CODEX_CI")
    {
        HarnessKind::Codex
    } else if program.contains("opencode") || env_hints.contains_key("OPENCODE_CLIENT") {
        HarnessKind::OpenCode
    } else if program.contains("aider") {
        HarnessKind::Aider
    } else {
        HarnessKind::Unknown
    }
}

/// Pure argv/env fingerprint used by the generic harness probe path.
pub fn fingerprint_harness_from_hints(
    argv: Option<&[String]>,
    env_hints: &BTreeMap<String, String>,
) -> HarnessFingerprint {
    let program = argv.and_then(|args| args.first()).map(String::as_str);
    let kind = detect_harness_kind(program, env_hints);

    let mut fingerprint = HarnessFingerprint {
        kind,
        harness: kind.as_str().map(str::to_string),
        provider: kind.default_provider().map(str::to_string),
        model: None,
        thinking_level: None,
        policy: None,
    };

    // HEDDLE_AGENT_PROVIDER fills only when no harness default was set
    // (historical `or_else` order in the CLI fingerprint).
    fingerprint.provider = fingerprint.provider.or_else(|| {
        env_hints
            .get("HEDDLE_AGENT_PROVIDER")
            .cloned()
            .and_then(clean_attribution_value)
    });
    fingerprint.model = env_hints
        .get("HEDDLE_AGENT_MODEL")
        .cloned()
        .and_then(clean_attribution_value)
        .or_else(|| env_hints.get("CODEX_MODEL").cloned())
        .or_else(|| env_hints.get("CLAUDE_MODEL").cloned())
        .or_else(|| env_hints.get("ANTHROPIC_MODEL").cloned())
        .or_else(|| env_hints.get("OPENAI_MODEL").cloned())
        .or_else(|| env_hints.get("OPENCODE_MODEL").cloned())
        .or_else(|| env_hints.get("AIDER_MODEL").cloned())
        .or_else(|| env_hints.get("MODEL").cloned());
    fingerprint.thinking_level = env_hints
        .get("THINKING_LEVEL")
        .cloned()
        .or_else(|| env_hints.get("CODEX_REASONING_EFFORT").cloned())
        .or_else(|| env_hints.get("REASONING_EFFORT").cloned())
        .or_else(|| env_hints.get("OPENAI_REASONING_EFFORT").cloned());
    fingerprint.policy = env_hints
        .get("HEDDLE_AGENT_POLICY")
        .cloned()
        .and_then(clean_attribution_value)
        .or_else(|| env_hints.get("PROMPT_POLICY").cloned());

    fingerprint
}

/// Decide the generic probe outcome from explicit harness name + argv/env.
///
/// An explicit harness name overrides the fingerprint harness label but does
/// **not** invent a default provider — that matches the historical generic
/// probe (`explicit.or(fingerprint)` for harness only).
pub fn decide_harness_probe(
    explicit_harness: Option<&str>,
    argv: Option<&[String]>,
    env_hints: &BTreeMap<String, String>,
) -> HarnessProbeDecision {
    let mut fingerprint = fingerprint_harness_from_hints(argv, env_hints);
    if let Some(name) = explicit_harness {
        fingerprint.kind = HarnessKind::parse_name(name);
        fingerprint.harness = Some(name.to_string());
    }
    let explicit = explicit_harness.is_some();
    HarnessProbeDecision {
        fingerprint,
        confidence: if explicit { 1.0 } else { 0.4 },
        probe_source: if explicit {
            "explicit_payload"
        } else {
            "argv_env"
        },
    }
}

/// Treat empty / `"unknown"` attribution placeholders as absent.
fn clean_attribution_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(value)
    }
}

// ---------------------------------------------------------------------------
// Session attach / create policy
// ---------------------------------------------------------------------------

/// Winning attach/create rule labels (stable machine strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAttachRule {
    ExplicitAgentSession,
    ExplicitHeddleSession,
    NativeActorKey,
    ClientInstanceId,
    NativeInstanceKey,
    CurrentWorktreeSession,
    TokenSid,
    CreateNewSession,
}

impl SessionAttachRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitAgentSession => "explicit-agent-session",
            Self::ExplicitHeddleSession => "explicit-heddle-session",
            Self::NativeActorKey => "native-actor-key",
            Self::ClientInstanceId => "client-instance-id",
            Self::NativeInstanceKey => "native-instance-key",
            Self::CurrentWorktreeSession => "current-worktree-session",
            Self::TokenSid => "token-sid",
            Self::CreateNewSession => "create-new-session",
        }
    }
}

/// Pure session policy: attach to an existing active session or create one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPolicy {
    AttachExisting {
        session_id: String,
        rule: SessionAttachRule,
    },
    CreateNew {
        because_claimed: bool,
        rule: SessionAttachRule,
    },
}

/// Full attach decision including precedence trail for reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachDecision {
    pub policy: SessionPolicy,
    pub winning_rule: &'static str,
    pub attach_reason: String,
    pub precedence: Vec<String>,
}

/// Soft lookup outcome for a single attach rule (CLI performed the I/O).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionLookupFact {
    /// Rule not applicable (no key/id to look up).
    #[default]
    NotProvided,
    /// Key was present; no active compatible match.
    Miss { key: String },
    /// Active session found for the key.
    Hit { key: String, session_id: String },
}

/// Hard-bind from an explicit agent registry entry (already validated active).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitAgentBind {
    pub agent_session_id: String,
    pub heddle_session_id: String,
}

/// Current worktree session candidate after claim checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WorktreeSessionFact {
    #[default]
    None,
    Available {
        session_id: String,
    },
    Claimed {
        session_id: String,
    },
}

/// Token-claim `sid` candidate after claim checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TokenSidFact {
    #[default]
    None,
    Available {
        session_id: String,
    },
    Claimed {
        session_id: String,
    },
}

/// Caller-gathered facts for pure session attach policy (no I/O).
///
/// Hard binds (`explicit_agent`, `explicit_heddle_session_id`) must already be
/// validated as active sessions by the caller; invalid binds should error
/// before calling [`decide_session_attach`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionAttachFacts {
    pub explicit_agent: Option<ExplicitAgentBind>,
    pub explicit_heddle_session_id: Option<String>,
    pub native_actor: SessionLookupFact,
    pub client_instance: SessionLookupFact,
    pub native_instance: SessionLookupFact,
    /// Probe attach hint: root actors may reuse the current worktree session.
    pub root_actor: bool,
    pub current_worktree: WorktreeSessionFact,
    pub token_sid: TokenSidFact,
}

/// Pure attach/create decision matching harness open_session precedence.
pub fn decide_session_attach(facts: &SessionAttachFacts) -> SessionAttachDecision {
    let mut precedence = Vec::new();

    if let Some(bind) = &facts.explicit_agent {
        precedence.push(format!(
            "explicit-agent-session:{}:matched",
            bind.agent_session_id
        ));
        return attach_decision(
            &bind.heddle_session_id,
            SessionAttachRule::ExplicitAgentSession,
            format!(
                "reattached actor {} to existing Heddle session {}",
                bind.agent_session_id, bind.heddle_session_id
            ),
            precedence,
        );
    }
    precedence.push("explicit-agent-session:miss".to_string());

    if let Some(session_id) = facts.explicit_heddle_session_id.as_deref() {
        precedence.push(format!("explicit-heddle-session:{session_id}:matched"));
        return attach_decision(
            session_id,
            SessionAttachRule::ExplicitHeddleSession,
            format!("attached to explicit Heddle session {session_id}"),
            precedence,
        );
    }
    precedence.push("explicit-heddle-session:miss".to_string());

    // Native actor key is only consulted when no client_instance_id is present
    // (stronger client-instance identity takes priority otherwise).
    let client_instance_provided = !matches!(facts.client_instance, SessionLookupFact::NotProvided);
    if !client_instance_provided {
        match &facts.native_actor {
            SessionLookupFact::Hit { key, session_id } => {
                precedence.push(format!("native-actor-key:{key}:matched"));
                return attach_decision(
                    session_id,
                    SessionAttachRule::NativeActorKey,
                    format!("reattached native actor {key} to Heddle session {session_id}"),
                    precedence,
                );
            }
            SessionLookupFact::Miss { key } => {
                precedence.push(format!("native-actor-key:{key}:miss"));
            }
            SessionLookupFact::NotProvided => {
                precedence.push("native-actor-key:miss".to_string());
            }
        }
    } else {
        precedence.push("native-actor-key:miss".to_string());
    }

    match &facts.client_instance {
        SessionLookupFact::Hit { key, session_id } => {
            precedence.push(format!("client-instance-id:{key}:matched"));
            return attach_decision(
                session_id,
                SessionAttachRule::ClientInstanceId,
                format!("reattached client instance {key} to Heddle session {session_id}"),
                precedence,
            );
        }
        SessionLookupFact::Miss { key } => {
            precedence.push(format!("client-instance-id:{key}:miss"));
            return create_decision(
                false,
                format!("started new Heddle session for distinct client instance {key}"),
                precedence,
            );
        }
        SessionLookupFact::NotProvided => {
            precedence.push("client-instance-id:miss".to_string());
        }
    }

    // Strong native actor key without a match → create (do not fall through to
    // weaker native-instance / worktree reuse). Hit already returned above.
    if !client_instance_provided && matches!(facts.native_actor, SessionLookupFact::Miss { .. }) {
        precedence.push("native-instance-key:skipped-strong-native-key".to_string());
        return create_decision(
            false,
            "started new Heddle session because no compatible native actor match was found"
                .to_string(),
            precedence,
        );
    }

    match &facts.native_instance {
        SessionLookupFact::Hit { key, session_id } => {
            precedence.push(format!("native-instance-key:{key}:matched"));
            return attach_decision(
                session_id,
                SessionAttachRule::NativeInstanceKey,
                format!("reattached native instance {key} to Heddle session {session_id}"),
                precedence,
            );
        }
        SessionLookupFact::Miss { key } => {
            precedence.push(format!("native-instance-key:{key}:miss"));
        }
        SessionLookupFact::NotProvided => {
            precedence.push("native-instance-key:miss".to_string());
        }
    }

    if facts.root_actor {
        match &facts.current_worktree {
            WorktreeSessionFact::Available { session_id } => {
                precedence.push(format!("current-worktree-session:{session_id}:matched"));
                return attach_decision(
                    session_id,
                    SessionAttachRule::CurrentWorktreeSession,
                    format!("attached to active worktree Heddle session {session_id}"),
                    precedence,
                );
            }
            WorktreeSessionFact::Claimed { session_id } => {
                precedence.push(format!("current-worktree-session:{session_id}:claimed"));
                return create_decision(
                    true,
                    "started a new Heddle session because the current session was already claimed by another active actor".to_string(),
                    precedence,
                );
            }
            WorktreeSessionFact::None => {
                precedence.push("current-worktree-session:miss".to_string());
            }
        }
    } else {
        precedence.push("current-worktree-session:miss".to_string());
    }

    match &facts.token_sid {
        TokenSidFact::Available { session_id } => {
            precedence.push(format!("token-sid:{session_id}:matched"));
            return attach_decision(
                session_id,
                SessionAttachRule::TokenSid,
                format!("attached to Heddle session {session_id} from auth token sid"),
                precedence,
            );
        }
        TokenSidFact::Claimed { session_id } => {
            precedence.push(format!("token-sid:{session_id}:claimed"));
            return create_decision(
                true,
                "started a new Heddle session because the current session was already claimed by another active actor".to_string(),
                precedence,
            );
        }
        TokenSidFact::None => {
            precedence.push("token-sid:miss".to_string());
        }
    }

    create_decision(false, "started new Heddle session".to_string(), precedence)
}

fn attach_decision(
    session_id: &str,
    rule: SessionAttachRule,
    attach_reason: String,
    precedence: Vec<String>,
) -> SessionAttachDecision {
    SessionAttachDecision {
        policy: SessionPolicy::AttachExisting {
            session_id: session_id.to_string(),
            rule,
        },
        winning_rule: rule.as_str(),
        attach_reason,
        precedence,
    }
}

fn create_decision(
    because_claimed: bool,
    attach_reason: String,
    precedence: Vec<String>,
) -> SessionAttachDecision {
    let rule = SessionAttachRule::CreateNewSession;
    SessionAttachDecision {
        policy: SessionPolicy::CreateNew {
            because_claimed,
            rule,
        },
        winning_rule: rule.as_str(),
        attach_reason,
        precedence,
    }
}

// ---------------------------------------------------------------------------
// Segment rotation
// ---------------------------------------------------------------------------

/// Whether the current session segment should rotate for a new identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRotation {
    Keep,
    Rotate,
}

/// Pure segment rotation policy: rotate when provider or model changes.
///
/// - No current segment → keep (caller creates the first segment elsewhere).
/// - `new_*` is `None` → does not force rotation (blank hints fall through).
/// - Rotation only when a new value is present and differs from current.
pub fn segment_rotation_policy(
    current_provider: Option<&str>,
    current_model: Option<&str>,
    new_provider: Option<&str>,
    new_model: Option<&str>,
) -> SegmentRotation {
    let Some(current_provider) = current_provider else {
        return SegmentRotation::Keep;
    };
    // Model may be missing on a segment only if caller has no current segment;
    // when a segment exists both provider and model are set. Treat missing
    // current model as empty for comparison only when provider was present.
    let current_model = current_model.unwrap_or("");

    let provider_changed = new_provider.is_some_and(|p| p != current_provider);
    let model_changed = new_model.is_some_and(|m| m != current_model);
    if provider_changed || model_changed {
        SegmentRotation::Rotate
    } else {
        SegmentRotation::Keep
    }
}

/// Convenience bool wrapper for [`segment_rotation_policy`].
pub fn should_rotate_segment(
    current_provider: Option<&str>,
    current_model: Option<&str>,
    new_provider: Option<&str>,
    new_model: Option<&str>,
) -> bool {
    matches!(
        segment_rotation_policy(current_provider, current_model, new_provider, new_model),
        SegmentRotation::Rotate
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn detect_harness_kind_from_env_and_program() {
        assert_eq!(
            detect_harness_kind(None, &env(&[("CLAUDECODE", "1")])),
            HarnessKind::ClaudeCode
        );
        assert_eq!(
            detect_harness_kind(Some("/usr/bin/codex"), &BTreeMap::new()),
            HarnessKind::Codex
        );
        assert_eq!(
            detect_harness_kind(None, &env(&[("OPENCODE_CLIENT", "desktop")])),
            HarnessKind::OpenCode
        );
        assert_eq!(
            detect_harness_kind(Some("aider"), &BTreeMap::new()),
            HarnessKind::Aider
        );
        assert_eq!(
            detect_harness_kind(Some("bash"), &BTreeMap::new()),
            HarnessKind::Unknown
        );
    }

    #[test]
    fn fingerprint_prefers_claude_over_codex_env_when_program_is_claude() {
        let fp = fingerprint_harness_from_hints(
            Some(&["claude".to_string()]),
            &env(&[("CODEX_THREAD_ID", "t1"), ("CLAUDECODE", "1")]),
        );
        assert_eq!(fp.kind, HarnessKind::ClaudeCode);
        assert_eq!(fp.harness.as_deref(), Some("claude-code"));
        assert_eq!(fp.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn fingerprint_reads_model_and_thinking_env() {
        let fp = fingerprint_harness_from_hints(
            None,
            &env(&[
                ("CODEX_THREAD_ID", "t1"),
                ("CODEX_MODEL", "gpt-5.5"),
                ("CODEX_REASONING_EFFORT", "xhigh"),
            ]),
        );
        assert_eq!(fp.kind, HarnessKind::Codex);
        assert_eq!(fp.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(fp.thinking_level.as_deref(), Some("xhigh"));
        assert_eq!(fp.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn fingerprint_strips_blank_heddle_agent_env() {
        let fp = fingerprint_harness_from_hints(
            None,
            &env(&[
                ("HEDDLE_AGENT_PROVIDER", "custom"),
                ("HEDDLE_AGENT_MODEL", ""),
                ("HEDDLE_AGENT_POLICY", "unknown"),
                ("MODEL", "fallback-model"),
            ]),
        );
        assert_eq!(fp.provider.as_deref(), Some("custom"));
        assert_eq!(fp.model.as_deref(), Some("fallback-model"));
        assert_eq!(fp.policy, None);
    }

    #[test]
    fn decide_harness_probe_explicit_vs_argv() {
        let explicit = decide_harness_probe(Some("codex"), None, &BTreeMap::new());
        assert_eq!(explicit.confidence, 1.0);
        assert_eq!(explicit.probe_source, "explicit_payload");
        assert_eq!(explicit.fingerprint.harness.as_deref(), Some("codex"));
        // Explicit name alone does not invent a default provider.
        assert_eq!(explicit.fingerprint.provider, None);

        let argv = decide_harness_probe(None, Some(&["/bin/claude".to_string()]), &BTreeMap::new());
        assert_eq!(argv.confidence, 0.4);
        assert_eq!(argv.probe_source, "argv_env");
        assert_eq!(argv.fingerprint.kind, HarnessKind::ClaudeCode);
        assert_eq!(argv.fingerprint.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn segment_rotates_on_provider_or_model_change_only() {
        assert!(!should_rotate_segment(
            Some("anthropic"),
            Some("opus"),
            Some("anthropic"),
            Some("opus"),
        ));
        assert!(should_rotate_segment(
            Some("anthropic"),
            Some("opus"),
            Some("openai"),
            Some("opus"),
        ));
        assert!(should_rotate_segment(
            Some("anthropic"),
            Some("opus"),
            Some("anthropic"),
            Some("sonnet"),
        ));
        // Blank new values do not force rotation.
        assert!(!should_rotate_segment(
            Some("anthropic"),
            Some("opus"),
            None,
            None,
        ));
        // No current segment → keep.
        assert!(!should_rotate_segment(
            None,
            None,
            Some("anthropic"),
            Some("opus")
        ));
        assert_eq!(
            segment_rotation_policy(
                Some("anthropic"),
                Some("opus"),
                Some("anthropic"),
                Some("sonnet"),
            ),
            SegmentRotation::Rotate
        );
    }

    #[test]
    fn session_attach_explicit_agent_wins() {
        let decision = decide_session_attach(&SessionAttachFacts {
            explicit_agent: Some(ExplicitAgentBind {
                agent_session_id: "agent-1".into(),
                heddle_session_id: "sess-1".into(),
            }),
            explicit_heddle_session_id: Some("sess-other".into()),
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            decision.policy,
            SessionPolicy::AttachExisting {
                session_id: "sess-1".into(),
                rule: SessionAttachRule::ExplicitAgentSession,
            }
        );
        assert_eq!(decision.winning_rule, "explicit-agent-session");
        assert!(decision.precedence[0].contains("matched"));
    }

    #[test]
    fn session_attach_client_instance_miss_creates() {
        let decision = decide_session_attach(&SessionAttachFacts {
            client_instance: SessionLookupFact::Miss {
                key: "cli-2".into(),
            },
            native_actor: SessionLookupFact::Hit {
                key: "codex:thread:t".into(),
                session_id: "should-not-use".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            decision.policy,
            SessionPolicy::CreateNew {
                because_claimed: false,
                rule: SessionAttachRule::CreateNewSession,
            }
        );
        assert!(decision.attach_reason.contains("cli-2"));
        // Native actor is skipped when client_instance is provided.
        assert!(
            decision
                .precedence
                .iter()
                .any(|p| p == "native-actor-key:miss")
        );
    }

    #[test]
    fn session_attach_native_actor_hit_and_miss() {
        let hit = decide_session_attach(&SessionAttachFacts {
            native_actor: SessionLookupFact::Hit {
                key: "codex:thread:t1".into(),
                session_id: "sess-a".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            hit.policy,
            SessionPolicy::AttachExisting {
                session_id: "sess-a".into(),
                rule: SessionAttachRule::NativeActorKey,
            }
        );

        let miss = decide_session_attach(&SessionAttachFacts {
            native_actor: SessionLookupFact::Miss {
                key: "codex:thread:t2".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert!(matches!(miss.policy, SessionPolicy::CreateNew { .. }));
        assert!(
            miss.precedence
                .iter()
                .any(|p| p == "native-instance-key:skipped-strong-native-key")
        );
    }

    #[test]
    fn session_attach_worktree_and_token_claim_paths() {
        let available = decide_session_attach(&SessionAttachFacts {
            root_actor: true,
            current_worktree: WorktreeSessionFact::Available {
                session_id: "wt-1".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            available.policy,
            SessionPolicy::AttachExisting {
                session_id: "wt-1".into(),
                rule: SessionAttachRule::CurrentWorktreeSession,
            }
        );

        let claimed = decide_session_attach(&SessionAttachFacts {
            root_actor: true,
            current_worktree: WorktreeSessionFact::Claimed {
                session_id: "wt-2".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            claimed.policy,
            SessionPolicy::CreateNew {
                because_claimed: true,
                rule: SessionAttachRule::CreateNewSession,
            }
        );

        let token = decide_session_attach(&SessionAttachFacts {
            token_sid: TokenSidFact::Available {
                session_id: "tok-1".into(),
            },
            ..SessionAttachFacts::default()
        });
        assert_eq!(
            token.policy,
            SessionPolicy::AttachExisting {
                session_id: "tok-1".into(),
                rule: SessionAttachRule::TokenSid,
            }
        );
    }

    #[test]
    fn session_attach_default_creates_new() {
        let decision = decide_session_attach(&SessionAttachFacts::default());
        assert_eq!(
            decision.policy,
            SessionPolicy::CreateNew {
                because_claimed: false,
                rule: SessionAttachRule::CreateNewSession,
            }
        );
        assert_eq!(decision.attach_reason, "started new Heddle session");
        assert_eq!(decision.winning_rule, "create-new-session");
    }
}
