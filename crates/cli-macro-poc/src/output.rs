// SPDX-License-Identifier: Apache-2.0
//! Output shape for the spike — a FAITHFUL mirror of the real init output.
//!
//! This PoC mirrors the real `crates/cli/src/cli/commands/init.rs` `InitOutput`
//! arg/output types so the measured drift/discriminator numbers reflect the
//! production surface, not a simplification. The real `InitOutput` is a private
//! (`struct`, not `pub`) type in `crates/cli`, so this throwaway crate cannot
//! import it; instead it replicates the EXACT field set, field types, and
//! serde/schemars attributes — including the `#[serde(skip_serializing)]
//! #[serde(rename = "verification")]` `trust` field (`init.rs:41-44`) that the
//! hand-written `InitSchema` mirror (`schemas.rs:842`) deliberately drops.
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

/// `heddle init --output json` response — field set, field types, and serde
/// attributes copied verbatim from the real private `InitOutput`
/// (`crates/cli/src/cli/commands/init.rs:22-45`).
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

/// Stand-in for the real `pub(crate)` `RepositoryVerificationState`
/// (`crates/cli/src/cli/commands/git_overlay_health.rs:51`). The real type is
/// large and crate-private; only its presence as a `skip_serializing` /
/// `rename = "verification"` field on `InitOutput` matters for the drift this
/// PoC measures, so a minimal `Serialize + JsonSchema` stand-in is faithful for
/// that purpose.
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepositoryVerificationState {
    pub verified: bool,
    pub status: String,
    pub summary: String,
}

/// Canonical TYPED value the real `heddle init` emits.
///
/// NOTE: this is the real emitted shape, not the curated `docs/json-schemas.md`
/// sample. The real `InitOutput` always serializes `principal_source`,
/// `principal`, and `principal_recommended_action` (no `skip_serializing_if`),
/// so they appear here as `null`/value even though the documented sample omits
/// them. `tests/measure.rs` asserts this typed-example-vs-doc-sample divergence
/// — it is itself evidence for the spike's "typed examples beat hand-curated
/// prose samples" point, and flags that heddle#205 must rebaseline the prose
/// sample. The `trust` block is built but never serialized (`skip_serializing`).
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
