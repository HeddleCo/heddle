// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    ConfidenceBand as ProtoConfidenceBand, IntegrationPolicyStatus as ProtoIntegrationPolicyStatus,
    ThreadFreshness as ProtoThreadFreshness, ThreadMode as ProtoThreadMode, ThreadSummary,
    thread_state::Kind as ProtoThreadState,
};
use objects::{object::StateId, store::ObjectStore};
use repo::{Repository, SyncedThreadMetadata};
use wire::ProtocolError;

pub(super) fn from_summary(
    repo: &Repository,
    remote_thread: &str,
    pulled_state: StateId,
    summary: ThreadSummary,
) -> Result<SyncedThreadMetadata, ProtocolError> {
    if summary.name != remote_thread {
        return Err(invalid_metadata(
            remote_thread,
            format!("was named '{}'", summary.name),
        ));
    }
    if summary.thread_id.is_empty() {
        return Err(invalid_metadata(
            remote_thread,
            "is missing its stable identity",
        ));
    }
    let base_state = super::helpers::parse_proto_state_id(summary.base_state)?
        .ok_or_else(|| invalid_metadata(remote_thread, "is missing its managed base state"))?;
    let advertised_current = super::helpers::parse_proto_state_id(summary.current_state)?;
    if advertised_current != Some(pulled_state) {
        return Err(invalid_metadata(
            remote_thread,
            format!("does not match pulled state {pulled_state}"),
        ));
    }
    let base_root = repo
        .store()
        .get_state(&base_state)?
        .ok_or_else(|| {
            invalid_metadata(
                remote_thread,
                format!("names base state {base_state}, which was not transferred"),
            )
        })?
        .tree
        .short();
    let state = thread_state(remote_thread, summary.thread_state)?;
    let now = chrono::Utc::now();

    Ok(SyncedThreadMetadata {
        id: summary.thread_id,
        thread: remote_thread.to_string(),
        target_thread: summary.target_thread,
        parent_thread: summary.parent_thread,
        mode: thread_mode(remote_thread, summary.thread_mode)?,
        state: state.clone(),
        base_state: base_state.short(),
        base_root,
        current_state: Some(pulled_state.short()),
        merged_state: (state == repo::ThreadState::Merged).then(|| pulled_state.short()),
        task: summary.task,
        changed_paths: summary.changed_paths,
        impact_categories: impact_categories(remote_thread, summary.impact_categories)?,
        heavy_impact_paths: summary.heavy_impact_paths,
        promotion_suggested: summary.promotion_suggested,
        freshness: freshness(summary.freshness),
        verification_summary: verification_summary(summary.verification_summary),
        confidence_summary: confidence_summary(summary.confidence_summary),
        integration_policy_result: integration_policy(summary.integration_policy_result),
        created_at: now,
        updated_at: now,
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    })
}

fn invalid_metadata(remote_thread: &str, detail: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(format!("hosted thread '{remote_thread}' metadata {detail}"))
}

fn thread_state(remote_thread: &str, value: i32) -> Result<repo::ThreadState, ProtocolError> {
    match ProtoThreadState::try_from(value).ok() {
        Some(ProtoThreadState::ThreadStateDraft) => Ok(repo::ThreadState::Draft),
        Some(ProtoThreadState::ThreadStateActive) => Ok(repo::ThreadState::Active),
        Some(ProtoThreadState::ThreadStateReady) => Ok(repo::ThreadState::Ready),
        Some(ProtoThreadState::ThreadStateBlocked) => Ok(repo::ThreadState::Blocked),
        Some(ProtoThreadState::ThreadStateMerged) => Ok(repo::ThreadState::Merged),
        Some(ProtoThreadState::ThreadStateAbandoned) => Ok(repo::ThreadState::Abandoned),
        Some(ProtoThreadState::ThreadStatePromoted) => Ok(repo::ThreadState::Promoted),
        _ => Err(invalid_metadata(
            remote_thread,
            "has no managed lifecycle state",
        )),
    }
}

fn thread_mode(remote_thread: &str, value: i32) -> Result<repo::ThreadMode, ProtocolError> {
    match ProtoThreadMode::try_from(value).ok() {
        Some(ProtoThreadMode::Materialized) => Ok(repo::ThreadMode::Materialized),
        Some(ProtoThreadMode::Virtualized) => Ok(repo::ThreadMode::Virtualized),
        Some(ProtoThreadMode::Solid) => Ok(repo::ThreadMode::Solid),
        _ => Err(invalid_metadata(
            remote_thread,
            "has no managed workspace mode",
        )),
    }
}

fn freshness(value: i32) -> repo::ThreadFreshness {
    match ProtoThreadFreshness::try_from(value).ok() {
        Some(ProtoThreadFreshness::Current) => repo::ThreadFreshness::Current,
        Some(ProtoThreadFreshness::Stale) => repo::ThreadFreshness::Stale,
        _ => repo::ThreadFreshness::Unknown,
    }
}

