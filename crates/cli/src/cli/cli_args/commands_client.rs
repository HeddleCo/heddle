// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::Subcommand;

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
}
