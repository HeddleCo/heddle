// SPDX-License-Identifier: Apache-2.0
//! `heddle spool …` — hosted spool child-edge management + facet-history
//! inspection (Spool epic P9, weft#358).
//!
//! Thin wrappers over the `HostedUserService` Spool RPCs. Each subcommand
//! resolves the hosted server from a named remote (mirroring `heddle thread
//! approve/…`) and renders the raw proto reply as text. The monorepo *clone*
//! lives in `clone.rs`; this file is the edge/history control surface.
//!
//! These are text-output management verbs (like `heddle presence`/`support`);
//! they do not advertise a JSON schema.

#![cfg(feature = "client")]

use anyhow::{Result, anyhow};
use grpc::heddle::v1::ChildEdgeStatus;
use repo::Repository;

use super::RecoveryAdvice;
use crate::{
    cli::{
        Cli, SpoolAttachArgs, SpoolChildrenArgs, SpoolCommands, SpoolDetachArgs, SpoolHistoryArgs,
    },
    client::{HostedAuthMode, HostedGrpcClient},
    config::UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};

/// Dispatch `heddle spool <subcommand>`.
pub async fn cmd_spool(cli: &Cli, command: SpoolCommands) -> Result<()> {
    match command {
        SpoolCommands::Attach(args) => cmd_spool_attach(cli, args).await,
        SpoolCommands::Detach(args) => cmd_spool_detach(cli, args).await,
        SpoolCommands::Children(args) => cmd_spool_children(cli, args).await,
        SpoolCommands::Governance(args) => cmd_spool_governance(cli, args).await,
        SpoolCommands::Membership(args) => cmd_spool_membership(cli, args).await,
    }
}

/// Resolve the named remote to a hosted client. Spool edges are a hosted-server
/// concept, so a local remote is a hard error (mirrors thread approvals).
async fn open_hosted_client(repo: &Repository, remote_name: &str) -> Result<HostedGrpcClient> {
    let (target, server_key) = resolve_remote_with_key(repo, Some(remote_name))?;
    let addr = match target {
        RemoteTarget::Network { addr, .. } => addr,
        RemoteTarget::Local(_) => {
            return Err(anyhow!(RecoveryAdvice::safety_refusal(
                "hosted_remote_required",
                format!("spool operations require a hosted remote; remote '{remote_name}' is local"),
                "Configure a hosted remote or retry against one that resolves to a network target.",
                format!("remote '{remote_name}' is local, but spool child edges live on the hosted server"),
                "running locally would imply a hosted spool change that no server recorded",
                "no hosted request was sent and local repository state was left unchanged",
                "heddle remote list",
                vec!["heddle remote list".to_string()],
            )));
        }
    };

    let user_config = UserConfig::load_default()?;
    // Attach/detach are pop-tier mutations, so use CredentialFallback (resolves
    // the credential store's proof key) rather than a token-only session, and
    // register the terminal human-signature callback for any human-tier
    // escalation.
    let client = HostedGrpcClient::open_session(
        addr,
        &user_config,
        server_key,
        HostedAuthMode::CredentialFallback,
    )
    .await?
    .with_human_signature_callback(crate::client::cli_human_signature_callback());
    Ok(client)
}

/// Default mount name = the child path's last `/`-segment.
fn default_mount_name(child_path: &str) -> &str {
    child_path
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(child_path)
}

fn change_id_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    objects::object::ChangeId::try_from_slice(bytes)
        .map(|id| id.to_string_full())
        .unwrap_or_default()
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn edge_status_label(status: i32) -> &'static str {
    match ChildEdgeStatus::try_from(status).unwrap_or(ChildEdgeStatus::Unspecified) {
        ChildEdgeStatus::UpToDate => "up-to-date",
        ChildEdgeStatus::FastForwardable => "fast-forwardable",
        ChildEdgeStatus::Diverged => "diverged",
        ChildEdgeStatus::NoChildHead => "no-child-head",
        ChildEdgeStatus::Unspecified => "unspecified",
    }
}

