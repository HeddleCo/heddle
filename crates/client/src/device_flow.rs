//! Client-side Biscuit attenuation helpers for the agent flow.
//!
//! Spawning a sub-agent in Heddle doesn't require a server round trip:
//! the parent process appends an attenuation block to its own
//! Biscuit and hands the resulting bytes to the agent. The verifier
//! enforces every block's checks on every request, so an attenuated
//! token can only ever be a strict subset of the parent's authority.
//!
//! See `.agents/agent-attenuation.md` for cookbook recipes (read-only
//! agent, single-repo agent, time-bounded inspector, sub-sub-agent
//! chain).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Operations that an agent attenuation can never authorize, even when a
/// caller constructs [`AgentAttenuation`] directly without an allowlist.
///
/// Credential-issuing and destructive methods excluded from the W1 agent
/// operation surface. This floor limits use of the attenuated token, but it
/// cannot constrain device-key-authenticated `MintBiscuit`; closing that G4
/// laundering path requires W2 delegated PoP.
pub const AGENT_OPERATION_DENY_FLOOR: &[&str] = &[
    "CreateServiceAccount",
    "IssueServiceAccountCredential",
    "DeleteRepository",
    "DeleteNamespace",
];

/// Curated W1 operation ceiling for `heddle auth derive-agent`.
///
/// `--allow` may select a subset of these methods. Parent and child blocks are
/// both evaluated by the server, so sub-derivation computes an intersection
/// and cannot widen an ancestor's selection.
pub const SAFE_AGENT_OPERATIONS: &[&str] = &[
    // Hosted push and pull.
    "Push",
    "Pull",
    "ListRefs",
    "UpdateRef",
    // Repository reads.
    "GetRefs",
    "ListStates",
    "GetState",
    "GetBlame",
    "ListProvenanceSummaries",
    "GetTree",
    "GetBlob",
    "GetCompare",
    "GetDiff",
    "GetSemanticHotSpots",
    "ListActions",
    // Context reads and writes.
    "ListContext",
    "GetContextHistory",
    "ListContextSuggestions",
    "SetContext",
    "ReviseContext",
    "SupersedeContext",
    // Discussions.
    "OpenDiscussion",
    "AppendTurn",
    "ResolveDiscussion",
    "ListByState",
    "ListBySymbol",
    "GetDiscussion",
    // Session identity.
    "WhoAmI",
];

/// Restrictions applied to a sub-agent's Biscuit. Constructed via
/// [`AgentAttenuation::time_bounded`] for the simplest case (no
/// operation/resource narrowing) or built up field-by-field for
/// richer restrictions.
///
/// Mirrors the server-side `weft_server::biscuit::AgentAttenuation`
/// shape — duplicated here because `server` is a heavy dep
/// (sqlx, tonic, axum, ...) we don't want to pull into the CLI's
/// production binary just for the attenuation machinery.
#[derive(Debug, Clone)]
pub struct AgentAttenuation {
    /// Stable id of the spawned agent — emitted as an `agent($id)`
    /// fact for audit trails. A reasonable default is
    /// `format!("agent-{}", uuid::Uuid::new_v4())`.
    pub agent_id: String,
    /// Hard expiry for this attenuation chain. The verifier injects
    /// `time(now())` on every authorized request; if it's past
    /// `expires_at`, the chain rejects regardless of the parent's
    /// own expiry.
    pub expires_at: DateTime<Utc>,
    /// When `Some`, the agent is restricted to the listed gRPC
    /// operations. Each entry is the bare method name (e.g.
    /// `"GetState"`, `"ListRefs"`).
    pub allowed_operations: Option<Vec<String>>,
    /// When `Some`, the agent is restricted to resources whose
    /// path matches one of the entries. Format: `(kind, path)`
    /// where `kind ∈ {"repo", "namespace"}`.
    pub allowed_resources: Option<Vec<(String, String)>>,
    /// Resource scopes carried for forward-compatible W3 enforcement.
    ///
    /// Today's server does not inject request `resource(...)` facts, so an
    /// `allowed_resources` check fails closed for every RPC. W1 records these
    /// declarations as `agent_scope(kind, path)` facts instead. They are inert
    /// until W3 teaches the server to enforce them.
    pub declared_scopes: Vec<(String, String)>,
}

