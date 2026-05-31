// SPDX-License-Identifier: Apache-2.0
//! Output shape for the spike — an ILLUSTRATIVE, DIRECTIONAL measurement aid.
//!
//! This is NOT a byte-faithful mirror of the real init output. It models the
//! shape of `crates/cli/src/cli/commands/init.rs` `InitOutput` closely enough to
//! demonstrate the schemars-vs-custom-emitter tradeoff *directionally* — which
//! path pins the discriminator, which re-exposes a skip-serialized field — but
//! exact field values, byte counts, and the `RepositoryVerificationState`
//! stand-in are approximate/representative, not production-exact. heddle#205
//! derives the real macro from `init.rs` directly, not from this throwaway crate.
//!
//! The load-bearing structural facts it DOES reproduce: the field is an
//! `#[serde(skip_serializing)] #[serde(rename = "verification")]` `trust` field
//! (mirroring `init.rs:41-44`) that the hand-written `InitSchema` mirror
//! (`schemas.rs:842`) deliberately drops.
//!
//! That `trust` field is the load-bearing difference. The mirror omits it by
//! hand, so today there is no schema/serialize drift. But heddle#205's plan is
//! to DELETE the mirror and derive `JsonSchema` on the real `InitOutput`; once
//! that happens schemars re-introduces a `verification` property the wire bytes
//! never carry. Measuring against the mirror would have hidden this; measuring
//! against the real shape surfaces it.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Serialize;

/// `heddle init --output json` response — modeled illustratively on the real
/// private `InitOutput` (`crates/cli/src/cli/commands/init.rs:22-45`): the
/// serde-relevant shape and attributes, not a verbatim copy (see
/// `RepositoryVerificationState` below for a deliberate divergence).
///
/// Initialize Heddle metadata. In a plain Git repository this creates the
/// `.heddle` sidecar and updates the local `.git/info/exclude` file for Heddle
/// metadata only; it does not import Git history or write Git-tracked files.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(example = "init_example")]
pub struct InitOutput {
    /// Stable output discriminator. Always `"init"`.
    pub output_kind: &'static str,
    /// Always `initialized` on success.
    pub status: String,
    /// Always `init` on success.
    pub action: String,
    /// Path to the initialized `.heddle` metadata directory.
    pub path: PathBuf,
    /// Repository capability after init, e.g. `git-overlay`.
    pub repository_mode: String,
    /// Whether init detected an existing Git repo.
    pub git_detected: bool,
    /// Whether Heddle metadata is now present.
    pub heddle_initialized: bool,
    /// Side effect outside `.heddle`. Currently always false.
    pub installed_heddleignore: bool,
    /// Whether a signing principal was configured during init.
    pub principal_configured: bool,
    /// Principal configuration status string.
    pub principal_status: String,
    /// Where the principal identity was sourced from, if any.
    pub principal_source: Option<String>,
    /// Resolved principal identity, if one was configured.
    pub principal: Option<InitPrincipalOutput>,
    /// Suggested follow-up to configure a principal, if unset.
    pub principal_recommended_action: Option<String>,
    /// Human-readable, machine-preserved list of what init changed.
    pub side_effects: Vec<String>,
    /// Human summary line.
    pub message: String,
    /// Primary verification-guided next command.
    pub next_action: Option<String>,
    /// Alias of `next_action` for the shared recovery-advice contract.
    pub recommended_action: Option<String>,
    /// Repository verification block. Attributes copied verbatim from the real
    /// `trust` field. serde DROPS it from `--output json`
    /// (`#[serde(skip_serializing)]`); schemars' derive still emits a
    /// `verification` property (a `writeOnly` field — it stays in the
    /// deserialize contract). That asymmetry is the schema/serialize drift this
    /// PoC measures and `tests/measure.rs` asserts.
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

/// Resolved principal identity attached to an init reply.
#[derive(Debug, Serialize, JsonSchema)]
pub struct InitPrincipalOutput {
    /// Principal display name.
    pub name: String,
    /// Principal email.
    pub email: String,
}

/// Minimal stand-in for the real `pub(crate)` `RepositoryVerificationState`
/// (`crates/cli/src/cli/commands/git_overlay_health.rs:51`). The real type is
/// large, crate-private, and has nested schema-bearing types; this stand-in is
/// deliberately small. Only its PRESENCE as a `skip_serializing` /
/// `rename = "verification"` field drives the drift fact this PoC asserts (a
/// phantom `verification` property appears in the schemars schema but never on
/// the wire). The schema *byte counts* printed by `print_measurement_table` are
/// therefore APPROXIMATE/directional — the real type would expand larger under
/// the naive derive, so the schemars-vs-custom size gap is understated here, not
/// overstated. The measured contract is the directional inequality and the
/// property presence/absence, not the exact byte magnitudes.
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepositoryVerificationState {
    pub verified: bool,
    pub status: String,
    pub summary: String,
}

/// Illustrative TYPED value used to drive the example-carry-through measurement.
///
/// This is a REPRESENTATIVE value, not the canonical `init` output and not a
/// rebaseline source for heddle#205 (individual field values — e.g. the
/// principal status string — are illustrative; the real command computes them).
/// The DIRECTIONAL point it demonstrates: a typed example tracks the struct, so
/// it always carries the always-serialized fields (`principal_source`,
/// `principal`, `principal_recommended_action` — no `skip_serializing_if`) that
/// a hand-curated prose sample can drift away from. `tests/measure.rs` asserts
/// that *presence* divergence, not the literal values — evidence for the spike's
/// "typed examples beat hand-curated prose samples" point. The `trust` block is
/// built but never serialized (`skip_serializing`).
pub fn init_example() -> InitOutput {
    InitOutput {
        output_kind: "init",
        status: "initialized".into(),
        action: "init".into(),
        path: PathBuf::from("/repo/.heddle"),
        repository_mode: "git-overlay".into(),
        git_detected: true,
        heddle_initialized: true,
        installed_heddleignore: false,
        principal_configured: false,
        principal_status: "unset".into(),
        principal_source: None,
        principal: None,
        principal_recommended_action: Some(
            "heddle init --principal-name <name> --principal-email <email>".into(),
        ),
        side_effects: vec![
            "created Heddle sidecar for the existing Git repository".into(),
            "updated .git/info/exclude for Heddle metadata".into(),
            "left Git-tracked files untouched".into(),
        ],
        message: "Initialized Heddle data in /repo/.heddle for Git-overlay workflows".into(),
        next_action: Some("heddle adopt --ref main".into()),
        recommended_action: Some("heddle adopt --ref main".into()),
        trust: RepositoryVerificationState {
            verified: true,
            status: "verified".into(),
            summary: "Git-overlay repository initialized and verified".into(),
        },
    }
}
