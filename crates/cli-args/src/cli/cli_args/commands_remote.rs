// SPDX-License-Identifier: Apache-2.0
//! Remote command definitions.

#[cfg(feature = "client")]
use clap::Args;
use clap::Subcommand;

#[cfg(feature = "client")]
#[derive(Args, Clone, Debug)]
#[command(after_help = "\
The hosted weft executor fetches the Git repository and imports it into a
native Heddle thread. This is the fast server-side path; `heddle adopt` imports
an already-local Git checkout instead.

Examples:
  heddle remote import-source https://github.com/octocat/Hello-World.git --to willow-ibis-8e7264/hello
  heddle remote import-source https://github.com/octocat/Hello-World.git --to https://api.heddle.sh/willow-ibis-8e7264/hello --name main
")]
pub struct ImportSourceArgs {
    /// Public HTTPS Git clone URL for weft to fetch.
    #[arg(value_name = "CLONE_URL")]
    pub clone_url: String,

    /// Hosted destination spool path or URL.
    #[arg(long, value_name = "SPOOL_PATH")]
    pub to: String,

    /// Name of the imported native thread (default: main).
    #[arg(long, value_name = "THREAD")]
    pub name: Option<String>,

    /// Heddle server address when --to is a spool path.
    #[arg(long)]
    pub server: Option<String>,

    /// Allow cleartext transport to a non-loopback test server.
    #[arg(long)]
    pub insecure: bool,
}

#[derive(Subcommand, Clone)]
pub enum RemoteCommands {
    /// List configured remotes.
    List,

    /// Add a remote.
    Add {
        /// Remote name.
        name: String,
        /// Hosted Heddle endpoint or local native Heddle repository path.
        url: String,
    },

    /// Remove a remote.
    Remove {
        /// Remote name.
        name: String,
    },

    /// Set the default Heddle remote for pull and push.
    SetDefault {
        /// Existing remote name.
        name: String,
    },

    /// Show remote details.
    Show {
        /// Remote name.
        name: String,
    },

    /// Import a public Git repository into a hosted spool via weft.
    #[cfg(feature = "client")]
    ImportSource(ImportSourceArgs),
}