impl AgentAttenuation {
    /// Time-bounded attenuation with no further restrictions. The
    /// agent inherits the full set of rights from the parent.
    pub fn time_bounded(agent_id: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        Self {
            agent_id: agent_id.into(),
            expires_at,
            allowed_operations: None,
            allowed_resources: None,
            declared_scopes: Vec::new(),
        }
    }
}

/// Attenuate a parent Biscuit (decoded base64 string) with the
/// supplied restrictions and return the attenuated Biscuit's
/// base64-encoded bytes.
///
/// Uses `UnverifiedBiscuit` because attenuation appends a new block
/// to bytes the parent already holds; the new block's signature
/// chains off the parent's keys, and the server validates the full
/// chain against its trust list when the agent presents the token.
/// The CLI never holds the server's signing key.
pub fn attenuate_for_agent(
    parent_token_b64: &str,
    restrictions: AgentAttenuation,
) -> Result<String> {
    let unverified = biscuit_auth::UnverifiedBiscuit::from_base64(parent_token_b64.as_bytes())
        .context("parse parent biscuit (unverified)")?;
    let block = build_attenuation_block(&restrictions)?;
    let attenuated = unverified
        .append(block)
        .context("append attenuation block")?;
    attenuated.to_base64().context("encode attenuated biscuit")
}

/// Build the BlockBuilder that holds the attenuation's facts +
/// checks. Pulled out so the agent-side code path can be unit-tested
/// without round-tripping through a parent token.
fn build_attenuation_block(
    restrictions: &AgentAttenuation,
) -> Result<biscuit_auth::builder::BlockBuilder> {
    // Fail closed on characters that could break out of a Biscuit string
    // literal or inject operators into the DSL before we assemble the block.
    validate_biscuit_token_string("agent_id", &restrictions.agent_id)?;
    if let Some(ops) = &restrictions.allowed_operations {
        for op in ops {
            validate_biscuit_token_string("allowed_operations entry", op)?;
        }
    }
    if let Some(resources) = &restrictions.allowed_resources {
        for (kind, path) in resources {
            validate_biscuit_token_string("resource kind", kind)?;
            validate_biscuit_token_string("resource path", path)?;
        }
    }
    for (kind, path) in &restrictions.declared_scopes {
        validate_biscuit_token_string("scope kind", kind)?;
        validate_biscuit_token_string("scope path", path)?;
    }

    let mut block = biscuit_auth::builder::BlockBuilder::new();
    block = block
        .fact(format!("agent({})", biscuit_string(&restrictions.agent_id)).as_str())
        .context("agent fact")?;
    block = block
        .fact(format!("agent_expires_at({})", restrictions.expires_at.to_rfc3339()).as_str())
        .context("agent expiry fact")?;
    block = block
        .check(
            format!(
                "check if time($now), $now < {}",
                restrictions.expires_at.to_rfc3339()
            )
            .as_str(),
        )
        .context("expiry check")?;
    // This independent check is deliberately present even when the caller
    // supplies no operation allowlist. It is the non-optional credential-
    // issuing/destructive operation floor for every token from this primitive.
    for denied in AGENT_OPERATION_DENY_FLOOR {
        block = block
            .check(format!("check if operation($op), $op != {}", biscuit_string(denied)).as_str())
            .context("agent operation deny floor")?;
    }
    if let Some(ops) = &restrictions.allowed_operations {
        let pred = if ops.is_empty() {
            // A syntactically valid predicate that no real gRPC method can
            // match. `Some(vec![])` therefore means deny all, not unrestricted.
            "$op == \"__heddle_no_agent_operations__\"".to_string()
        } else {
            ops.iter()
                .map(|op| format!("$op == {}", biscuit_string(op)))
                .collect::<Vec<_>>()
                .join(" || ")
        };
        block = block
            .check(format!("check if operation($op), {pred}").as_str())
            .context("operation allowlist check")?;
    }
    if let Some(resources) = &restrictions.allowed_resources
        && !resources.is_empty()
    {
        let mut clauses = Vec::new();
        for (kind, path) in resources {
            let prefix = format!("{path}/");
            clauses.push(format!(
                "($k == {kind_lit} && ($p == {path_lit} || $p.starts_with({prefix_lit})))",
                kind_lit = biscuit_string(kind),
                path_lit = biscuit_string(path),
                prefix_lit = biscuit_string(&prefix),
            ));
        }
        let pred = clauses.join(" || ");
        block = block
            .check(format!("check if resource($k, $p), {pred}").as_str())
            .context("resource allowlist check")?;
    }
    for (kind, path) in &restrictions.declared_scopes {
        block = block
            .fact(
                format!(
                    "agent_scope({}, {})",
                    biscuit_string(kind),
                    biscuit_string(path)
                )
                .as_str(),
            )
            .context("forward-compatible resource scope")?;
    }
    Ok(block)
}

