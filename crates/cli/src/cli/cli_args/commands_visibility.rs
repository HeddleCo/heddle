// SPDX-License-Identifier: Apache-2.0
//! `heddle visibility` — declare and inspect a state's audience tier.
//!
//! The visibility primitive (spike #266) attaches an additive, per-state
//! `StateVisibility` sidecar record outside the hashed state bytes, so a
//! tier change never mutates the state or invalidates its signature. The
//! verb family mirrors `redact`:
//!
//! - `set` declares a tier on a state (`OpRecord::StateVisibilitySet`).
//! - `promote` appends a superseding, less-restrictive declaration
//!   (`OpRecord::StateVisibilityPromote`).
//! - `show` reports a state's effective tier (public-by-absence when none).
//! - `list` enumerates every state carrying a non-public tier.
//!
//! Capture binds the inherited default tier automatically (Invariant A); the
//! `set`/`promote` verbs are the explicit operator overrides on top of that.

use clap::{Args, Subcommand, ValueEnum};
use objects::object::VisibilityTier;

#[derive(Clone, Debug, Subcommand)]
pub enum VisibilityCommands {
    /// Declare a visibility tier on a state. Public is the default and stays
    /// record-free (absence ≡ public); a non-public tier writes a per-state
    /// sidecar record and an oplog audit entry.
    Set(VisibilitySetArgs),
    /// Promote a state to a less-restrictive tier by appending a superseding
    /// record. Requires an existing visibility record to supersede.
    Promote(VisibilityPromoteArgs),
    /// Show a state's effective visibility tier.
    Show(VisibilityShowArgs),
    /// List every state that carries a non-public visibility tier.
    List(VisibilityListArgs),
}

/// CLI surface for the tier enum. `VisibilityTier` carries a label on its
/// team-scoped / restricted variants, so it can't derive `ValueEnum`
/// directly; this flat enum + `--label` reconstructs it. Kept in lockstep
/// with `VisibilityTier` by [`VisibilityTierArg::into_tier`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum VisibilityTierArg {
    Public,
    Internal,
    TeamScoped,
    Restricted,
}

impl VisibilityTierArg {
    /// Build the [`VisibilityTier`] this arg denotes. `team-scoped` and
    /// `restricted` require `--label` (the team id / scope label); the label
    /// is ignored for `public` / `internal`. Returns the human-facing error
    /// string when a required label is missing.
    pub fn into_tier(self, label: Option<String>) -> Result<VisibilityTier, String> {
        match self {
            VisibilityTierArg::Public => Ok(VisibilityTier::Public),
            VisibilityTierArg::Internal => Ok(VisibilityTier::Internal),
            VisibilityTierArg::TeamScoped => match label {
                Some(team_id) if !team_id.trim().is_empty() => {
                    Ok(VisibilityTier::TeamScoped { team_id })
                }
                _ => Err("the team-scoped tier requires --label <team-id>".to_string()),
            },
            VisibilityTierArg::Restricted => match label {
                Some(scope_label) if !scope_label.trim().is_empty() => {
                    Ok(VisibilityTier::Restricted { scope_label })
                }
                _ => Err("the restricted tier requires --label <scope-label>".to_string()),
            },
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct VisibilitySetArgs {
    /// State to declare the tier on. Accepts short or full state IDs, marker
    /// names, `HEAD`, `@`, or `HEAD~N`.
    pub state: String,
    /// The audience tier to declare.
    #[arg(long, value_enum)]
    pub tier: VisibilityTierArg,
    /// Label for the `team-scoped` (team id) or `restricted` (scope label)
    /// tiers. Ignored for `public` / `internal`.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct VisibilityPromoteArgs {
    /// State to promote. Accepts short or full state IDs, marker names,
    /// `HEAD`, `@`, or `HEAD~N`.
    pub state: String,
    /// The less-restrictive tier to promote to.
    #[arg(long, value_enum)]
    pub tier: VisibilityTierArg,
    /// Label for the `team-scoped` / `restricted` target tier.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct VisibilityShowArgs {
    /// State to inspect.
    pub state: String,
}

#[derive(Clone, Debug, Args)]
pub struct VisibilityListArgs {}
