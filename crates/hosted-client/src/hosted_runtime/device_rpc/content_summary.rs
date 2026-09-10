//! Shared pure mapping of one exact native State.
use api::heddle::api::v1alpha1 as shared;
use objects::object::State;

pub(super) fn summary(
    state: &State,
    terminal_status: Option<shared::StateStatus>,
) -> shared::StateSummary {
    let agent = state
        .attribution
        .agent
        .as_ref()
        .map(|agent| shared::StateAgent {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
        });
    let verification = state
        .verification
        .as_ref()
        .map(|verification| shared::StateVerification {
            tests_passed: verification.tests_passed.map(u32::from).unwrap_or_default(),
            tests_failed: verification.tests_failed.unwrap_or_default(),
            coverage_pct: verification.coverage_pct,
            coverage_delta: verification.coverage_delta,
            lint_warnings: verification.lint_warnings.unwrap_or_default(),
        });
    shared::StateSummary {
        state_id: Some(shared::StateId {
            value: state.id().as_bytes().to_vec(),
        }),
        parents: state
            .parents
            .iter()
            .map(|id| id.as_bytes().to_vec())
            .collect(),
        intent: state.intent.clone().unwrap_or_default(),
        created_at: Some(prost_types::Timestamp {
            seconds: state.created_at.timestamp(),
            nanos: state.created_at.timestamp_subsec_nanos() as i32,
        }),
        principal: Some(shared::StatePrincipal {
            name: state.attribution.principal.name_lossy().into_owned(),
            email: state.attribution.principal.email_lossy().into_owned(),
        }),
        agent,
        confidence: state.confidence,
        status: terminal_status.unwrap_or(match state.status {
            objects::object::Status::Draft => shared::StateStatus::Draft,
            objects::object::Status::Published => shared::StateStatus::Published,
        }) as i32,
        verification,
        change_id: Some(shared::ChangeId {
            value: state.change_id.as_bytes().to_vec(),
        }),
        agent_run_id: None,
    }
}
