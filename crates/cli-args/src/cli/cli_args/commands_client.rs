// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::{Args, Subcommand, ValueEnum};

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

    /// Inspect or explicitly replace descriptor-signing trust
    Trust {
        #[command(subcommand)]
        command: AuthTrustCommands,
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

#[derive(Subcommand, Clone, Debug)]
pub enum AuthTrustCommands {
    /// Show the descriptor trust controlling a server connection
    Show(AuthTrustShowArgs),
    /// Atomically replace an automatic descriptor trust pin
    Replace(AuthTrustReplaceArgs),
}

#[derive(Args, Clone, Debug)]
pub struct AuthTrustShowArgs {
    /// Heddle server authority
    #[arg(long)]
    pub server: String,
}

#[derive(Args, Clone, Debug)]
pub struct AuthTrustReplaceArgs {
    /// Heddle server authority
    #[arg(long)]
    pub server: String,
    /// Current descriptor public key required for compare-and-swap
    #[arg(long, value_name = "64_HEX")]
    pub expect_current_public_key: String,
    /// New descriptor key id confirmed out of band
    #[arg(long)]
    pub key_id: String,
    /// New descriptor public key confirmed out of band
    #[arg(long, value_name = "64_HEX")]
    pub public_key: String,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{AuthCommands, AuthTrustCommands, Cli, Commands};

    #[test]
    fn trust_replace_parses_compare_and_swap_inputs() {
        let old_key = "11".repeat(32);
        let new_key = "22".repeat(32);
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "trust",
            "replace",
            "--server",
            "api.example",
            "--expect-current-public-key",
            &old_key,
            "--key-id",
            "next-key",
            "--public-key",
            &new_key,
        ])
        .expect("trust replacement parses");

        let Commands::Auth {
            command:
                AuthCommands::Trust {
                    command: AuthTrustCommands::Replace(args),
                },
        } = cli.command
        else {
            panic!("expected auth trust replace");
        };
        assert_eq!(args.server, "api.example");
        assert_eq!(args.expect_current_public_key, old_key);
        assert_eq!(args.key_id, "next-key");
        assert_eq!(args.public_key, new_key);
    }

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
            vec![
                "--credential",
                "/run/secrets/agent.hcred",
                "--server",
                "api.heddle.sh",
            ],
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
