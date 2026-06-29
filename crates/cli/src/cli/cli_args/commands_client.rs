// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::{Args, Subcommand};

#[derive(Subcommand, Clone, Debug)]
pub enum AuthCommands {
    /// Authenticate with a Heddle server
    Login {
        /// Heddle server address
        #[arg(long, default_value = "grpc.heddle.sh")]
        server: String,

        /// Don't open a browser automatically
        #[arg(long)]
        no_browser: bool,
    },

    /// Remove stored credentials for a server
    Logout {
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
    },

    /// Show current authentication status
    Status {
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
    },

    /// Create a service token for CI/scripts, scoped to a namespace
    CreateServiceToken {
        /// Display name for the service account (e.g. "github-ci-main")
        name: String,
        /// Namespace to scope the token to (e.g. "heddle/platform")
        #[arg(long)]
        namespace: String,
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
    },
}

impl From<AuthCommands> for heddle_client::AuthCommand {
    fn from(command: AuthCommands) -> Self {
        match command {
            AuthCommands::Login { server, no_browser } => {
                heddle_client::AuthCommand::Login { server, no_browser }
            }
            AuthCommands::Logout { server } => heddle_client::AuthCommand::Logout { server },
            AuthCommands::Status { server } => heddle_client::AuthCommand::Status { server },
            AuthCommands::CreateServiceToken {
                name,
                namespace,
                server,
            } => heddle_client::AuthCommand::CreateServiceToken {
                name,
                namespace,
                server,
            },
        }
    }
}

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

impl From<SupportCommands> for heddle_client::SupportCommand {
    fn from(command: SupportCommands) -> Self {
        match command {
            SupportCommands::Grant(args) => heddle_client::SupportCommand::Grant(args.into()),
            SupportCommands::List(args) => heddle_client::SupportCommand::List(args.into()),
            SupportCommands::Revoke(args) => heddle_client::SupportCommand::Revoke(args.into()),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct SupportGrantArgs {
    /// The Heddle staff email being granted access.
    pub operator_email: String,
    /// Namespace path, e.g. `org/acme`. Mutually exclusive with --target-repo.
    #[arg(long, conflicts_with = "support_repo")]
    pub namespace: Option<String>,
    /// Hosted repository path, e.g. `org/acme/heddle`. Mutually exclusive with
    /// --namespace. The global --repo flag still selects the local Heddle repo.
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
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

impl From<SupportGrantArgs> for heddle_client::SupportGrant {
    fn from(args: SupportGrantArgs) -> Self {
        Self {
            operator_email: args.operator_email,
            namespace: args.namespace,
            repo: args.repo,
            ttl: args.ttl,
            reason: args.reason,
            remote: args.remote,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct SupportListArgs {
    /// Namespace path. Mutually exclusive with --target-repo.
    #[arg(long, conflicts_with = "support_repo")]
    pub namespace: Option<String>,
    /// Hosted repository path. Mutually exclusive with --namespace. The global
    /// --repo flag still selects the local Heddle repo.
    #[arg(
        long = "target-repo",
        id = "support_repo",
        conflicts_with = "namespace"
    )]
    pub repo: Option<String>,
    /// Include revoked + expired entries. Defaults to active-only.
    #[arg(long)]
    pub include_inactive: bool,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

impl From<SupportListArgs> for heddle_client::SupportList {
    fn from(args: SupportListArgs) -> Self {
        Self {
            namespace: args.namespace,
            repo: args.repo,
            include_inactive: args.include_inactive,
            remote: args.remote,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct SupportRevokeArgs {
    /// Audit-row id of the grant to revoke (UUID).
    pub id: String,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

impl From<SupportRevokeArgs> for heddle_client::SupportRevoke {
    fn from(args: SupportRevokeArgs) -> Self {
        Self {
            id: args.id,
            remote: args.remote,
        }
    }
}
