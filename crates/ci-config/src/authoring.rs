// SPDX-License-Identifier: Apache-2.0
//! Construct a valid host-exec `TreadleDefinition` in Rust.
//!
//! Authoring for real pipelines lives in the api SDK. These helpers exist so
//! tests (and this runner) can emit canonical bytes without a second language.

use api::{
    heddle::api::v1alpha1::{
        TreadleCheck, TreadleCheckClass, TreadleDefinition, TreadleDeterminismClass,
        TreadleIsolationHints, TreadleJob, TreadleNetworkAccess, TreadlePlatform, TreadleRetry,
        TreadleTargetEnvironment, TreadleTrigger, TreadleTriggerKind,
    },
    treadle::{canonical_treadle_definition_bytes, treadle_definition_blake3},
};
use prost::Message;

use crate::load::{ConfigError, host_oci_platform};

/// Lockfile format_version written next to a definition. Same as definition v1.
pub const TREADLE_LOCK_FORMAT: u32 = 1;

/// A required argv check that is eligible for local host-exec on this machine.
#[must_use]
pub fn argv_check(name: &str, command: &str, args: &[&str]) -> TreadleCheck {
    TreadleCheck {
        name: name.to_string(),
        command: command.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        class: TreadleCheckClass::Required as i32,
        timeout_seconds: 60,
        env: Vec::new(),
        working_directory: String::new(),
        service_dependencies: Vec::new(),
        retry: Some(TreadleRetry {
            max_retries: 0,
            flake_signatures: Vec::new(),
        }),
        cache_paths: Vec::new(),
        isolation: Some(TreadleIsolationHints {
            profile: String::new(),
            network_access: TreadleNetworkAccess::None as i32,
            cpu_millis: 0,
            memory_bytes: 0,
            process_limit: 0,
        }),
        triggers: vec![TreadleTrigger {
            kind: TreadleTriggerKind::Push as i32,
            cron_expression: String::new(),
        }],
        supersede_older_runs: true,
        target_environment: Some(host_target_environment()),
        determinism_class: TreadleDeterminismClass::Deterministic as i32,
    }
}

/// One concrete job.
#[must_use]
pub fn job(name: &str, checks: Vec<TreadleCheck>) -> TreadleJob {
    TreadleJob {
        name: name.to_string(),
        matrix: Vec::new(),
        checks,
    }
}

/// Pipeline with the given jobs. Matrix expansion is already done.
#[must_use]
pub fn pipeline(name: &str, jobs: Vec<TreadleJob>) -> TreadleDefinition {
    TreadleDefinition {
        format_version: 1,
        name: name.to_string(),
        jobs,
        services: Vec::new(),
        secret_refs: Vec::new(),
    }
}

/// Pipeline with one job containing `checks`.
#[must_use]
pub fn definition(name: &str, job_name: &str, checks: Vec<TreadleCheck>) -> TreadleDefinition {
    pipeline(name, vec![job(job_name, checks)])
}

/// Two-job host-exec pipeline: `/bin/true`, `/bin/echo`, and another `/bin/true`.
///
/// Proof tests write the canonical bytes + lock and run every check.
#[must_use]
pub fn host_pipeline_fixture() -> TreadleDefinition {
    pipeline(
        "local",
        vec![
            job(
                "unit",
                vec![
                    argv_check("ok", "/bin/true", &[]),
                    argv_check("echo", "/bin/echo", &["pipeline"]),
                ],
            ),
            job("docs", vec![argv_check("docs-ok", "/bin/true", &[])]),
        ],
    )
}

/// Two-job pipeline whose first required check fails. The engine is sequential,
/// not a `needs` DAG: later checks still run.
#[must_use]
pub fn host_pipeline_with_required_failure() -> TreadleDefinition {
    pipeline(
        "local",
        vec![
            job(
                "first",
                vec![
                    argv_check("fail", "/bin/false", &[]),
                    argv_check("later", "/bin/true", &[]),
                ],
            ),
            job("second", vec![argv_check("sibling", "/bin/true", &[])]),
        ],
    )
}

/// Host platform plus a stub image digest. Host-exec v0 does not pull the image.
#[must_use]
pub fn host_target_environment() -> TreadleTargetEnvironment {
    let (os, arch) = host_oci_platform();
    TreadleTargetEnvironment {
        oci_image_digest: format!("sha256:{}", "0".repeat(64)),
        platform: Some(TreadlePlatform { os, arch }),
    }
}

/// Canonical protobuf bytes and hex BLAKE3 of `definition`.
pub fn canonical_definition(
    definition: &TreadleDefinition,
) -> Result<(Vec<u8>, String), ConfigError> {
    let bytes = canonical_treadle_definition_bytes(definition)?;
    let digest = hex::encode(treadle_definition_blake3(definition)?);
    Ok((bytes, digest))
}

/// `treadle.lock.json` body for `digest`.
#[must_use]
pub fn lock_json(digest: &str) -> String {
    format!("{{\"format_version\":{TREADLE_LOCK_FORMAT},\"definition_digest\":\"{digest}\"}}")
}

/// Valid protobuf that is not the v1 canonical encoding (jobs are unsorted).
#[must_use]
pub fn non_canonical_bytes() -> Vec<u8> {
    let mut definition = definition("local", "zeta", vec![argv_check("unit", "/bin/true", &[])]);
    definition.jobs.push(TreadleJob {
        name: "alpha".to_string(),
        matrix: Vec::new(),
        checks: vec![argv_check("other", "/bin/true", &[])],
    });
    definition.encode_to_vec()
}
