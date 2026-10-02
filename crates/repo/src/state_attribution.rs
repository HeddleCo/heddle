// SPDX-License-Identifier: Apache-2.0
//! Read immutable attribution from the exact State being displayed.

use objects::{
    error::{HeddleError, Result},
    object::{ATTRIBUTION_EVIDENCE_MAX_BYTES, Agent, AttributionEvidenceV1, State},
    store::ObjectSource,
};

/// An evidence pointer is required metadata, not an optional best-effort hint.
/// Legacy States without a pointer do not acquire inferred attribution here.
pub fn load_attribution_evidence(
    source: &(impl ObjectSource + ?Sized),
    state: &State,
) -> Result<Option<AttributionEvidenceV1>> {
    let Some(hash) = state.attribution_evidence else {
        return Ok(None);
    };
    let missing = || HeddleError::MissingObject {
        object_type: "attribution evidence".to_string(),
        id: hash.to_hex(),
    };
    let size = source.decoded_blob_len(&hash)?.ok_or_else(missing)?;
    if size > ATTRIBUTION_EVIDENCE_MAX_BYTES as u64 {
        return Err(HeddleError::InvalidObject(
            "attribution evidence exceeds its size limit".to_string(),
        ));
    }
    let blob = source.get_blob(&hash)?.ok_or_else(missing)?;
    decode(state, hash, &blob).map(Some)
}

#[cfg(feature = "async-source")]
pub async fn load_attribution_evidence_async(
    source: &(impl objects::store::AsyncObjectSource + ?Sized),
    state: &State,
) -> Result<Option<AttributionEvidenceV1>> {
    let Some(hash) = state.attribution_evidence else {
        return Ok(None);
    };
    let blob = source
        .get_blob(&hash)
        .await?
        .ok_or_else(|| HeddleError::MissingObject {
            object_type: "attribution evidence".to_string(),
            id: hash.to_hex(),
        })?;
    decode(state, hash, &blob).map(Some)
}

fn decode(
    state: &State,
    hash: objects::object::ContentHash,
    blob: &objects::object::Blob,
) -> Result<AttributionEvidenceV1> {
    let evidence = AttributionEvidenceV1::from_blob_with_hash(blob, hash)
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    evidence
        .validate_legacy_agent(state.attribution.agent.as_ref())
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    Ok(evidence)
}

fn model_identity<'a>(
    selected: &'a objects::object::ModelAttribution,
    response: &'a objects::object::ModelAttribution,
) -> &'a objects::object::ModelAttribution {
    if response.model.is_some() {
        response
    } else {
        selected
    }
}

fn identity_label(
    harness: Option<&objects::object::AttributionClaim>,
    identity: &objects::object::ModelAttribution,
) -> String {
    let model = match (&identity.provider, &identity.model) {
        (Some(provider), Some(model)) => format!("{}/{}", provider.value, model.value),
        (_, Some(model)) => model.value.clone(),
        _ => "model unknown".to_string(),
    };
    match harness {
        Some(harness) => format!("{} ({model})", harness.value),
        None => model,
    }
}

fn same_operation_contributor(
    left: &objects::object::AttributionOperationIdentity,
    right: &objects::object::AttributionOperationIdentity,
) -> bool {
    fn value(claim: &Option<objects::object::AttributionClaim>) -> Option<&str> {
        claim.as_ref().map(|claim| claim.value.as_str())
    }
    let left_model = model_identity(&left.selected, &left.response);
    let right_model = model_identity(&right.selected, &right.response);
    // Event IDs differ between operations by the same contributor. Actor and
    // session namespaces, including absent identities, must still agree.
    value(&left.harness) == value(&right.harness)
        && value(&left.harness_version) == value(&right.harness_version)
        && left.harness_version_scope == right.harness_version_scope
        && value(&left_model.provider) == value(&right_model.provider)
        && value(&left_model.model) == value(&right_model.model)
        && value(&left_model.version) == value(&right_model.version)
        && left.scope.heddle_session_id == right.scope.heddle_session_id
        && left.scope.heddle_segment_id == right.scope.heddle_segment_id
        && left.scope.harness_instance_id == right.scope.harness_instance_id
        && left.scope.harness_session_id == right.scope.harness_session_id
        && left.scope.actor_id == right.scope.actor_id
        && left.scope.parent_actor_id == right.scope.parent_actor_id
        && left.scope.parent_harness_session_id == right.scope.parent_harness_session_id
}

