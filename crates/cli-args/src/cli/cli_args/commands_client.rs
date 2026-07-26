// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::{Subcommand, ValueEnum};

/// Preset operation ceilings for `heddle auth derive-agent`.
///
/// Each variant expands to a curated set of safe agent operations. `reviewer`
/// and `ci-landing` are strict subsets of the safe ceiling; `contributor` is
/// the full safe ceiling (the named form of the default `--allow`-less
/// derivation). `--scope`/`--allow` stay usable alongside a template and, when
/// combined, may only *narrow* it (they intersect the template's set).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTemplateArg {
    /// Read + review: every read RPC plus Pull. No writes, no ref moves.
    /// (ListStates/GetState/GetBlame/GetTree/GetBlob/GetDiff/GetCompare/
    /// ListActions/ListContext/GetContextHistory/GetDiscussion/ListByState/
    /// ListBySymbol/... + Pull + WhoAmI.)
    Reviewer,
    /// Read + collaboration writes: the reviewer set plus Push, UpdateRef,
    /// SetContext/ReviseContext/SupersedeContext, and
    /// OpenDiscussion/AppendTurn/ResolveDiscussion. No repo/namespace admin.
    /// This is the full safe agent ceiling — the named form of deriving with
    /// no --template/--allow.
    Contributor,
    /// Read + Pull + the Push/UpdateRef a CI lander needs to run ready/land.
    /// No context or discussion writes.
    #[value(name = "ci-landing")]
    CiLanding,
}

impl From<AgentTemplateArg> for heddle_client::device_flow::AgentTemplate {
    fn from(arg: AgentTemplateArg) -> Self {
        match arg {
            AgentTemplateArg::Reviewer => Self::Reviewer,
            AgentTemplateArg::Contributor => Self::Contributor,
            AgentTemplateArg::CiLanding => Self::CiLanding,
        }
    }
}

#[derive(Subcommand, Clone, Debug)]
pub enum AuthCommands {
    /// Authenticate with a Heddle server
    Login {
        /// Heddle server address (browser flow). Omit to use the configured
        /// default server.
        #[arg(long)]
        server: Option<String>,

        /// Open the authorization URL in the system browser.
        #[arg(long)]
        open_browser: bool,

        /// Install a verified `.hcred` credential file without a browser.
        /// The server is taken from the file.
        #[arg(long, value_name = "HCRED_PATH", conflicts_with_all = ["server", "open_browser"])]
        credential: Option<std::path::PathBuf>,
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

    /// Derive a scoped, short-lived agent token offline
    DeriveAgent {
        /// Server whose stored credential is the parent.
        #[arg(long)]
        server: String,

        /// Delegation name recorded in the Biscuit chain.
        #[arg(long)]
        agent_id: Option<String>,

        /// Child lifetime in seconds (clamped by the parent expiry).
        #[arg(long = "ttl", default_value_t = 3600)]
        ttl_secs: u64,

        /// Forward-compatible resource scope (`repo:org/name`, `namespace:org`, or a bare repo path).
        #[arg(long = "scope")]
        scopes: Vec<String>,

        /// Narrow the safe operation set (repeatable, using gRPC method names such as `Push`).
        #[arg(long = "allow")]
        allowed_operations: Vec<String>,

        /// Preset operation ceiling. `reviewer` = read-only + Pull;
        /// `contributor` = reviewer + Push/UpdateRef + context/discussion
        /// writes; `ci-landing` = reviewer + Push/UpdateRef for ready/land.
        /// A combined `--allow` may only narrow the template.
        #[arg(long, value_enum)]
        template: Option<AgentTemplateArg>,

        /// Write a single self-verifying `<name>.hcred` credential file to this
        /// path instead of installing the child into the keystore.
        #[arg(long, value_name = "HCRED_PATH")]
        out: Option<std::path::PathBuf>,
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
        /// Write the `.hcred` credential file to this path
        /// (default: ~/.heddle/service-accounts/<name>.hcred)
        #[arg(long, value_name = "HCRED_PATH")]
        out: Option<std::path::PathBuf>,
    },
}

impl From<AuthCommands> for heddle_client::AuthCommand {
    fn from(command: AuthCommands) -> Self {
        match command {
            AuthCommands::Login {
                server,
                open_browser,
                credential,
            } => heddle_client::AuthCommand::Login {
                server,
                open_browser,
                credential,
            },
            AuthCommands::Logout { server } => heddle_client::AuthCommand::Logout { server },
            AuthCommands::Status { server } => heddle_client::AuthCommand::Status { server },
            AuthCommands::DeriveAgent {
                server,
                agent_id,
                ttl_secs,
                scopes,
                allowed_operations,
                template,
                out,
            } => heddle_client::AuthCommand::DeriveAgent {
                server,
                agent_id,
                ttl_secs,
                scopes,
                allowed_operations,
                template: template.map(Into::into),
                out,
            },
            AuthCommands::CreateServiceToken {
                name,
                namespace,
                server,
                out,
            } => heddle_client::AuthCommand::CreateServiceToken {
                name,
                namespace,
                server,
                out,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{AuthCommands, Cli, Commands};

    #[test]
    fn login_parses_credential_path() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "login",
            "--credential",
            "/run/secrets/agent.hcred",
        ])
        .expect("credential login flag parses");

        let Commands::Auth {
            command:
                AuthCommands::Login {
                    server,
                    credential,
                    open_browser,
                },
        } = cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(server, None, "server comes from the credential file");
        assert_eq!(
            credential.as_deref(),
            Some(std::path::Path::new("/run/secrets/agent.hcred"))
        );
        assert!(!open_browser);
    }

    #[test]
    fn login_credential_conflicts_with_browser_flags() {
        for conflicting in [
            vec!["--credential", "/run/secrets/agent.hcred", "--server", "grpc.heddle.sh"],
            vec!["--credential", "/run/secrets/agent.hcred", "--open-browser"],
        ] {
            let mut args = vec!["heddle", "auth", "login"];
            args.extend(conflicting);
            assert!(
                Cli::try_parse_from(args).is_err(),
                "--credential must not combine with the browser-login flags"
            );
        }
    }

    #[test]
    fn interactive_login_needs_no_flags() {
        Cli::try_parse_from(["heddle", "auth", "login"])
            .expect("interactive login may resolve the configured default server");
    }

    #[test]
    fn derive_agent_parses_repeatable_scopes_and_operation_narrowing() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "derive-agent",
            "--server",
            "grpc.heddle.test",
            "--ttl",
            "900",
            "--scope",
            "repo:acme/api",
            "--scope",
            "namespace:acme",
            "--allow",
            "Push",
            "--allow",
            "GetState",
        ])
        .expect("derive-agent flags parse");

        let Commands::Auth {
            command:
                AuthCommands::DeriveAgent {
                    server,
                    ttl_secs,
                    scopes,
                    allowed_operations,
                    ..
                },
        } = cli.command
        else {
            panic!("expected auth derive-agent");
        };
        assert_eq!(server, "grpc.heddle.test");
        assert_eq!(ttl_secs, 900);
        assert_eq!(scopes, ["repo:acme/api", "namespace:acme"]);
        assert_eq!(allowed_operations, ["Push", "GetState"]);

        assert!(
            Cli::try_parse_from([
                "heddle",
                "auth",
                "derive-agent",
                "--server",
                "grpc.heddle.test",
                "--stdout",
            ])
            .is_err(),
            "token-only child export is unsafe because it cannot carry its proof key"
        );
    }
}
