// SPDX-License-Identifier: Apache-2.0
//! Verdict-body assembly from resolved execution facts.

use std::{collections::BTreeMap, time::Duration};

use ci_config::{Check, CheckClass as ConfigCheckClass};
use crypto::{
    CI_VERDICT_BODY_SCHEMA_VERSION, CheckClass, CheckDescriptor, CiVerdictBody, Conclusion,
    Execution, FailureDetail, Outcome, Repro,
};

use crate::model::ExecutionContext;

pub(crate) struct BodyInputs<'a> {
    pub(crate) conclusion: Conclusion,
    pub(crate) failure: Option<FailureDetail>,
    pub(crate) environment: &'a BTreeMap<String, String>,
    pub(crate) ran_suites: Vec<String>,
    pub(crate) skipped_suites: Vec<String>,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) duration: Duration,
}

pub(crate) fn build_body(
    check: &Check,
    context: &ExecutionContext,
    inputs: BodyInputs<'_>,
) -> CiVerdictBody {
    let services: Vec<_> = check
        .services
        .iter()
        .map(|service| service.name.clone())
        .collect();
    CiVerdictBody {
        schema_version: CI_VERDICT_BODY_SCHEMA_VERSION,
        repo: context.repo.clone(),
        state: context.state.clone(),
        basis: context.basis.clone(),
        check: CheckDescriptor {
            name: check.name.clone(),
            class: map_class(check.class),
            definition_digest: context.definition_digest.clone(),
            command: check.command.clone(),
            image_digest: context.image_digest.clone(),
            toolchain: context.toolchain.clone(),
            params: BTreeMap::new(),
            services: services.clone(),
            node_id: None,
        },
        outcome: Outcome {
            conclusion: inputs.conclusion,
            failure: inputs.failure,
        },
        execution: Execution {
            pick_id: context.pick_id.clone(),
            attempt: context.attempt,
            runner: context.runner.clone(),
            started_at: inputs.started_at,
            finished_at: inputs.finished_at,
            duration_ms: u64::try_from(inputs.duration.as_millis()).unwrap_or(u64::MAX),
            ran_suites: inputs.ran_suites,
            skipped_suites: inputs.skipped_suites,
            runner_pool: None,
            trust_tier: None,
            isolation_tier: None,
            materialization_proof: None,
            secret_grants: Vec::new(),
        },
        log: None,
        repro: Repro {
            command: check.command.clone(),
            env: inputs.environment.clone(),
            image: None,
            services,
        },
        check_set_digest: None,
    }
}

fn map_class(class: ConfigCheckClass) -> CheckClass {
    match class {
        ConfigCheckClass::Required => CheckClass::Required,
        ConfigCheckClass::Advisory => CheckClass::Advisory,
        ConfigCheckClass::Informational => CheckClass::Informational,
    }
}