/// Allowlist for values interpolated into Biscuit DSL string literals.
///
/// Restricted to `[A-Za-z0-9._/@:+-]` so quotes, newlines, `$`, `|`, and
/// other DSL/metacharacters cannot inject facts or checks. Paths may use
/// `/`; operation names and agent ids are alphanumeric-plus-punctuation.
fn validate_biscuit_token_string(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    for ch in value.chars() {
        if !matches!(
            ch,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '/' | '@' | ':' | '+' | '-'
        ) {
            anyhow::bail!(
                "{field} contains forbidden character {ch:?}; allowed: [A-Za-z0-9._/@:+-]"
            );
        }
    }
    Ok(())
}

fn biscuit_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Convenience constructor for the common "spawn an agent for the
/// next N hours, no further restrictions" case.
pub fn time_bounded(
    parent_token_b64: &str,
    agent_id: impl Into<String>,
    expires_at: DateTime<Utc>,
) -> Result<String> {
    attenuate_for_agent(
        parent_token_b64,
        AgentAttenuation::time_bounded(agent_id, expires_at),
    )
}

/// Convenience: attenuate to a read-only sub-agent on a single repo
/// for `duration_hours`. Emits both an operation allowlist (limited
/// to common read RPCs) and a resource allowlist scoped to the
/// repo's path. Use as a starting point — for finer-grained access,
/// build the [`AgentAttenuation`] directly.
pub fn read_only_repo_agent(
    parent_token_b64: &str,
    agent_id: impl Into<String>,
    repo_path: impl Into<String>,
    duration_hours: i64,
) -> Result<String> {
    attenuate_for_agent(
        parent_token_b64,
        AgentAttenuation {
            agent_id: agent_id.into(),
            expires_at: Utc::now() + chrono::Duration::hours(duration_hours),
            allowed_operations: Some(vec![
                "GetState".to_string(),
                "GetTree".to_string(),
                "GetBlob".to_string(),
                "GetCompare".to_string(),
                "GetDiff".to_string(),
                "ListRefs".to_string(),
                "ListStates".to_string(),
                "ListContext".to_string(),
            ]),
            allowed_resources: Some(vec![("repo".to_string(), repo_path.into())]),
            declared_scopes: Vec::new(),
        },
    )
}

#[cfg(test)]
mod tests {
    use biscuit_auth::{Biscuit, KeyPair, builder::AuthorizerBuilder};

    use super::*;

