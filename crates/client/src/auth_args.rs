//! CLI argument definitions for `heddle auth` commands.

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
    },
}
