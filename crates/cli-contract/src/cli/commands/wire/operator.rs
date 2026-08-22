// SPDX-License-Identifier: Apache-2.0
//! Operator envelope types shared by the state-advancing verbs
//! (`continue`, `abort`, `sync`, `ready`, `land`, ...).
//!
//! The wire shape is produced by [`OperatorCommandOutput::serialize_with_output_kind`];
//! the `output_kind` is injected at render time by [`OperatorCommandEnvelope`],
//! and `blockers`/`warnings` are omitted when empty. Because the serializer is
//! hand-written, `JsonSchema` is implemented from [`OperatorCommandShape`]
//! directly below it — change them together.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use verbs::{
    ActionTemplate, RepositoryVerificationState, VerificationClaimPolicyFacts,
    VerificationClaimTrustFacts,
    repository_verification_allows_success_claim as core_repository_verification_allows_success_claim,
    status::next_action::non_empty_action,
};

use crate::cli::commands::verification_health::{
    action_template, repository_verification_blockers, repository_verification_primary_command,
};

/// The verb role an operator envelope is rendered for. Serializes as the
/// catalog's `output_kind` wire value (e.g. `thread_cleanup`, `cherry-pick`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OperatorAction {
    Abort,
    Bisect,
    CherryPick,
    #[default]
    Continue,
    Land,
    Merge,
    Ready,
    Rebase,
    Revert,
    Sync,
    ThreadCleanup,
    ThreadDrop,
    ThreadPromote,
    ThreadRefresh,
    ThreadResolve,
}

impl OperatorAction {
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Bisect => "bisect",
            Self::CherryPick => "cherry-pick",
            Self::Continue => "continue",
            Self::Land => "land",
            Self::Merge => "merge",
            Self::Ready => "ready",
            Self::Rebase => "rebase",
            Self::Revert => "revert",
            Self::Sync => "sync",
            Self::ThreadCleanup => "thread_cleanup",
            Self::ThreadDrop => "thread_drop",
            Self::ThreadPromote => "thread_promote",
            Self::ThreadRefresh => "thread_refresh",
            Self::ThreadResolve => "thread_resolve",
        }
    }
}

impl From<&repo::OperationKind> for OperatorAction {
    fn from(kind: &repo::OperationKind) -> Self {
        match kind {
            repo::OperationKind::Merge => Self::Merge,
            repo::OperationKind::Rebase => Self::Rebase,
            repo::OperationKind::CherryPick => Self::CherryPick,
            repo::OperationKind::Revert => Self::Revert,
            repo::OperationKind::Bisect => Self::Bisect,
        }
    }
}

impl Serialize for OperatorAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.wire_value())
    }
}

impl JsonSchema for OperatorAction {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("OperatorAction")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        <String as JsonSchema>::json_schema(&mut SchemaGenerator::default())
    }
}

/// How much an operator envelope may claim success while repository
/// verification is blocked.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerificationClaimPolicy {
    allow_land_publish_followup: bool,
    allow_matching_workflow_action: bool,
}

impl VerificationClaimPolicy {
    pub fn strict() -> Self {
        Self::default()
    }

    pub fn allow_land_publish_followup(mut self) -> Self {
        self.allow_land_publish_followup = true;
        self
    }

    pub fn allow_matching_workflow_action(mut self) -> Self {
        self.allow_matching_workflow_action = true;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct OperatorCommandOutput {
    pub status: String,
    pub action: OperatorAction,
    pub message: String,
    /// Reasons the operation could not advance state. Only populated
    /// when `status == "blocked"` or `status == "failed"`. When the
    /// operation succeeded with caveats, use `warnings` instead.
    pub blockers: Vec<String>,
    /// Non-blocking nudges surfaced when the operation actually
    /// advanced state but the caller may still want a follow-up
    /// (e.g. a heavy-impact change worth reviewing for broader impact).
    /// Always omitted when empty.
    pub warnings: Vec<String>,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
}

impl OperatorCommandOutput {
    pub fn blocked_by_repository_verification(
        action: OperatorAction,
        message: impl Into<String>,
        trust: &RepositoryVerificationState,
    ) -> Self {
        let recommended_action = repository_verification_primary_command(trust);
        Self {
            status: "blocked".to_string(),
            action,
            message: message.into(),
            blockers: repository_verification_blockers(trust),
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action),
        }
    }

    pub fn block_success_claim_if_verification_blocked(
        &mut self,
        trust: &RepositoryVerificationState,
        local_context: impl Into<String>,
        policy: VerificationClaimPolicy,
    ) {
        if repository_verification_allows_success_claim(self, trust, policy) {
            return;
        }
        *self = Self::blocked_by_repository_verification(
            self.action,
            format!(
                "{} reached local checks, but repository verification is blocked: {}",
                local_context.into(),
                trust.summary
            ),
            trust,
        );
    }

    pub fn envelope_for_command(&self, output_kind: OperatorAction) -> OperatorCommandEnvelope<'_> {
        OperatorCommandEnvelope {
            output: self,
            output_kind,
        }
    }