    /// Mint a parent Biscuit using biscuit-auth directly. We avoid
    /// pulling `weft_server::biscuit::mint` because that would force
    /// server into the regular dep graph; the goal here is to
    /// keep the CLI small.
    fn fresh_parent_token() -> (String, KeyPair) {
        let kp = KeyPair::new();
        let mut builder = biscuit_auth::Biscuit::builder();
        builder = builder.fact(r#"user("alice")"#).expect("user fact");
        builder = builder.fact(r#"session("sess-1")"#).expect("session fact");
        let exp = chrono::Utc::now() + chrono::Duration::hours(2);
        builder = builder
            .fact(format!("expires_at({})", exp.to_rfc3339()).as_str())
            .expect("expires_at fact");
        builder = builder
            .check(format!("check if time($now), $now < {}", exp.to_rfc3339()).as_str())
            .expect("expiry check");
        let biscuit = builder.build(&kp).expect("build parent biscuit");
        (biscuit.to_base64().expect("to_base64"), kp)
    }

    /// Exercise the same Biscuit chain verification and request-fact shape as
    /// the hosted server (`time` + bare gRPC `operation`).
    fn server_authorizes(
        token: &str,
        root: &KeyPair,
        operation: &str,
        now: DateTime<Utc>,
    ) -> Result<(), biscuit_auth::error::Token> {
        let root_public = root.public();
        let biscuit = Biscuit::from_base64(token, move |_| Ok(root_public))?;
        let mut authorizer = AuthorizerBuilder::new()
            .fact(format!("time({})", now.to_rfc3339()).as_str())?
            .fact(format!("operation({})", biscuit_string(operation)).as_str())?
            .policy("allow if true")?
            .build(&biscuit)?;
        authorizer.authorize().map(|_| ())
    }

    #[test]
    fn attenuate_appends_a_block_with_agent_marker() {
        let (parent, _kp) = fresh_parent_token();
        let attenuated = time_bounded(&parent, "agent-1", Utc::now() + chrono::Duration::hours(2))
            .expect("attenuate");
        // The attenuated bytes are strictly longer than the parent's
        // (the new block adds bytes). End-to-end verify happens in
        // the integration tests where a real server's keypair is
        // available.
        assert!(attenuated.len() > parent.len());
    }

    #[test]
    fn time_bounded_with_past_expiry_still_attenuates() {
        // The helper itself doesn't enforce expiry — that's the
        // verifier's job. A past-expiry attenuation builds fine but
        // gets rejected at verify time. This test just guards
        // against the helper accidentally rejecting timestamps it
        // doesn't like.
        let (parent, _kp) = fresh_parent_token();
        let result = time_bounded(&parent, "agent-1", Utc::now() - chrono::Duration::hours(1));
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_repo_agent_builds_with_op_and_resource_restrictions() {
        let (parent, _kp) = fresh_parent_token();
        let attenuated =
            read_only_repo_agent(&parent, "agent-r", "org/acme/heddle", 2).expect("attenuate");
        // Sanity: the attenuated bytes parse back as a Biscuit (via
        // the unverified path so we don't need the parent's root
        // key). The verifier round-trip is exercised in the
        // integration tests.
        let parsed =
            biscuit_auth::UnverifiedBiscuit::from_base64(attenuated.as_bytes()).expect("parse");
        assert!(parsed.block_count() >= 2, "expected attenuation block");
    }

    #[test]
    fn rejects_injection_in_allowed_operations() {
        let (parent, _kp) = fresh_parent_token();
        let err = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-1".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec![r#"x" || true || $op == "y"#.to_string()]),
                allowed_resources: None,
                declared_scopes: Vec::new(),
            },
        )
        .expect_err("injection payload must be rejected");
        let message = format!("{err:#}");
        assert!(
            message.contains("forbidden character") || message.contains("allowed_operations"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn accepts_normal_operation_names() {
        let (parent, _kp) = fresh_parent_token();
        attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-1".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["GetState".to_string(), "ListRefs".to_string()]),
                allowed_resources: Some(vec![("repo".to_string(), "org/acme/heddle".to_string())]),
                declared_scopes: Vec::new(),
            },
        )
        .expect("normal ops must attenuate");
    }

    #[test]
    fn server_accepts_allowed_operation_and_rejects_credential_issuance_and_delete_floor() {
        let (parent, root) = fresh_parent_token();
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-safe".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(
                    SAFE_AGENT_OPERATIONS
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                ),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle".to_string())],
            },
        )
        .expect("derive safe child");