/// The response model wins over the selected alias within each contributor.
/// Only complete, content-bound, homogeneous operation attribution has a
/// singular model; capture context never replaces mixed or unresolved work.
pub fn attribution_model<'a>(
    agent: Option<&'a Agent>,
    evidence: Option<&'a AttributionEvidenceV1>,
) -> Option<&'a str> {
    let Some(evidence) = evidence else {
        return agent.map(|agent| agent.model.as_str());
    };
    if evidence.operations_incomplete {
        return None;
    }
    if let Some(first) = evidence.operations.first() {
        if evidence.operations.iter().any(|operation| {
            operation.resolution != objects::object::AttributionOperationResolution::ContentBound
                || !same_operation_contributor(&first.identity, &operation.identity)
        }) {
            return None;
        }
        return model_identity(&first.identity.selected, &first.identity.response)
            .model
            .as_ref()
            .map(|claim| claim.value.as_str());
    }
    model_identity(&evidence.selected, &evidence.response)
        .model
        .as_ref()
        .map(|claim| claim.value.as_str())
}

/// A human label, not a synthesized provider/model identity. Full, separate
/// selected and response claims remain available in the JSON/wire evidence.
pub fn attribution_agent_label(
    agent: Option<&Agent>,
    evidence: Option<&AttributionEvidenceV1>,
) -> Option<String> {
    const MAX_DISPLAYED_CONTRIBUTORS: usize = 3;

    let Some(evidence) = evidence else {
        return agent.map(ToString::to_string);
    };
    if evidence.operations.is_empty() {
        return Some(if evidence.operations_incomplete {
            "agent-assisted (incomplete attribution; model unknown)".to_string()
        } else {
            identity_label(
                evidence.harness.as_ref(),
                model_identity(&evidence.selected, &evidence.response),
            )
        });
    }

    let mut contributors: Vec<&objects::object::AttributionOperation> = Vec::new();
    for operation in &evidence.operations {
        if !contributors.iter().any(|existing| {
            existing.resolution == operation.resolution
                && same_operation_contributor(&existing.identity, &operation.identity)
        }) {
            contributors.push(operation);
        }
    }
    let mut labels: Vec<String> = contributors
        .iter()
        .take(MAX_DISPLAYED_CONTRIBUTORS)
        .map(|operation| {
            let identity = &operation.identity;
            let label = identity_label(
                identity.harness.as_ref(),
                model_identity(&identity.selected, &identity.response),
            );
            match operation.resolution {
                objects::object::AttributionOperationResolution::ContentBound => label,
                objects::object::AttributionOperationResolution::Unresolved => {
                    format!("unresolved claim: {label}")
                }
            }
        })
        .collect();
    if contributors.len() > MAX_DISPLAYED_CONTRIBUTORS {
        labels.push(format!(
            "+{} more contributors",
            contributors.len() - MAX_DISPLAYED_CONTRIBUTORS
        ));
    }
    let mut qualifiers = Vec::new();
    if contributors.len() > 1 {
        qualifiers.push("mixed attribution");
    }
    if evidence.operations_incomplete {
        qualifiers.push("incomplete attribution");
    }
    // Truncating human output must not hide unresolved activity. Complete
    // individually scoped claims remain available in the JSON/wire record.
    if contributors
        .iter()
        .skip(MAX_DISPLAYED_CONTRIBUTORS)
        .any(|operation| {
            operation.resolution == objects::object::AttributionOperationResolution::Unresolved
        })
    {
        qualifiers.push("unresolved claims");
    }
    let qualifier = if qualifiers.is_empty() {
        String::new()
    } else {
        format!(" ({})", qualifiers.join("; "))
    };
    Some(format!("agent-assisted{qualifier}: {}", labels.join("; ")))
}

#[cfg(test)]
#[path = "state_attribution_tests.rs"]
mod tests;
