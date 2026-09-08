//! Shared narrowing rules for offline human-to-agent and agent-to-agent delegation.
//! This module never creates a root. The caller appends this block plus a
//! signed proof-key transition to the existing Biscuit; hosts verify the full
//! authority chain and all ancestor checks for each actual request.

use biscuit_auth::builder::BlockBuilder;
use chrono::{DateTime, Utc};

use crate::{BiscuitError, BiscuitResultExt as _, SELF_OBSERVATION_OPERATION, biscuit_string};

/// Restrictions on an agent attenuation. `None` preserves the parent's
/// ceiling; an explicitly empty list denies work on that dimension.
/// Defaults to expiry-only (no operation/resource narrowing) so the simplest "spawn an agent
/// for the next 4 hours" call site is one constructor away.
#[derive(Debug, Clone)]
pub struct AgentAttenuation {
    /// Stable id of the spawned agent — emitted as an `agent($id)`
    /// fact so audit logs can trace which sub-agent acted.
    pub agent_id: String,
    /// Hard expiry for this attenuation chain. The verifier injects
    /// `time(now())` on every authorized request; if it's past
    /// `expires_at`, the chain rejects regardless of the parent's
    /// own expiry.
    pub expires_at: DateTime<Utc>,
    /// When `Some`, the agent is restricted to the listed hosted
    /// operations. Each entry is the bare method name (e.g.
    /// `"ReadContent"`, `"ObserveThread"`). An empty list denies all work;
    /// `None` retains the parent's operation ceiling. Self-observation remains
    /// available through either ceiling. For the check to fire, the
    /// verifier must inject an `operation($name)` fact at request
    /// time.
    pub allowed_operations: Option<Vec<String>>,
    /// When `Some`, the agent is restricted to resources whose path
    /// matches one of the entries. Format: `(kind, path)`. The
    /// match is path-prefix: an entry of `("spool", "org/acme")`
    /// covers `spool:org/acme` and any nested spool. The verifier
    /// must inject `resource($kind, $path)` per request for the
    /// check to fire. An empty list denies all resource work; `None` retains
    /// the parent's resource ceiling. Self-observation is always caller-bound.
    pub allowed_resources: Option<Vec<(String, String)>>,
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
        }
    }
}

impl AgentAttenuation {
    /// Build explicit restrictions without imposing a hidden agent-role ceiling.
    /// This block alone does not authorize a child key: the caller must also add
    /// a signed `pop_delegation` binding before appending it to the parent.
    pub fn block(&self) -> Result<BlockBuilder, BiscuitError> {
        agent_attenuation_block(self)
    }
}

fn agent_attenuation_block(restrictions: &AgentAttenuation) -> Result<BlockBuilder, BiscuitError> {
    let mut block: BlockBuilder = BlockBuilder::new();
    block = block
        .fact(format!("agent({})", biscuit_string(&restrictions.agent_id)).as_str())
        .internal_ctx("attenuate agent fact")?;
    block = block
        .check(
            format!(
                "check if time($now), $now < {}",
                restrictions.expires_at.to_rfc3339()
            )
            .as_str(),
        )
        .internal_ctx("attenuate expiry check")?;
    if let Some(ops) = &restrictions.allowed_operations {
        let pred = if ops.is_empty() {
            "false".to_string()
        } else {
            ops.iter()
                .map(|op| format!("$op == {}", biscuit_string(op)))
                .collect::<Vec<_>>()
                .join(" || ")
        };
        // ObserveIdentity is caller-bound self-introspection, not an operation on an
        // attenuated resource. It remains subject to signature, expiry, and
        // every non-operation caveat, but never to the delegated work ceiling.
        // An explicit identity permission grants the inherited account view.
        // Add the limited self-introspection exception only to work-only lists;
        // hosts must be able to distinguish that exception from full permission.
        let check = if ops.iter().any(|op| op == SELF_OBSERVATION_OPERATION) {
            format!("check if operation($op), {pred}")
        } else {
            format!(
                "check if operation(\"{SELF_OBSERVATION_OPERATION}\") or operation($op), {pred}"
            )
        };
        block = block
            .check(check.as_str())
            .internal_ctx("attenuate operation check")?;
    }
    if let Some(resources) = &restrictions.allowed_resources {
        // `check if resource($k, $p), (kind/path tuple matching)`.
        // Each entry matches via exact-equality OR path-prefix
        // (`p == "org/acme" || p.starts_with("org/acme/")`) so a
        // parent-spool grant covers descendants cleanly.
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
        let pred = if clauses.is_empty() {
            "false".to_string()
        } else {
            clauses.join(" || ")
        };
        // The same self-introspection exception applies to resource ceilings:
        // ObserveIdentity returns only the verified caller and cannot access the scoped
        // repository named by this caveat.
        let check = format!(
            "check if operation(\"{SELF_OBSERVATION_OPERATION}\") or resource($k, $p), {pred}"
        );
        block = block
            .check(check.as_str())
            .internal_ctx("attenuate resource check")?;
    }
    Ok(block)
}