fn verification_summary(
    value: Option<api::heddle::api::v1alpha1::ThreadVerificationSummary>,
) -> repo::ThreadVerificationSummary {
    value.map_or_else(repo::ThreadVerificationSummary::default, |summary| {
        repo::ThreadVerificationSummary {
            tests_passed: summary.tests_passed,
            tests_failed: Some(summary.tests_failed),
            coverage_pct: summary.coverage_pct,
            lint_warnings: summary.lint_warnings,
        }
    })
}

fn confidence_summary(
    value: Option<api::heddle::api::v1alpha1::ThreadConfidenceSummary>,
) -> repo::ThreadConfidenceSummary {
    value.map_or_else(repo::ThreadConfidenceSummary::default, |summary| {
        repo::ThreadConfidenceSummary {
            value: summary.value,
            band: match ProtoConfidenceBand::try_from(summary.band).ok() {
                Some(ProtoConfidenceBand::Low) => Some(repo::ConfidenceBand::Low),
                Some(ProtoConfidenceBand::Medium) => Some(repo::ConfidenceBand::Medium),
                Some(ProtoConfidenceBand::High) => Some(repo::ConfidenceBand::High),
                _ => None,
            },
        }
    })
}

fn integration_policy(
    value: Option<api::heddle::api::v1alpha1::ThreadIntegrationPolicy>,
) -> repo::ThreadIntegrationPolicy {
    value.map_or_else(repo::ThreadIntegrationPolicy::default, |policy| {
        repo::ThreadIntegrationPolicy {
            status: match ProtoIntegrationPolicyStatus::try_from(policy.status).ok() {
                Some(ProtoIntegrationPolicyStatus::Previewed) => Some("previewed".to_string()),
                Some(ProtoIntegrationPolicyStatus::Current) => Some("current".to_string()),
                Some(ProtoIntegrationPolicyStatus::Blocked) => Some("blocked".to_string()),
                Some(ProtoIntegrationPolicyStatus::ManualResolved) => {
                    Some("manual_resolved".to_string())
                }
                Some(ProtoIntegrationPolicyStatus::AutoIntegrated) => {
                    Some("auto_integrated".to_string())
                }
                _ => None,
            },
            reason: (!policy.reason.is_empty()).then_some(policy.reason),
            manual_resolution_state: None,
            conflicts_resolved_manually: false,
        }
    })
}

fn impact_categories(
    remote_thread: &str,
    values: Vec<String>,
) -> Result<Vec<repo::ThreadImpactCategory>, ProtocolError> {
    values
        .into_iter()
        .map(|category| match category.as_str() {
            "dependency_graph" => Ok(repo::ThreadImpactCategory::DependencyGraph),
            "build_runtime_config" => Ok(repo::ThreadImpactCategory::BuildRuntimeConfig),
            "generated_outputs" => Ok(repo::ThreadImpactCategory::GeneratedOutputs),
            "repo_wide_refactor" => Ok(repo::ThreadImpactCategory::RepoWideRefactor),
            "public_api_surface" => Ok(repo::ThreadImpactCategory::PublicApiSurface),
            _ => Err(invalid_metadata(
                remote_thread,
                format!("has unknown impact category '{category}'"),
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha1::{
        ThreadFreshness as ProtoThreadFreshness, ThreadMode as ProtoThreadMode, ThreadSummary,
        thread_state::Kind as ProtoThreadState,
    };
    use repo::Repository;
    use tempfile::TempDir;

    use super::from_summary;

    #[test]
    fn hosted_thread_summary_rehydrates_managed_metadata_at_pulled_tip() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();
        std::fs::write(temp.path().join("runner.txt"), "runner\n").unwrap();
        let tip = repo.snapshot(Some("runner".to_string()), None).unwrap();
        let summary = ThreadSummary {
            name: "shuttle/runner".to_string(),
            thread_id: "thread-stable-runner".to_string(),
            base_state: super::super::helpers::proto_state_id(base.state_id),
            current_state: super::super::helpers::proto_state_id(tip.state_id),
            target_thread: Some("main".to_string()),
            task: Some("fixture runner".to_string()),
            thread_mode: ProtoThreadMode::Solid as i32,
            freshness: ProtoThreadFreshness::Current as i32,
            thread_state: ProtoThreadState::ThreadStateReady as i32,
            changed_paths: vec!["runner.txt".to_string()],
            ..ThreadSummary::default()
        };

        let metadata = from_summary(&repo, "shuttle/runner", tip.state_id, summary).unwrap();
        assert_eq!(metadata.id, "thread-stable-runner");
        assert_eq!(metadata.thread, "shuttle/runner");
        assert_eq!(metadata.target_thread.as_deref(), Some("main"));
        assert_eq!(metadata.state, repo::ThreadState::Ready);
        assert_eq!(metadata.current_state, Some(tip.state_id.short()));
        assert_eq!(metadata.changed_paths, vec!["runner.txt"]);
    }
}
