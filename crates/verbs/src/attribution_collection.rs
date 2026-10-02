// SPDX-License-Identifier: Apache-2.0
//! Collection methods are independent of harnesses. Adapters declare what a
//! supported harness/version actually exposes and normalize into this boundary.
//! Selecting a method never configures telemetry export, credentials or a proxy.
use crate::{IdentityCursor, OperationEventPhase};
use objects::object::{AttributionCollectionMethod, AttributionEvidenceV1};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Explicit coverage of one installed adapter, not a prediction based on a
/// harness name. Unsupported/unimplemented methods must set available=false.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributionCollectionCapability {
    pub harness: String,
    pub supported_version: Option<String>,
    pub method: AttributionCollectionMethod,
    pub available: bool,
    pub causal_operation_ids: bool,
    pub file_transitions: bool,
    pub response_model: bool,
}

/// All suitable methods can run together. There is no single-method-per-harness
/// preference and no global confidence score that silently discards conflicts.
pub fn select_attribution_methods<'a>(
    harness: &str,
    version: Option<&str>,
    requested: &[AttributionCollectionMethod],
    capabilities: &'a [AttributionCollectionCapability],
) -> Vec<&'a AttributionCollectionCapability> {
    capabilities
        .iter()
        .filter(|capability| {
            capability.available
                && capability.harness == harness
                && capability
                    .supported_version
                    .as_deref()
                    .is_none_or(|supported| Some(supported) == version)
                && (requested.is_empty() || requested.contains(&capability.method))
        })
        .collect()
}

/// Every collection method crosses the same typed boundary. Raw source events,
/// URLs, credentials, transcripts, prompts and tool arguments are excluded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributionObservation {
    pub method: AttributionCollectionMethod,
    pub identity: AttributionEvidenceV1,
    pub phase: OperationEventPhase,
    pub paths: Vec<PathBuf>,
}

pub fn record_attribution_observation(
    root: &Path,
    observation: &AttributionObservation,
) -> std::io::Result<()> {
    if observation.paths.len() > 32
        || observation.paths.iter().any(|p| p.as_os_str().len() > 1024)
        || (observation.phase == OperationEventPhase::Observe && !observation.paths.is_empty())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid attribution paths",
        ));
    }
    observation.identity.validate().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid normalized attribution observation",
        )
    })?;
    if !observation.identity.operations.is_empty() || observation.identity.operations_incomplete {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "nested capture evidence is not an observation",
        ));
    }
    let cursor = IdentityCursor {
        attribution_evidence: Some(observation.identity.clone()),
        ..Default::default()
    };
    crate::operation_attribution::record_with_method(
        root,
        &cursor,
        observation.phase,
        &observation.paths,
        observation.method,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn methods_are_composable_and_not_tied_to_a_harness_name() {
        let capabilities = [
            AttributionCollectionMethod::Hook,
            AttributionCollectionMethod::EventStream,
            AttributionCollectionMethod::OpenTelemetry,
            AttributionCollectionMethod::Transcript,
            AttributionCollectionMethod::Proxy,
        ]
        .map(|method| AttributionCollectionCapability {
            harness: "custom-agent".into(),
            supported_version: Some("1.2.3".into()),
            method,
            available: method != AttributionCollectionMethod::Proxy,
            causal_operation_ids: true,
            file_transitions: method == AttributionCollectionMethod::Hook,
            response_model: method == AttributionCollectionMethod::OpenTelemetry,
        });
        assert_eq!(
            select_attribution_methods("custom-agent", Some("1.2.3"), &[], &capabilities).len(),
            4
        );
        assert!(
            select_attribution_methods("custom-agent", Some("2.0"), &[], &capabilities).is_empty()
        );
        assert_eq!(
            select_attribution_methods(
                "custom-agent",
                Some("1.2.3"),
                &[AttributionCollectionMethod::Transcript],
                &capabilities
            )
            .len(),
            1
        );
        assert!(
            select_attribution_methods(
                "custom-agent",
                Some("1.2.3"),
                &[AttributionCollectionMethod::Proxy],
                &capabilities
            )
            .is_empty()
        );
    }
}
