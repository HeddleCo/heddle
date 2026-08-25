// SPDX-License-Identifier: Apache-2.0
//! Wire payload for `heddle ready`.
//!
//! `Serialize` is hand-written because the envelope flattens the embedded
//! [`OperatorCommandOutput`] beside ready-specific axes and derives the
//! readiness summary at emission time. The schema comes from [`ReadyShape`]
//! directly below the serializer — change them together.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::ser::Error as SerError;
use serde::{Serialize, Serializer, ser::SerializeStruct};
use verbs::{
    ActionTemplate, RepositoryVerificationState, ThreadPreviewReport,
    ready_freshness_summary as core_ready_freshness_summary,
    ready_integration_summary as core_ready_integration_summary,
    ready_merge_type_summary as core_ready_merge_type_summary,
    ready_status_summary as core_ready_status_summary,
};

use super::operator::{OperatorAction, OperatorCommandOutput, normalized_action};
use crate::cli::commands::verification_health::action_template;

/// JSON payload for `heddle ready`.
pub struct ReadyOutput {
    pub operator: OperatorCommandOutput,
    pub captured: bool,
    pub captured_state: Option<String>,
    pub thread_state: String,
    pub trust: RepositoryVerificationState,
    pub report: ThreadPreviewReport,
}

impl ReadyOutput {
    fn capture_status(&self) -> (&'static str, &'static str) {
        if self.captured {
            (
                "captured",
                "worktree changes were captured during this ready invocation",
            )
        } else if ready_blocked_by_missing_intent(self) {
            (
                "required",
                "the worktree has uncaptured changes and capture intent is missing",
            )
        } else if !self.trust.verified && self.report.merge_relation == "blocked" {
            (
                "not_checked",
                "repository verification blocked capture evaluation",
            )
        } else {
            (
                "not_needed",
                "the worktree already matches the current Heddle state",
            )
        }
    }

    pub fn readiness_summary(&self) -> ReadyReadinessSummary {
        let (capture_status, capture_reason) = self.capture_status();
        let checks = ready_checks_summary(self);
        let integration = core_ready_integration_summary(&self.report.merge_relation);
        let freshness =
            core_ready_freshness_summary(&self.report.merge_relation, &self.report.freshness);
        let merge_type = core_ready_merge_type_summary(&self.report.merge_relation);
        let impact = if self.report.impact_categories.is_empty() {
            "none".to_string()
        } else {
            self.report.impact_categories.join(", ")
        };
        ReadyReadinessSummary {
            status: core_ready_status_summary(
                &self.report.merge_relation,
                self.report.blockers.is_empty(),
                &self.report.thread_health,
            ),
            captured: self.captured,
            captured_state: self.captured_state.clone(),
            capture_status: capture_status.to_string(),
            capture_reason: capture_reason.to_string(),
            checks,
            integration,
            freshness,
            merge_type,
            changed_path_count: self.report.changed_path_count,
            changed_paths: self.report.changed_paths.clone(),
            conflict_count: self.report.conflict_count,
            conflicts: self.report.conflicts.clone(),
            impact,
            impact_categories: self.report.impact_categories.clone(),
            blockers: self.report.blockers.clone(),
        }
    }
}

impl Serialize for ReadyOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let next_action = normalized_action(self.operator.next_action.as_deref());
        let recommended_action = normalized_action(self.operator.recommended_action.as_deref());
        let next_action_template = next_action.and_then(action_template);
        let recommended_action_template = recommended_action.and_then(action_template);
        let verification = serde_json::to_value(&self.trust).map_err(SerError::custom)?;
        let readiness = self.readiness_summary();

        let (capture_status, capture_reason) = self.capture_status();
        let mut state = serializer.serialize_struct("ReadyOutput", 20)?;
        state.serialize_field("output_kind", "ready")?;
        state.serialize_field("status", &self.operator.status)?;
        state.serialize_field("action", &self.operator.action)?;
        state.serialize_field("message", &self.operator.message)?;
        state.serialize_field("blockers", &self.operator.blockers)?;
        state.serialize_field("warnings", &self.operator.warnings)?;
        state.serialize_field("next_action", &next_action)?;
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
        state.serialize_field("recommended_action_template", &recommended_action_template)?;
        state.serialize_field("captured", &self.captured)?;
        state.serialize_field("captured_state", &self.captured_state)?;
        state.serialize_field("capture_status", capture_status)?;
        state.serialize_field("capture_reason", capture_reason)?;
        state.serialize_field("thread_state", &self.thread_state)?;
        state.serialize_field("readiness", &readiness)?;
        state.serialize_field("report", &self.report)?;
        state.serialize_field("verification", &verification)?;
        state.end()
    }
}

