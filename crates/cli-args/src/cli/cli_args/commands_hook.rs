// SPDX-License-Identifier: Apache-2.0
//! Hook command definitions.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Subcommand, Clone)]
pub enum HookCommands {
    /// List installed hooks.
    List,

    /// Install a hook.
    Install {
        /// Hook name (e.g., pre-snapshot, post-push).
        name: String,
        #[command(flatten)]
        source: HookInstallSource,
    },

    /// Uninstall a hook.
    Uninstall {
        /// Hook name.
        name: String,
    },

    /// Show the hook event catalog (W2/A15).
    ///
    /// Prints the JSON-Schema for each registered event's payload and
    /// response. Use this to scaffold a hook without reading source.
    Events {
        /// When set, returns the schema for one event only.
        #[arg(long)]
        event: Option<String>,
    },
}

#[derive(Args, Clone)]
#[group(required = false, multiple = false)]
pub struct HookInstallSource {
    /// Read the hook script from a file path.
    #[arg(long = "from-file", value_name = "PATH")]
    pub from_file: Option<PathBuf>,

    /// Read the hook script from standard input.
    #[arg(long = "from-stdin")]
    pub from_stdin: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands, HookCommands};

    #[test]
    fn parses_hook_install_from_file_without_positional_script_body() {
        let cli = Cli::try_parse_from([
            "heddle",
            "hook",
            "install",
            "pre-snapshot",
            "--from-file",
            "hooks/pre-snapshot.sh",
        ])
        .expect("parse hook install command");

        match cli.command {
            Commands::Hook {
                command: HookCommands::Install { name, source },
            } => {
                assert_eq!(name, "pre-snapshot");
                assert_eq!(
                    source.from_file.as_deref(),
                    Some(std::path::Path::new("hooks/pre-snapshot.sh"))
                );
                assert!(!source.from_stdin);
            }
            _ => panic!("unexpected command"),
        }
    }
}
