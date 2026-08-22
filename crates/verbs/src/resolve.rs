// SPDX-License-Identifier: Apache-2.0
//! Typed report contract for inspectable conflict resolution.

use objects::object::{Agent, Attribution, ConflictRange, ConflictRegion, ConflictSide, Principal};
use oplog::ConflictResolutionMode;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract, schema_for_report,
};

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ResolveReport {
    pub output_kind: String,
    pub message: Option<String>,
    pub resolved: Vec<String>,
    pub remaining: Vec<String>,
    /// Path-level compatibility/progress surface, including structural conflicts.
    pub conflict_paths: Vec<String>,
    /// Content conflicts expanded into independently addressable regions.
    pub conflicts: Vec<ConflictRegionReport>,
    /// Resolution operations produced by this command invocation.
    pub resolutions: Vec<ConflictResolutionReport>,
    pub continued: bool,
    pub continuation_status: Option<String>,
    pub continuation_message: Option<String>,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
}

impl ResolveReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "resolve",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "resolve",
        }),
        schema: schema_for_report::<Self>,
    };
}

impl HeddleReport for ResolveReport {
    const CONTRACT: ReportContract = Self::CONTRACT;
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConflictRegionReport {
    pub id: String,
    pub path: String,
    pub symbol: Option<String>,
    pub occurrence: u32,
    pub merged_range: ConflictRangeReport,
    pub base: ConflictSideReport,
    pub ours: ConflictSideReport,
    pub theirs: ConflictSideReport,
}

impl From<&ConflictRegion> for ConflictRegionReport {
    fn from(conflict: &ConflictRegion) -> Self {
        Self {
            id: conflict.id.clone(),
            path: conflict.path.clone(),
            symbol: conflict.symbol.clone(),
            occurrence: conflict.occurrence,
            merged_range: conflict.merged_range.into(),
            base: (&conflict.base).into(),
            ours: (&conflict.ours).into(),
            theirs: (&conflict.theirs).into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
pub struct ConflictRangeReport {
    pub start_line: u32,
    pub end_line: u32,
}

impl From<ConflictRange> for ConflictRangeReport {
    fn from(range: ConflictRange) -> Self {
        Self {
            start_line: range.start_line,
            end_line: range.end_line,
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConflictSideReport {
    pub source_state: String,
    pub blob_id: Option<String>,
    pub range: ConflictRangeReport,
    pub hunk_hash: String,
}

impl From<&ConflictSide> for ConflictSideReport {
    fn from(side: &ConflictSide) -> Self {
        Self {
            source_state: side.source_state.to_string_full(),
            blob_id: side.blob_id.map(|id| id.to_hex()),
            range: side.range.into(),
            hunk_hash: side.hunk_hash.to_hex(),
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConflictResolutionReport {
    pub conflict_id: String,
    pub path: String,
    pub resolution: String,
    pub mode: ConflictResolutionModeReport,
    pub resolver: ResolverAttributionReport,
}

impl ConflictResolutionReport {
    pub fn new(
        conflict_id: impl Into<String>,
        path: impl Into<String>,
        resolver: &Attribution,
        mode: ConflictResolutionMode,
    ) -> Self {
        Self {
            conflict_id: conflict_id.into(),
            path: path.into(),
            resolution: mode.as_str().to_string(),
            mode: mode.into(),
            resolver: resolver.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolutionModeReport {
    Ours,
    Theirs,
    Edit,
    Auto,
}

impl From<ConflictResolutionMode> for ConflictResolutionModeReport {
    fn from(mode: ConflictResolutionMode) -> Self {
        match mode {
            ConflictResolutionMode::Ours => Self::Ours,
            ConflictResolutionMode::Theirs => Self::Theirs,
            ConflictResolutionMode::Edit => Self::Edit,
            ConflictResolutionMode::Auto => Self::Auto,
        }
    }
}

impl std::fmt::Display for ConflictResolutionModeReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Edit => "edit",
            Self::Auto => "auto",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ResolverAttributionReport {
    pub kind: ResolverKindReport,
    pub principal: PrincipalReport,
    pub agent: Option<AgentReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolverKindReport {
    Human,
    Agent,
}

impl From<&Attribution> for ResolverAttributionReport {
    fn from(attribution: &Attribution) -> Self {
        Self {
            kind: if attribution.agent.is_some() {
                ResolverKindReport::Agent
            } else {
                ResolverKindReport::Human
            },
            principal: (&attribution.principal).into(),
            agent: attribution.agent.as_ref().map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PrincipalReport {
    pub name: String,
    pub email: String,
}

impl From<&Principal> for PrincipalReport {
    fn from(principal: &Principal) -> Self {
        Self {
            name: principal.name_lossy().into_owned(),
            email: principal.email_lossy().into_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AgentReport {
    pub provider: String,
    pub model: String,
    pub session_id: Option<String>,
    pub segment_id: Option<String>,
    pub policy_id: Option<String>,
}

impl From<&Agent> for AgentReport {
    fn from(agent: &Agent) -> Self {
        Self {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            session_id: agent.session_id.clone(),
            segment_id: agent.segment_id.clone(),
            policy_id: agent.policy_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_kind_distinguishes_human_and_agent_without_dropping_identity() {
        let principal = Principal::new("Ada", "ada@example.com");
        let human = ResolverAttributionReport::from(&Attribution::human(principal.clone()));
        assert_eq!(human.kind, ResolverKindReport::Human);
        assert!(human.agent.is_none());

        let agent = ResolverAttributionReport::from(&Attribution::with_agent(
            principal,
            Agent::new("openai", "gpt-resolver"),
        ));
        assert_eq!(agent.kind, ResolverKindReport::Agent);
        assert_eq!(agent.principal.email, "ada@example.com");
        assert_eq!(agent.agent.unwrap().provider, "openai");
    }

    #[test]
    fn resolution_report_preserves_automatic_vs_edited_mode() {
        let resolver = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let automatic = ConflictResolutionReport::new(
            "conflict-a",
            "src/lib.rs",
            &resolver,
            ConflictResolutionMode::Auto,
        );
        let edited = ConflictResolutionReport::new(
            "conflict-b",
            "src/lib.rs",
            &resolver,
            ConflictResolutionMode::Edit,
        );
        assert_eq!(automatic.mode, ConflictResolutionModeReport::Auto);
        assert_eq!(edited.mode, ConflictResolutionModeReport::Edit);
    }
}
