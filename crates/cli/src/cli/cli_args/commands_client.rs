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
        /// Heddle server address (required for headless credential install).
        #[arg(long)]
        server: Option<String>,

        /// Open the authorization URL in the system browser.
        #[arg(long, conflicts_with = "token")]
        open_browser: bool,

        /// Install an existing Biscuit credential without opening a browser.
        #[arg(long, requires_all = ["key_file", "server"])]
        token: Option<String>,

        /// Device private-key PEM matching the token's proof key.
        #[arg(long, value_name = "PEM_PATH", requires = "token")]
        key_file: Option<std::path::PathBuf>,
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

        /// Narrow the safe operation set (repeatable, using hosted method names such as `Push`).
        #[arg(long = "allow")]
        allowed_operations: Vec<String>,

        /// Preset operation ceiling. `reviewer` = read-only + Pull;
        /// `contributor` = reviewer + Push/UpdateRef + context/discussion
        /// writes; `ci-landing` = reviewer + Push/UpdateRef for ready/land.
        /// A combined `--allow` may only narrow the template.
        #[arg(long, value_enum)]
        template: Option<AgentTemplateArg>,

        /// Write `token`, child `device-key.pem`, and `metadata.json` files to this directory.
        #[arg(long, value_name = "DIR")]
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
        /// Write the private-key PEM to this path (default: under ~/.heddle/service-accounts/)
        #[arg(long)]
        key_out: Option<String>,
        /// Include the private key PEM in stdout / JSON (default: write file only)
        #[arg(long)]
        show_secrets: bool,
    },
}

impl From<AuthCommands> for heddle_client::AuthCommand {
    fn from(command: AuthCommands) -> Self {
        match command {
            AuthCommands::Login {
                server,
                open_browser,
                token,
                key_file,
            } => heddle_client::AuthCommand::Login {
                server,
                open_browser,
                token,
                key_file,
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
                key_out,
                show_secrets,
            } => heddle_client::AuthCommand::CreateServiceToken {
                name,
                namespace,
                server,
                key_out,
                show_secrets,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{AuthCommands, Cli, Commands};

    #[test]
    fn login_parses_headless_token_and_key_file() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "login",
            "--server",
            "127.0.0.1:8421",
            "--token",
            "biscuit-token",
            "--key-file",
            "/run/secrets/device.pem",
        ])
        .expect("headless login flags parse");

        let Commands::Auth {
            command:
                AuthCommands::Login {
                    server,
                    token,
                    key_file,
                    open_browser,
                },
        } = cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(server.as_deref(), Some("127.0.0.1:8421"));
        assert_eq!(token.as_deref(), Some("biscuit-token"));
        assert_eq!(
            key_file.as_deref(),
            Some(std::path::Path::new("/run/secrets/device.pem"))
        );
        assert!(!open_browser);
    }

    #[test]
    fn login_requires_token_and_key_file_together() {
        for incomplete in [
            vec!["--token", "biscuit-token"],
            vec!["--key-file", "/run/secrets/device.pem"],
        ] {
            let mut args = vec!["heddle", "auth", "login"];
            args.extend(incomplete);
            assert!(
                Cli::try_parse_from(args).is_err(),
                "incomplete headless credential must be rejected"
            );
        }
    }

    #[test]
    fn headless_login_requires_an_explicit_server() {
        assert!(
            Cli::try_parse_from([
                "heddle",
                "auth",
                "login",
                "--token",
                "biscuit-token",
                "--key-file",
                "/run/secrets/device.pem",
            ])
            .is_err()
        );
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
            "api.heddle.test",
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
        assert_eq!(server, "api.heddle.test");
        assert_eq!(ttl_secs, 900);
        assert_eq!(scopes, ["repo:acme/api", "namespace:acme"]);
        assert_eq!(allowed_operations, ["Push", "GetState"]);

        assert!(
            Cli::try_parse_from([
                "heddle",
                "auth",
                "derive-agent",
                "--server",
                "api.heddle.test",
                "--stdout",
            ])
            .is_err(),
            "token-only child export is unsafe because it cannot carry its proof key"
        );
    }
}
