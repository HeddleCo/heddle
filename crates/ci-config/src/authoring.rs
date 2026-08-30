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

/// Pipeline with one job containing `checks`.
#[must_use]
pub fn definition(name: &str, job: &str, checks: Vec<TreadleCheck>) -> TreadleDefinition {
    TreadleDefinition {
        format_version: 1,
        name: name.to_string(),
        jobs: vec![TreadleJob {
            name: job.to_string(),
            matrix: Vec::new(),
            checks,
        }],
        services: Vec::new(),
        secret_refs: Vec::new(),
    }
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
