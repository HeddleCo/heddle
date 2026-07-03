// SPDX-License-Identifier: Apache-2.0
//! `heddle spool` command arguments (Spool epic P9, weft#358).
//!
//! Child-edge management + facet-history inspection over a hosted spool. Thin
//! wrappers over the `HostedUserService` Spool RPCs. Each subcommand resolves
//! the hosted server from a named remote (default `origin`), mirroring the
//! `heddle thread approve/…` hosted surface.

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Subcommand)]
pub enum SpoolCommands {
    /// Attach a child spool under a parent at a mount point.
    ///
    /// The child is anchored at its current content head. Only the parent's
    /// children edge-set changes — never the parent's content.
    Attach(SpoolAttachArgs),

    /// Detach the child mounted at a mount point under a parent.
    Detach(SpoolDetachArgs),

    /// List the child edges of a spool, with each edge's anchored state.
    Children(SpoolChildrenArgs),

    /// Show a spool's governance-facet history (newest first).
    Governance(SpoolHistoryArgs),

    /// Show a spool's membership-facet history (newest first).
    Membership(SpoolHistoryArgs),
}

#[derive(Clone, Debug, Args)]
pub struct SpoolAttachArgs {
    /// Parent spool path (the monorepo container).
    pub parent: String,
    /// Child spool path to attach.
    pub child: String,
    /// Mount name for the child inside the parent (the SPOOLLINK entry name).
    /// Defaults to the child path's last segment.
    #[arg(long = "as", value_name = "MOUNT")]
    pub mount_name: Option<String>,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct SpoolDetachArgs {
    /// Parent spool path.
    pub parent: String,
    /// Mount name of the child to detach.
    pub mount_name: String,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct SpoolChildrenArgs {
    /// Parent spool path whose children to list.
    pub parent: String,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

#[derive(Clone, Debug, Args)]
pub struct SpoolHistoryArgs {
    /// Spool path whose facet history to walk.
    pub spool: String,
    /// Max entries to walk (server default when unset).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Remote that maps to the hosted server (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}
