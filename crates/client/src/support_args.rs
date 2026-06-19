//! `heddle support` — customer-issued temporary admin grants for Heddle
//! staff. Mirrors the `GrantSupportAccess` / `ListSupportAccessGrants` /
//! `RevokeSupportAccess` RPCs on `HostedUserService`.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum SupportCommands {
    /// Grant a Heddle staff member temporary admin on a namespace or
    /// repository. Reason and TTL are required; the server enforces a
    /// hard cap of 7 days.
    Grant(SupportGrantArgs),
    /// List active (or all) support-access grants on a namespace/repo.
    /// Caller must hold Admin on the target.
    List(SupportListArgs),
    /// Revoke an active support-access grant by id.
    Revoke(SupportRevokeArgs),
}

#[derive(Clone, Debug, Args)]
pub struct SupportGrantArgs {
    /// The Heddle staff email being granted access.
    pub operator_email: String,
    /// Namespace path, e.g. `org/acme`. Mutually exclusive with --target-repo.
    #[arg(long, conflicts_with = "support_repo")]
    pub namespace: Option<String>,
    /// Repository path, e.g. `org/acme/heddle`. Mutually exclusive with
    /// --namespace.
    #[arg(
        long = "target-repo",
        id = "support_repo",
        conflicts_with = "namespace"
    )]
    pub repo: Option<String>,
    /// Time-to-live, e.g. `2h`, `24h`, `4d`. Hard-capped at 7d server-side.
    #[arg(long, default_value = "24h")]
    pub ttl: String,
    /// Free-form reason, surfaced in the audit listing. Required.
    #[arg(long)]
    pub reason: String,
    /// Remote that maps to the remote service (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct SupportListArgs {
    /// Namespace path. Mutually exclusive with --target-repo.
    #[arg(long, conflicts_with = "support_repo")]
    pub namespace: Option<String>,
    /// Repository path. Mutually exclusive with --namespace.
    #[arg(
        long = "target-repo",
        id = "support_repo",
        conflicts_with = "namespace"
    )]
    pub repo: Option<String>,
    /// Include revoked + expired entries. Defaults to active-only.
    #[arg(long)]
    pub include_inactive: bool,
    /// Remote that maps to the remote service (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct SupportRevokeArgs {
    /// Audit-row id of the grant to revoke (UUID).
    pub id: String,
    /// Remote that maps to the remote service (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}
