// SPDX-License-Identifier: Apache-2.0
//! Repository import command definitions.

use clap::{Args, Subcommand};

use super::ImportLocalArgs;

#[derive(Args, Clone, Debug)]
#[command(verbatim_doc_comment)]
#[command(about = "Bring an existing Git repository into Heddle.")]
#[command(long_about = "Bring an existing Git repository into Heddle.")]
#[command(after_help = "\
Choose a source:
  local  Import a Git checkout on this computer.
  url    Have the server import a public HTTPS Git URL.

Already checked out the repository?
  heddle import local .
  heddle push <destination>

Want the server to fetch it, without a local checkout?
  heddle import url https://github.com/acme/widget.git --to <handle>/widget

Use \"heddle init\" to start a repository or keep working in Git mode.
Use \"heddle clone\" to download an existing hosted Heddle repository.
")]
pub struct ImportArgs {
    #[command(subcommand)]
    pub command: ImportCommands,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ImportCommands {
    /// Import a Git checkout on this computer.
    Local(ImportLocalArgs),

    /// Have the server import a public HTTPS Git URL.
    #[cfg(feature = "client")]
    Url(ImportUrlArgs),

    /// Watch a durable import operation until it reaches a terminal state.
    ///
    /// With `--output json`, emits JSONL: one `import_operation` document per
    /// committed version, each carrying `operation_id` and `terminal`.
    #[cfg(feature = "client")]
    Status(ImportOperationArgs),

    /// Retry a failed durable import operation.
    #[cfg(feature = "client")]
    Retry(ImportOperationArgs),
}

#[cfg(feature = "client")]
#[derive(Args, Clone, Debug)]
pub struct ImportUrlArgs {
    /// Public HTTPS Git URL for the server to fetch.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Hosted destination spool path or URL.
    #[arg(long, value_name = "SPOOL|URL")]
    pub to: String,

    /// Name of the imported native thread (default: main).
    #[arg(long, value_name = "NAME")]
    pub thread: Option<String>,

    /// Heddle server address when --to is a spool path.
    #[arg(long, value_name = "HOST")]
    pub server: Option<String>,
}

#[cfg(feature = "client")]
#[derive(Args, Clone, Debug)]
pub struct ImportOperationArgs {
    /// Durable operation-record ID returned by import URL or retry.
    #[arg(value_name = "OPERATION")]
    pub operation: String,

    /// Hosted destination spool path or URL.
    #[arg(long, value_name = "SPOOL|URL")]
    pub to: String,

    /// Heddle server address when --to is a spool path.
    #[arg(long, value_name = "HOST")]
    pub server: Option<String>,
}
