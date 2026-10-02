//! Shared projection of verified, state-bound attribution into the existing API.
use anyhow::Result;
use api::heddle::api::common as shared;
use objects::{object as native, store::ObjectSource};

pub(super) fn attribution(
    source: &(impl ObjectSource + ?Sized),
    state: &native::State,
) -> Result<Option<shared::StateAttribution>> {
    let evidence = repo::load_attribution_evidence(source, state)?;
    Ok(state
        .attribution_evidence
        .zip(evidence)
        .map(|(hash, evidence)| shared::StateAttribution {
            evidence_hash: hash.as_bytes().to_vec(),
            evidence: Some(Box::new(project(evidence))),
        }))
}

fn project(evidence: native::AttributionEvidenceV1) -> shared::AttributionEvidenceV1 {
    shared::AttributionEvidenceV1 {
        format_version: u32::from(evidence.format_version),
        harness: evidence.harness.map(claim),
        harness_version: evidence.harness_version.map(claim),
        harness_version_scope: evidence.harness_version_scope.map(harness_version_scope),
        selected: Some(model(evidence.selected)),
        response: Some(model(evidence.response)),
        scope: Some(scope(evidence.scope)),
        operations: evidence.operations.into_iter().map(operation).collect(),
        operations_incomplete: evidence.operations_incomplete,
    }
}

fn harness_version_scope(scope: native::HarnessVersionScope) -> i32 {
    (match scope {
        native::HarnessVersionScope::CurrentInvocation => {
            shared::HarnessVersionScope::CurrentInvocation
        }
        native::HarnessVersionScope::SessionCreation => {
            shared::HarnessVersionScope::SessionCreation
        }
        native::HarnessVersionScope::InstalledBinary => {
            shared::HarnessVersionScope::InstalledBinary
        }
    }) as i32
}

fn scope(scope: native::AttributionScope) -> shared::AttributionScope {
    shared::AttributionScope {
        heddle_session_id: scope.heddle_session_id,
        heddle_segment_id: scope.heddle_segment_id,
        harness_instance_id: scope.harness_instance_id,
        harness_session_id: scope.harness_session_id,
        actor_id: scope.actor_id,
        parent_actor_id: scope.parent_actor_id,
        parent_harness_session_id: scope.parent_harness_session_id,
        turn_id: scope.turn_id,
        message_id: scope.message_id,
        root_turn_id: scope.root_turn_id,
        tool_call_id: scope.tool_call_id,
        request_id: scope.request_id,
        response_id: scope.response_id,
        attempt_id: scope.attempt_id,
    }
}

fn operation(operation: native::AttributionOperation) -> shared::AttributionOperation {
    let identity = operation.identity;
    shared::AttributionOperation {
        identity: Some(shared::AttributionOperationIdentity {
            harness: identity.harness.map(claim),
            harness_version: identity.harness_version.map(claim),
            harness_version_scope: identity.harness_version_scope.map(harness_version_scope),
            selected: Some(model(identity.selected)),
            response: Some(model(identity.response)),
            scope: Some(scope(identity.scope)),
            collection_methods: identity
                .collection_methods
                .into_iter()
                .map(collection_method)
                .collect(),
        }),
        changes: operation
            .changes
            .into_iter()
            .map(|change| shared::AttributionFileChange {
                path: change.path,
                before: change.before.map(blob_hash),
                after: change.after.map(blob_hash),
            })
            .collect(),
        resolution: match operation.resolution {
            native::AttributionOperationResolution::Unresolved => {
                shared::AttributionOperationResolution::Unresolved
            }
            native::AttributionOperationResolution::ContentBound => {
                shared::AttributionOperationResolution::ContentBound
            }
        } as i32,
    }
}

fn collection_method(method: native::AttributionCollectionMethod) -> i32 {
    (match method {
        native::AttributionCollectionMethod::Hook => shared::AttributionCollectionMethod::Hook,
        native::AttributionCollectionMethod::EventStream => {
            shared::AttributionCollectionMethod::EventStream
        }
        native::AttributionCollectionMethod::OpenTelemetry => {
            shared::AttributionCollectionMethod::OpenTelemetry
        }
        native::AttributionCollectionMethod::Transcript => {
            shared::AttributionCollectionMethod::Transcript
        }
        native::AttributionCollectionMethod::Proxy => shared::AttributionCollectionMethod::Proxy,
        native::AttributionCollectionMethod::Explicit => {
            shared::AttributionCollectionMethod::Explicit
        }
    }) as i32
}

fn blob_hash(hash: native::ContentHash) -> shared::AttributionBlobHash {
    shared::AttributionBlobHash {
        value: hash.as_bytes().to_vec(),
    }
}

fn model(model: native::ModelAttribution) -> shared::ModelAttribution {
    shared::ModelAttribution {
        provider: model.provider.map(claim),
        model: model.model.map(claim),
        version: model.version.map(claim),
        thought_level: model.thought_level.map(claim),
    }
}

fn claim(claim: native::AttributionClaim) -> shared::AttributionClaim {
    shared::AttributionClaim {
        value: claim.value,
        basis: match claim.basis {
            native::AttributionBasis::Configured => shared::AttributionBasis::Configured,
            native::AttributionBasis::Explicit => shared::AttributionBasis::Explicit,
            native::AttributionBasis::RequestReported => shared::AttributionBasis::RequestReported,
            native::AttributionBasis::ResponseReported => {
                shared::AttributionBasis::ResponseReported
            }
            native::AttributionBasis::Observed => shared::AttributionBasis::Observed,
            native::AttributionBasis::Legacy => shared::AttributionBasis::Legacy,
        } as i32,
        source: match claim.source {
            native::AttributionSource::HarnessHook => shared::AttributionSource::HarnessHook,
            native::AttributionSource::StatusLine => shared::AttributionSource::StatusLine,
            native::AttributionSource::Transcript => shared::AttributionSource::Transcript,
            native::AttributionSource::SessionMetadata => {
                shared::AttributionSource::SessionMetadata
            }
            native::AttributionSource::Request => shared::AttributionSource::Request,
            native::AttributionSource::Response => shared::AttributionSource::Response,
            native::AttributionSource::Configuration => shared::AttributionSource::Configuration,
            native::AttributionSource::Environment => shared::AttributionSource::Environment,
            native::AttributionSource::ExplicitArgument => {
                shared::AttributionSource::ExplicitArgument
            }
            native::AttributionSource::Process => shared::AttributionSource::Process,
            native::AttributionSource::Legacy => shared::AttributionSource::Legacy,
        } as i32,
        observation_id: claim.observation_id,
    }
}

#[cfg(test)]
#[path = "content_attribution_tests.rs"]
mod tests;