async fn cmd_spool_attach(cli: &Cli, args: SpoolAttachArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mount = args
        .mount_name
        .clone()
        .unwrap_or_else(|| default_mount_name(&args.child).to_string());
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let edge = client
        .attach_child(&args.parent, &args.child, &mount)
        .await?;

    println!(
        "Attached {child} under {parent} at '{mount}'",
        child = args.child,
        parent = args.parent,
    );
    let anchored = change_id_string(&edge.anchored_state_id);
    if !anchored.is_empty() {
        println!("  anchored state: {}", short(&anchored));
    }
    println!("  status:         {}", edge_status_label(edge.status));
    Ok(())
}

async fn cmd_spool_detach(cli: &Cli, args: SpoolDetachArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let removed = client.detach_child(&args.parent, &args.mount_name).await?;

    if removed {
        println!("Detached '{}' from {}", args.mount_name, args.parent);
    } else {
        println!(
            "No child mounted at '{}' under {} (nothing to detach).",
            args.mount_name, args.parent
        );
    }
    Ok(())
}

async fn cmd_spool_children(cli: &Cli, args: SpoolChildrenArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let children = client.list_children(&args.parent).await?;

    if children.is_empty() {
        println!("{} has no child spools.", args.parent);
    } else {
        println!("{} child spool(s) of {}:", children.len(), args.parent);
        for edge in &children {
            let anchored = change_id_string(&edge.anchored_state_id);
            println!(
                "  {mount:<20} {child}  [{status}] @ {state}",
                mount = edge.mount_name,
                child = edge.child_spool_id,
                status = edge_status_label(edge.status),
                state = short(&anchored),
            );
        }
    }
    Ok(())
}

async fn cmd_spool_governance(cli: &Cli, args: SpoolHistoryArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let entries = client
        .get_governance_history(&args.spool, args.limit)
        .await?;

    if entries.is_empty() {
        println!("No governance history for {}.", args.spool);
    } else {
        println!(
            "{} governance entr{} for {} (newest first):",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            args.spool
        );
        for entry in &entries {
            let cid = change_id_string(&entry.change_id);
            println!(
                "  {cid}  visibility={vis}  {when}",
                cid = short(&cid),
                vis = entry.visibility,
                when = ts_label(&entry.committed_at),
            );
        }
    }
    Ok(())
}

async fn cmd_spool_membership(cli: &Cli, args: SpoolHistoryArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let entries = client
        .get_membership_history(&args.spool, args.limit)
        .await?;

    if entries.is_empty() {
        println!("No membership history for {}.", args.spool);
    } else {
        println!(
            "{} membership entr{} for {} (newest first):",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            args.spool
        );
        for entry in &entries {
            let cid = change_id_string(&entry.change_id);
            println!(
                "  {cid}  {n} grant(s)  {when}",
                cid = short(&cid),
                n = entry.grants.len(),
                when = ts_label(&entry.committed_at),
            );
            for grant in &entry.grants {
                println!("    {} = {}", grant.subject, grant.role);
            }
        }
    }
    Ok(())
}

fn ts_secs(ts: &Option<prost_types::Timestamp>) -> u64 {
    ts.as_ref().map(|t| t.seconds.max(0) as u64).unwrap_or(0)
}

fn ts_label(ts: &Option<prost_types::Timestamp>) -> String {
    let secs = ts_secs(ts);
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mount_name_uses_last_path_segment() {
        assert_eq!(default_mount_name("acme/lib"), "lib");
        assert_eq!(default_mount_name("acme/team/lib"), "lib");
        assert_eq!(default_mount_name("acme/lib/"), "lib");
        assert_eq!(default_mount_name("solo"), "solo");
    }

    #[test]
    fn edge_status_label_covers_every_variant() {
        assert_eq!(
            edge_status_label(ChildEdgeStatus::UpToDate as i32),
            "up-to-date"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatus::FastForwardable as i32),
            "fast-forwardable"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatus::Diverged as i32),
            "diverged"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatus::NoChildHead as i32),
            "no-child-head"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatus::Unspecified as i32),
            "unspecified"
        );
    }
}
