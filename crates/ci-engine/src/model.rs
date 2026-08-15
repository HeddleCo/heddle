// SPDX-License-Identifier: Apache-2.0
//! Public executor inputs and outputs.

use std::path::Path;

use ci_config::Trigger;
use crypto::{Basis, CiVerdictBody, Conclusion, StateRef};
use serde::{Deserialize, Serialize};

use crate::{
    HermeticEnv, ProcGroupRegistry, ServiceProvider,
    result_cache::{ResultCache, SpotCheck},
};

/// Facts about the exact state/tree being evaluated.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// Repository identifier.
    pub repo: String,
    /// State reference carried in the verdict body.
    pub state: StateRef,
    /// Exact evaluated tree and basis.
    pub basis: Basis,
    /// Typed-blob digest of the raw definition.
    pub definition_digest: String,
    /// Optional toolchain string.
    pub toolchain: Option<String>,
    /// Hosted pick id; absent locally.
    pub pick_id: Option<String>,
    /// One-based run attempt.
    pub attempt: u32,
    /// Hosted runner identity; absent locally.
    pub runner: Option<String>,
    /// Immutable image digest, when used.
    pub image_digest: Option<String>,
}

/// Stable options for an executor invocation.
pub struct RunOptions<'a> {
    /// Directory in which argv executes.
    pub workdir: &'a Path,
    /// Service lifecycle provider.
    pub services: &'a dyn ServiceProvider,
    /// Injected RFC3339 clock.
    pub now_rfc3339: &'a dyn Fn() -> String,
}

/// Additive execution controls.
#[derive(Default)]
pub struct RunControls<'a> {
    /// Optional trigger filter.
    pub trigger: Option<Trigger>,
    /// Cache root outside the evaluated source tree.
    pub cache_root: Option<&'a Path>,
    /// Optional injected hermetic environment.
    pub hermetic_env: Option<&'a HermeticEnv>,
    /// Optional group registry for runner drain.
    pub proc_groups: Option<ProcGroupRegistry>,
    /// Optional content-addressed result cache.
    pub result_cache: Option<&'a dyn ResultCache>,
    /// Spot-check policy applied to cache hits. Ignored when no cache is set.
    pub spot_check: SpotCheck,
}

/// Operational record for one flake-retry attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// One-based attempt index.
    pub attempt: u32,
    /// Attempt conclusion.
    pub conclusion: Conclusion,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
    /// Whether output matched a flake signature.
    pub flake_matched: bool,
}

/// One check's verdict body and operational sidecars.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Signable verdict-v2 body.
    pub body: CiVerdictBody,
    /// ANSI-stripped combined output.
    pub combined_output: String,
    /// Attempts performed.
    pub attempts: u32,
    /// Attempt records, not part of the signed body.
    pub attempt_records: Vec<AttemptRecord>,
}

impl CheckResult {
    /// Terminal conclusion.
    #[must_use]
    pub fn conclusion(&self) -> Conclusion {
        self.body.outcome.conclusion
    }

    /// Whether a successful result needed a flake retry.
    #[must_use]
    pub fn recovered_after_flake(&self) -> bool {
        self.conclusion() == Conclusion::Success
            && self
                .attempt_records
                .iter()
                .any(|attempt| attempt.flake_matched)
    }
}
