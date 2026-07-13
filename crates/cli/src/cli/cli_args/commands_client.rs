// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::Subcommand;

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
            AuthCommands::Login { server, no_browser } => {
                heddle_client::AuthCommand::Login { server, no_browser }
            }
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
