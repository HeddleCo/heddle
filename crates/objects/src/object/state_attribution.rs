// SPDX-License-Identifier: Apache-2.0
//! Attribution types for states.

use serde::{Deserialize, Serialize};

/// Human identity accountable for changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Human-readable name.
    pub name: String,
    /// Email address.
    pub email: String,
}

impl Principal {
    /// Create a new principal.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let name = std::env::var("HEDDLE_PRINCIPAL_NAME").ok()?;
        let email = std::env::var("HEDDLE_PRINCIPAL_EMAIL").ok()?;
        Some(Self { name, email })
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} <{}>", self.name, self.email)
    }
}

/// AI agent identity that performed changes on behalf of a principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// Provider name (e.g., "anthropic", "openai").
    pub provider: String,
    /// Model identifier (e.g., "claude-opus-4-5-20250120").
    pub model: String,
    /// Session identifier (opt-in, links to Session.id).
    pub session_id: Option<String>,
    /// Segment identifier (opt-in, links to SessionSegment.id).
    pub segment_id: Option<String>,
    /// Policy or prompt template identifier.
    pub policy_id: Option<String>,
}

impl Agent {
    /// Create a new agent.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            session_id: None,
            segment_id: None,
            policy_id: None,
        }
    }

    /// Create with session linkage.
    pub fn with_session(
        mut self,
        session_id: impl Into<String>,
        segment_id: impl Into<String>,
    ) -> Self {
        self.session_id = Some(session_id.into());
        self.segment_id = Some(segment_id.into());
        self
    }

    /// Create with policy ID.
    pub fn with_policy(mut self, policy_id: impl Into<String>) -> Self {
        self.policy_id = Some(policy_id.into());
        self
    }

    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let provider = std::env::var("HEDDLE_AGENT_PROVIDER").ok()?;
        let model = std::env::var("HEDDLE_AGENT_MODEL").ok()?;
        let session_id = std::env::var("HEDDLE_SESSION_ID").ok();
        let segment_id = std::env::var("HEDDLE_SESSION_SEGMENT").ok();
        let policy_id = std::env::var("HEDDLE_AGENT_POLICY").ok();
        Some(Self {
            provider,
            model,
            session_id,
            segment_id,
            policy_id,
        })
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Attribution for a change (who did it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Human accountable for the change.
    pub principal: Principal,
    /// AI agent that performed the change (if any).
    pub agent: Option<Agent>,
}

impl Attribution {
    /// Create attribution for a human-only change.
    pub fn human(principal: Principal) -> Self {
        Self {
            principal,
            agent: None,
        }
    }

    /// Create attribution for an agent-assisted change.
    pub fn with_agent(principal: Principal, agent: Agent) -> Self {
        Self {
            principal,
            agent: Some(agent),
        }
    }
}

impl std::fmt::Display for Attribution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(agent) = &self.agent {
            write!(f, "{} (via {})", self.principal, agent)
        } else {
            write!(f, "{}", self.principal)
        }
    }
}
