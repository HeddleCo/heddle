// SPDX-License-Identifier: Apache-2.0
//! Outcome, execution, and reproduction sections of a CI verdict body.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Terminal outcome of a check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Outcome {
    /// Terminal conclusion.
    pub conclusion: Conclusion,
    /// Structured triage detail when the check failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureDetail>,
}

/// Exhaustive terminal check conclusions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// Check passed.
    #[default]
    Success,
    /// Check failed with code or assertion evidence.
    Failure,
    /// Check was cancelled before completion.
    Cancelled,
    /// Check was intentionally skipped.
    Skipped,
    /// Check exceeded its deadline.
    TimedOut,
    /// Check could not run because of infrastructure.
    InfraError,
}

/// Failure details suitable for routing a repair attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailureDetail {
    /// Broad failure class.
    pub class: FailureClass,
    /// Optional finer-grained failure subclass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subclass: Option<String>,
    /// Failing step or check name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    /// ANSI-stripped, producer-capped, untrusted error excerpt.
    pub excerpt: String,
    /// Encoding of the excerpt, such as `utf8`.
    pub excerpt_encoding: String,
}

/// Broad class of a check failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Compile or build failure.
    #[default]
    Build,
    /// Test assertion or panic.
    Test,
    /// Lint failure.
    Lint,
    /// Benchmark failure or regression.
    Bench,
    /// Runner or service infrastructure failure.
    Infra,
    /// Execution timeout.
    Timeout,
    /// Speculative merge conflict.
    MergeConflict,
}

/// Runner, timing, and attestation metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Execution {
    /// Leased run identifier; absent for purely local execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick_id: Option<String>,
    /// One-based run attempt.
    pub attempt: u32,
    /// Runner principal identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// RFC3339 execution start.
    pub started_at: String,
    /// RFC3339 execution finish.
    pub finished_at: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Suites that actually ran.
    pub ran_suites: Vec<String>,
    /// Suites intentionally skipped.
    pub skipped_suites: Vec<String>,
    /// Runner pool that produced the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_pool: Option<String>,
    /// Runner trust tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_tier: Option<String>,
    /// Sandbox isolation tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_tier: Option<String>,
    /// Proof that the evaluated tree was materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialization_proof: Option<String>,
    /// Names of secret grants; values never enter the verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_grants: Vec<String>,
}

/// Pointer to a finalized log blob.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LogRef {
    /// Digest of the log manifest.
    pub manifest_digest: String,
    /// Total log size in bytes.
    pub size_bytes: u64,
}

/// Exact local reproduction recipe.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Repro {
    /// Argument vector to execute.
    pub command: Vec<String>,
    /// Sorted environment variables to set.
    pub env: BTreeMap<String, String>,
    /// Optional container image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Required service containers.
    pub services: Vec<String>,
}
