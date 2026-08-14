// SPDX-License-Identifier: Apache-2.0
//! Verdict result assembly for completed, skipped, and infrastructure runs.

use std::{collections::BTreeMap, time::Duration};

use ci_config::Check;
use crypto::{Conclusion, FailureClass, FailureDetail};

use crate::{
    body::{BodyInputs, build_body},
    classify::{EXCERPT_CAP_BYTES, cap_bytes, classify, extract_excerpt},
    exec::ResolvedRun,
    model::{AttemptRecord, CheckResult, ExecutionContext},
    process::RunOutput,
    service::ServiceError,
    strip_ansi,
};

pub(crate) struct CompletedRun {
    pub(crate) output: RunOutput,
    pub(crate) attempts: Vec<AttemptRecord>,
    pub(crate) environment: BTreeMap<String, String>,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) duration: Duration,
}

pub(crate) fn finalize(
    check: &Check,
    context: &ExecutionContext,
    run: CompletedRun,
) -> CheckResult {
    let (conclusion, failure) = failure_from_run(check, &run.output);
    let body = build_body(
        check,
        context,
        BodyInputs {
            conclusion,
            failure,
            environment: &run.environment,
            ran_suites: vec![check.name.clone()],
            skipped_suites: Vec::new(),
            started_at: run.started_at,
            finished_at: run.finished_at,
            duration: run.duration,
        },
    );
    CheckResult {
        body,
        combined_output: run.output.combined_output,
        attempts: run.attempts.len() as u32,
        attempt_records: run.attempts,
    }
}

fn failure_from_run(check: &Check, run: &RunOutput) -> (Conclusion, Option<FailureDetail>) {
    let Some(class) = classify(run.disposition, &run.combined_output) else {
        return (Conclusion::Success, None);
    };
    let conclusion = match class {
        FailureClass::Timeout => Conclusion::TimedOut,
        FailureClass::Infra => Conclusion::InfraError,
        _ => Conclusion::Failure,
    };
    let failure = FailureDetail {
        class,
        subclass: None,
        failing_step: Some(check.name.clone()),
        excerpt: extract_excerpt(&run.combined_output, class),
        excerpt_encoding: "utf8".to_string(),
    };
    (conclusion, Some(failure))
}

pub(crate) fn skipped_result(
    check: &Check,
    context: &ExecutionContext,
    run: &ResolvedRun<'_>,
) -> CheckResult {
    let at = (run.options.now_rfc3339)();
    let environment = run
        .environment
        .build(&check.env, &BTreeMap::new(), &BTreeMap::new());
    CheckResult {
        body: build_body(
            check,
            context,
            BodyInputs {
                conclusion: Conclusion::Skipped,
                failure: None,
                environment: &environment,
                ran_suites: Vec::new(),
                skipped_suites: vec![check.name.clone()],
                started_at: at.clone(),
                finished_at: at,
                duration: Duration::ZERO,
            },
        ),
        combined_output: String::new(),
        attempts: 0,
        attempt_records: Vec::new(),
    }
}

pub(crate) fn infra_result(
    check: &Check,
    context: &ExecutionContext,
    run: &ResolvedRun<'_>,
    started_at: String,
    duration: Duration,
    error: &ServiceError,
) -> CheckResult {
    let detail = format!("service provisioning failed: {error}");
    let excerpt = cap_bytes(&strip_ansi(&detail), EXCERPT_CAP_BYTES);
    let environment = BTreeMap::new();
    CheckResult {
        body: build_body(
            check,
            context,
            BodyInputs {
                conclusion: Conclusion::InfraError,
                failure: Some(FailureDetail {
                    class: FailureClass::Infra,
                    subclass: Some("service_provisioning".to_string()),
                    failing_step: Some(check.name.clone()),
                    excerpt,
                    excerpt_encoding: "utf8".to_string(),
                }),
                environment: &environment,
                ran_suites: Vec::new(),
                skipped_suites: Vec::new(),
                started_at,
                finished_at: (run.options.now_rfc3339)(),
                duration,
            },
        ),
        combined_output: detail,
        attempts: 0,
        attempt_records: Vec::new(),
    }
}