pub fn ready_blocked_by_missing_intent(output: &ReadyOutput) -> bool {
    output
        .report
        .blockers
        .iter()
        .any(|blocker| blocker.contains("-m/--message/--intent"))
        && output.report.merge_relation == "not_checked"
        && output
            .operator
            .recommended_action
            .as_deref()
            .is_some_and(|action| action == "heddle capture -m \"...\"")
}

fn ready_checks_summary(output: &ReadyOutput) -> ReadyChecksSummary {
    if ready_blocked_by_missing_intent(output) {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "commit intent is required before readiness checks can run".to_string(),
        }
    } else if !output.trust.verified {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "repository verification is blocked".to_string(),
        }
    } else if output.report.merge_relation == "not_checked" {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "readiness preview was not reached".to_string(),
        }
    } else {
        ReadyChecksSummary {
            status: "completed".to_string(),
            reason: "readiness preview ran".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "ReadyReadinessSchema")]
pub struct ReadyReadinessSummary {
    pub status: String,
    pub captured: bool,
    pub captured_state: Option<String>,
    pub capture_status: String,
    pub capture_reason: String,
    pub checks: ReadyChecksSummary,
    pub integration: String,
    pub freshness: String,
    pub merge_type: String,
    pub changed_path_count: usize,
    pub changed_paths: Vec<String>,
    pub conflict_count: usize,
    pub conflicts: Vec<String>,
    pub impact: String,
    pub impact_categories: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "ReadyChecksSchema")]
pub struct ReadyChecksSummary {
    pub status: String,
    pub reason: String,
}

// ---- JSON Schema -----------------------------------------------------------
//
// The published shape mirrors the hand-written serializer above.

#[derive(JsonSchema)]
#[allow(dead_code)] // fields describe the wire; the serializer writes them
#[schemars(rename = "ReadySchema")]
struct ReadyShape {
    output_kind: String,
    status: String,
    action: OperatorAction,
    message: String,
    blockers: Vec<String>,
    warnings: Vec<String>,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    captured: bool,
    captured_state: Option<String>,
    capture_status: String,
    capture_reason: String,
    thread_state: String,
    readiness: ReadyReadinessSummary,
    report: ThreadPreviewReport,
    verification: RepositoryVerificationState,
}

impl JsonSchema for ReadyOutput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <ReadyShape as JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <ReadyShape as JsonSchema>::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use verbs::doctor_schemas_plan::schema_property_keys;

    /// Every field the hand-written serializer emits must be declared on the
    /// shape struct that publishes the schema.
    #[test]
    fn serialized_ready_keys_are_declared_on_the_shape() {
        let output = ReadyOutput {
            operator: OperatorCommandOutput {
                status: "completed".to_string(),
                action: OperatorAction::Ready,
                message: "ready to land".to_string(),
                blockers: Vec::new(),
                warnings: Vec::new(),
                next_action: None,
                recommended_action: Some("heddle land".to_string()),
            },
            captured: false,
            captured_state: None,
            thread_state: "active".to_string(),
            trust: RepositoryVerificationState {
                verified: false,
                status: "not_checked".to_string(),
                repository_mode: "native-heddle".to_string(),
                heddle_initialized: true,
                git_branch: None,
                heddle_thread: Some("main".to_string()),
                worktree_dirty: false,
                worktree_state: "clean".to_string(),
                import_state: "not_applicable".to_string(),
                mapping_state: "not_applicable".to_string(),
                remote_drift: "clean".to_string(),
                active_operation: None,
                default_remote: None,
                clone_verification: "not_applicable".to_string(),
                machine_contract: "available".to_string(),
                machine_contract_coverage: verbs::MachineContractCoverage::not_checked(),
                workflow_status: "idle".to_string(),
                workflow_summary: String::new(),
                summary: String::new(),
                recommended_action: String::new(),
                recommended_action_template: None,
                recovery_commands: Vec::new(),
                recovery_action_templates: Vec::new(),
                checks: Vec::new(),
            },
            report: ThreadPreviewReport {
                thread: "main".to_string(),
                thread_mode: "solid".to_string(),
                thread_state: "active".to_string(),
                freshness: "current".to_string(),
                task: None,
                changed_paths: Vec::new(),
                changed_path_count: 0,
                impact_categories: Vec::new(),
                heavy_impact_paths: Vec::new(),
                merge_relation: "not_applicable".to_string(),
                conflicts: Vec::new(),
                conflict_count: 0,
                blockers: Vec::new(),
                recommended_action: String::new(),
                recommended_action_template: None,
                thread_health: "healthy".to_string(),
            },
        };
        let emitted = serde_json::to_value(&output).expect("ready payload serializes");
        let schema = serde_json::to_value(schemars::schema_for!(ReadyShape)).expect("schema");
        let declared = schema_property_keys(&schema);
        for key in emitted.as_object().expect("object").keys() {
            assert!(
                declared.contains(key),
                "serializer emits `{key}` but the published schema does not declare it"
            );
        }
        assert_eq!(emitted["output_kind"], "ready");
    }
}