    fn serialize_with_output_kind<S>(
        &self,
        serializer: S,
        output_kind: OperatorAction,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let next_action = normalized_action(self.next_action.as_deref());
        let recommended_action = normalized_action(self.recommended_action.as_deref());
        let next_action_template = next_action.and_then(action_template);
        let recommended_action_template = recommended_action.and_then(action_template);

        let mut len = 8;
        if !self.blockers.is_empty() {
            len += 1;
        }
        if !self.warnings.is_empty() {
            len += 1;
        }

        let mut state = serializer.serialize_struct("OperatorCommandOutput", len)?;
        state.serialize_field("output_kind", &output_kind.wire_value())?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("action", &self.action)?;
        state.serialize_field("message", &self.message)?;
        if !self.blockers.is_empty() {
            state.serialize_field("blockers", &self.blockers)?;
        }
        if !self.warnings.is_empty() {
            state.serialize_field("warnings", &self.warnings)?;
        }
        state.serialize_field("next_action", &next_action)?;
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
        state.serialize_field(
            "recommended_action_template",
            &recommended_action_template,
        )?;
        state.end()
    }
}

fn repository_verification_allows_success_claim(
    output: &OperatorCommandOutput,
    trust: &RepositoryVerificationState,
    policy: VerificationClaimPolicy,
) -> bool {
    core_repository_verification_allows_success_claim(
        &output.status,
        VerificationClaimTrustFacts {
            verified: trust.verified,
            recommended_action: &trust.recommended_action,
            remote_drift: &trust.remote_drift,
            workflow_status: &trust.workflow_status,
        },
        output.action == OperatorAction::Land && output.status == "landed",
        output
            .recommended_action
            .as_deref()
            .is_some_and(|action| action == trust.recommended_action),
        VerificationClaimPolicyFacts {
            allow_land_publish_followup: policy.allow_land_publish_followup,
            allow_matching_workflow_action: policy.allow_matching_workflow_action,
        },
    )
}

pub(crate) fn normalized_action(action: Option<&str>) -> Option<&str> {
    non_empty_action(action)
}

impl Serialize for OperatorCommandOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.serialize_with_output_kind(serializer, self.action)
    }
}

/// Render-time view of an operator envelope: same shape as the output, with
/// the emitting command's `output_kind`.
pub struct OperatorCommandEnvelope<'a> {
    pub output: &'a OperatorCommandOutput,
    pub output_kind: OperatorAction,
}

impl Serialize for OperatorCommandEnvelope<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.output
            .serialize_with_output_kind(serializer, self.output_kind)
    }
}

// ---- JSON Schema -----------------------------------------------------------
//
// `Serialize` above is hand-written (dynamic `output_kind`, conditional
// `blockers`/`warnings`, templates derived at emission), so the schema comes
// from this shape struct instead of a derive. Keep in lockstep with
// `serialize_with_output_kind`; the test below pins the emitted keys to the
// shape's declared properties.

#[derive(JsonSchema)]
#[allow(dead_code)] // fields describe the wire; the serializer writes them
struct OperatorCommandShape {
    pub output_kind: String,
    pub status: String,
    pub action: OperatorAction,
    pub message: String,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
}

impl JsonSchema for OperatorCommandOutput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <OperatorCommandShape as JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <OperatorCommandShape as JsonSchema>::json_schema(generator)
    }
}

impl JsonSchema for OperatorCommandEnvelope<'_> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <OperatorCommandShape as JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <OperatorCommandShape as JsonSchema>::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use verbs::doctor_schemas_plan::schema_property_keys;

    /// Every field the hand-written serializer can emit must be declared on
    /// the shape struct that publishes the schema.
    #[test]
    fn serialized_envelope_keys_are_declared_on_the_shape() {
        let output = OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Merge,
            message: "conflicts remain".to_string(),
            blockers: vec!["src/lib.rs".to_string()],
            warnings: vec!["review the impact".to_string()],
            next_action: Some("heddle resolve --list".to_string()),
            recommended_action: None,
        };
        let emitted =
            serde_json::to_value(output.envelope_for_command(OperatorAction::Continue))
                .expect("envelope serializes");
        let schema = serde_json::to_value(schemars::schema_for!(OperatorCommandShape))
            .expect("shape schema serializes");
        let declared = schema_property_keys(&schema);
        for key in emitted.as_object().expect("envelope is an object").keys() {
            assert!(
                declared.contains(key),
                "serializer emits `{key}` but the published schema does not declare it"
            );
        }
        assert_eq!(
            emitted["output_kind"], "continue",
            "the envelope injects the rendering command's output_kind"
        );
    }
}
