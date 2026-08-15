// SPDX-License-Identifier: Apache-2.0
//! Portable, serializable cache entry and fail-closed comparison.

use crypto::{CiVerdictBody, Conclusion};
use serde::{Deserialize, Serialize};

use super::key::CacheKey;
use crate::model::{AttemptRecord, CheckResult};

/// Schema version of [`ResultCacheEntry`]. Bump when the bytes change.
pub const RESULT_CACHE_SCHEMA_VERSION: u32 = 1;

/// A portable cached check result, keyed only on the content-addressed triple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultCacheEntry {
    /// Entry schema version.
    pub schema_version: u32,
    /// Digest of the content-addressed environment `E`.
    pub env_digest: String,
    /// Content-addresses of the evaluated inputs.
    pub input_digests: Vec<String>,
    /// Digest of the authored definition.
    pub definition_digest: String,
    /// Check name within that definition.
    pub check_name: String,
    /// BLAKE3 of the captured output (logs are not the cache key).
    pub evidence_digest: String,
    /// Reusable verdict body (a cache hit *is* a verdict).
    pub body: CiVerdictBody,
    /// ANSI-stripped combined output reused on a hit.
    pub combined_output: String,
    /// Attempt count from the original run.
    pub attempts: u32,
    /// Operational attempt records; not part of the signed body.
    pub attempt_records: Vec<AttemptRecord>,
}

/// Details of a fail-closed spot-check disagreement.
#[derive(Debug)]
pub struct SpotCheckDivergence {
    /// Check that disagreed.
    pub check_name: String,
    /// Conclusion stored in the cache entry.
    pub cached_conclusion: String,
    /// Evidence digest stored in the cache entry.
    pub cached_evidence: String,
    /// Conclusion produced by the fresh run.
    pub fresh_conclusion: String,
    /// Evidence digest of the fresh run.
    pub fresh_evidence: String,
    /// Environment digest from the lookup key.
    pub env_digest: String,
    /// Input digests from the lookup key.
    pub input_digests: Vec<String>,
    /// Definition digest from the lookup key.
    pub definition_digest: String,
}

impl std::fmt::Display for SpotCheckDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ci result cache spot-check failed for check `{}`: \
             cached (conclusion={}, evidence={}) \
             disagrees with fresh (conclusion={}, evidence={}); \
             refusing to trust the cache entry \
             [env={} inputs={:?} definition={}]",
            self.check_name,
            self.cached_conclusion,
            self.cached_evidence,
            self.fresh_conclusion,
            self.fresh_evidence,
            self.env_digest,
            self.input_digests,
            self.definition_digest
        )
    }
}

impl std::error::Error for SpotCheckDivergence {}

/// Errors from cache I/O or a fail-closed spot-check disagreement.
#[derive(Debug, thiserror::Error)]
pub enum ResultCacheError {
    /// Filesystem or encoding failure while reading or writing an entry.
    #[error("ci result cache I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A sampled cache hit disagreed with a fresh run. Never trusted.
    #[error(transparent)]
    SpotCheckDivergence(Box<SpotCheckDivergence>),
}

impl ResultCacheEntry {
    /// Build a portable entry from a completed check result.
    #[must_use]
    pub fn from_result(key: &CacheKey, check_name: &str, result: &CheckResult) -> Self {
        Self {
            schema_version: RESULT_CACHE_SCHEMA_VERSION,
            env_digest: key.env_digest.clone(),
            input_digests: key.input_digests.clone(),
            definition_digest: key.definition_digest.clone(),
            check_name: check_name.to_string(),
            evidence_digest: evidence_digest(&result.combined_output),
            body: result.body.clone(),
            combined_output: result.combined_output.clone(),
            attempts: result.attempts,
            attempt_records: result.attempt_records.clone(),
        }
    }

    /// Reconstruct the executor result reused on a cache hit.
    #[must_use]
    pub fn into_check_result(self) -> CheckResult {
        CheckResult {
            body: self.body,
            combined_output: self.combined_output,
            attempts: self.attempts,
            attempt_records: self.attempt_records,
        }
    }

    pub(super) fn is_valid_for(&self, key: &CacheKey, check_name: &str) -> bool {
        self.schema_version == RESULT_CACHE_SCHEMA_VERSION
            && self.env_digest == key.env_digest
            && self.input_digests == key.input_digests
            && self.definition_digest == key.definition_digest
            && self.check_name == check_name
            && self.evidence_digest == evidence_digest(&self.combined_output)
    }

    /// Fail-closed comparison of a cached entry against a fresh run.
    pub fn verify_fresh(&self, fresh: &CheckResult) -> Result<(), ResultCacheError> {
        let fresh_evidence = evidence_digest(&fresh.combined_output);
        if self.body.outcome == fresh.body.outcome && self.evidence_digest == fresh_evidence {
            return Ok(());
        }
        Err(ResultCacheError::SpotCheckDivergence(Box::new(
            SpotCheckDivergence {
                check_name: self.check_name.clone(),
                cached_conclusion: conclusion_label(self.body.outcome.conclusion).to_string(),
                cached_evidence: self.evidence_digest.clone(),
                fresh_conclusion: conclusion_label(fresh.conclusion()).to_string(),
                fresh_evidence,
                env_digest: self.env_digest.clone(),
                input_digests: self.input_digests.clone(),
                definition_digest: self.definition_digest.clone(),
            },
        )))
    }
}

/// Domain-separated BLAKE3 of captured check output.
#[must_use]
pub fn evidence_digest(combined_output: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"heddle-ci-evidence-v1\0");
    hasher.update(&(combined_output.len() as u64).to_le_bytes());
    hasher.update(combined_output.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn conclusion_label(conclusion: Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Success => "success",
        Conclusion::Failure => "failure",
        Conclusion::Cancelled => "cancelled",
        Conclusion::Skipped => "skipped",
        Conclusion::TimedOut => "timed_out",
        Conclusion::InfraError => "infra_error",
    }
}