        let parent_authority = biscuit_auth::UnverifiedBiscuit::from_base64(parent.as_bytes())
            .expect("parse parent")
            .print_block_source(0)
            .expect("parent authority");
        let child_authority = biscuit_auth::UnverifiedBiscuit::from_base64(child.as_bytes())
            .expect("parse child")
            .print_block_source(0)
            .expect("child authority");
        assert_eq!(
            child_authority, parent_authority,
            "offline attenuation must leave the root-human authority block unchanged"
        );

        server_authorizes(&child, &root, "Push", Utc::now())
            .expect("server accepts an allowlisted push");
        for denied in AGENT_OPERATION_DENY_FLOOR {
            assert!(
                server_authorizes(&child, &root, denied, Utc::now()).is_err(),
                "server must reject hard-denied operation {denied}"
            );
        }
    }

    #[test]
    fn server_rejects_child_after_attenuation_ttl() {
        let (parent, root) = fresh_parent_token();
        let expires_at = Utc::now() + chrono::Duration::minutes(5);
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-expiring".to_string(),
                expires_at,
                allowed_operations: Some(vec!["GetState".to_string()]),
                allowed_resources: None,
                declared_scopes: Vec::new(),
            },
        )
        .expect("derive expiring child");

        server_authorizes(&child, &root, "GetState", Utc::now())
            .expect("server accepts child before expiry");
        assert!(
            server_authorizes(
                &child,
                &root,
                "GetState",
                expires_at + chrono::Duration::seconds(1)
            )
            .is_err(),
            "server must reject child after TTL"
        );
    }

    #[test]
    fn sub_derivation_intersects_every_operation_block() {
        let (parent, root) = fresh_parent_token();
        let parent_agent = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-parent".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["Push".to_string()]),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle".to_string())],
            },
        )
        .expect("derive parent agent");
        let subagent = attenuate_for_agent(
            &parent_agent,
            AgentAttenuation {
                agent_id: "agent-child".to_string(),
                expires_at: Utc::now() + chrono::Duration::minutes(30),
                // Attempt to add GetState cannot override the parent's block.
                allowed_operations: Some(vec!["Push".to_string(), "GetState".to_string()]),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle/subdir".to_string())],
            },
        )
        .expect("derive subagent");

        server_authorizes(&subagent, &root, "Push", Utc::now())
            .expect("operation retained by both blocks is allowed");
        assert!(
            server_authorizes(&subagent, &root, "GetState", Utc::now()).is_err(),
            "a child cannot widen its parent's operation set"
        );
        let parsed = biscuit_auth::UnverifiedBiscuit::from_base64(subagent.as_bytes())
            .expect("parse subagent");
        assert_eq!(parsed.block_count(), 3, "authority plus two agent hops");
        let parent_source = parsed.print_block_source(1).expect("parent block source");
        let child_source = parsed.print_block_source(2).expect("child block source");
        assert!(parent_source.contains("agent_scope(\"repo\", \"acme/heddle\")"));
        assert!(child_source.contains("agent_scope(\"repo\", \"acme/heddle/subdir\")"));
    }

    #[test]
    fn validate_biscuit_token_string_allowlist() {
        assert!(validate_biscuit_token_string("op", "GetState").is_ok());
        assert!(validate_biscuit_token_string("path", "org/acme/heddle").is_ok());
        assert!(validate_biscuit_token_string("op", r#"x" || true"#).is_err());
        assert!(validate_biscuit_token_string("op", "a$b").is_err());
        assert!(validate_biscuit_token_string("op", "").is_err());
    }
}
