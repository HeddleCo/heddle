// SPDX-License-Identifier: Apache-2.0
//! `heddle thread approve` / `approvals` / `revoke-approval` /
//! `check-merge` — record and inspect merge approvals against the
//! hosted server's policies.
//!
//! Each subcommand:
//! 1. Opens the local repo to read the source thread's current state.
//! 2. Resolves the named remote to a hosted address + repo_path.
//! 3. Calls the corresponding RPC and renders the result.
//!
//! These commands are server-only operations, but the source-state
//! lookup happens locally — that's how the gate distinguishes a
//! fresh approval from a stale one across pushes.

#![cfg(feature = "client")]

use anyhow::{Context, Result, anyhow};
use objects::object::ThreadName;
use repo::Repository;
use serde::Serialize;

use super::RecoveryAdvice;
use crate::{
    cli::{
        Cli,
        cli_args::{
            ThreadApprovalsArgs, ThreadApproveArgs, ThreadCheckMergeArgs, ThreadRevokeApprovalArgs,
        },
        should_output_json,
    },
    client::{HostedAuthMode, HostedGrpcClient},
    config::UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};

#[derive(Serialize)]
struct ApprovalOutput {
    id: String,
    repo_path: String,
    source_thread: String,
    target_thread: String,
    source_state: String,
    approver_user_id: String,
    note: String,
    approved_at: u64,
    expires_at: u64,
}

fn ts_secs(ts: &Option<prost_types::Timestamp>) -> u64 {
    ts.as_ref().map(|t| t.seconds.max(0) as u64).unwrap_or(0)
}

fn bytes_to_change_id_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    objects::object::ChangeId::try_from_slice(bytes)
        .map(|id| id.to_string_full())
        .unwrap_or_default()
}

#[derive(Serialize)]
struct UnmetOutput {
    policy_id: String,
    kind: String,
    group_id: String,
    reason: String,
    needed: u32,
    have: u32,
}

#[derive(Serialize)]
struct EligibilityOutput {
    allowed: bool,
    unmet: Vec<UnmetOutput>,
    valid_approvals: Vec<ApprovalOutput>,
}

#[derive(Serialize)]
struct ApprovalRevokeOutput {
    output_kind: &'static str,
    id: String,
    deleted: bool,
}

/// Resolve the named remote and its repo_path. Errors if the remote
/// is local (approvals are a hosted-server concept) or has no path.
async fn open_heddle_client(
    repo: &Repository,
    remote_name: &str,
) -> Result<(HostedGrpcClient, String)> {
    let (target, server_key) =
        resolve_remote_with_key(repo, Some(remote_name)).map_err(anyhow::Error::msg)?;
    let (addr, repo_path) = match target {
        RemoteTarget::Network { addr, repo_path } => (
            addr,
            repo_path.context("hosted remote must include a repository path")?,
        ),
        RemoteTarget::Local(_) => {
            return Err(anyhow!(RecoveryAdvice::safety_refusal(
                "hosted_remote_required",
                format!("approvals require a hosted remote; remote '{remote_name}' is local"),
                "Configure a hosted remote or retry against one that resolves to a network target.",
                format!("remote '{remote_name}' is local, but approvals run on the hosted server"),
                "running locally would imply a hosted approval policy change that no server recorded",
                "no hosted request was sent and local repository state was left unchanged",
                "heddle remote list",
                vec!["heddle remote list".to_string()],
            )));
        }
    };

    let user_config = UserConfig::load_default()?;
    let client =
        HostedGrpcClient::open_session(addr, &user_config, server_key, HostedAuthMode::ConfigToken)
            .await?;
    Ok((client, repo_path))
}

/// Read a thread's head state. The head is what the gate pins
/// approvals against — push a new state and `stale_on_update`
/// will invalidate the prior approval.
fn thread_head_state(repo: &Repository, thread: &str) -> Result<String> {
    repo.refs()
        .get_thread(&ThreadName::new(thread))?
        .map(|change_id| change_id.to_string())
        .ok_or_else(|| anyhow!("thread '{thread}' has no head state"))
}

pub async fn cmd_thread_approve(cli: &Cli, args: ThreadApproveArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let source_state = thread_head_state(&repo, &args.source)?;
    let (mut client, repo_path) = open_heddle_client(&repo, &args.remote).await?;
    let approval = client
        .approve_thread(
            &repo_path,
            &args.source,
            &args.target,
            &source_state,
            args.note.as_deref(),
        )
        .await?;

    if should_output_json(cli, Some(repo.config())) {
        let out = ApprovalOutput {
            id: approval.id,
            repo_path: approval.repo_path,
            source_thread: approval.source_thread,
            target_thread: approval.target_thread,
            source_state: bytes_to_change_id_string(&approval.source_state),
            approver_user_id: approval.approver_user_id,
            note: approval.note,
            approved_at: ts_secs(&approval.approved_at),
            expires_at: ts_secs(&approval.expires_at),
        };
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!(
            "Approved {source} -> {target} at {state}",
            source = args.source,
            target = args.target,
            state = &source_state[..source_state.len().min(12)],
        );
        println!("  approval id: {}", approval.id);
        let exp_secs = ts_secs(&approval.expires_at);
        if exp_secs > 0
            && let Some(d) = chrono::DateTime::from_timestamp(exp_secs as i64, 0)
        {
            println!("  expires at:  {}", d.to_rfc3339());
        }
        if !approval.note.is_empty() {
            println!("  note:        {}", approval.note);
        }
    }
    Ok(())
}

pub async fn cmd_thread_approvals(cli: &Cli, args: ThreadApprovalsArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let (mut client, repo_path) = open_heddle_client(&repo, &args.remote).await?;
    let approvals = client
        .list_thread_approvals(&repo_path, &args.source, &args.target)
        .await?;

    if should_output_json(cli, Some(repo.config())) {
        let out: Vec<ApprovalOutput> = approvals
            .into_iter()
            .map(|a| ApprovalOutput {
                id: a.id,
                repo_path: a.repo_path,
                source_thread: a.source_thread,
                target_thread: a.target_thread,
                source_state: bytes_to_change_id_string(&a.source_state),
                approver_user_id: a.approver_user_id,
                note: a.note,
                approved_at: ts_secs(&a.approved_at),
                expires_at: ts_secs(&a.expires_at),
            })
            .collect();
        println!("{}", serde_json::to_string(&out)?);
    } else if approvals.is_empty() {
        println!(
            "No approvals recorded for {} -> {}.",
            args.source, args.target
        );
    } else {
        println!(
            "{} approval(s) for {} -> {}:",
            approvals.len(),
            args.source,
            args.target
        );
        for a in approvals {
            let approved_secs = ts_secs(&a.approved_at);
            let when = chrono::DateTime::from_timestamp(approved_secs as i64, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| approved_secs.to_string());
            let state_str = bytes_to_change_id_string(&a.source_state);
            print!(
                "  {id}  approver={user}  state={state}  approved_at={when}",
                id = a.id,
                user = a.approver_user_id,
                state = &state_str[..state_str.len().min(12)],
            );
            let exp_secs = ts_secs(&a.expires_at);
            if exp_secs > 0 {
                let exp = chrono::DateTime::from_timestamp(exp_secs as i64, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| exp_secs.to_string());
                print!("  expires_at={exp}");
            }
            if !a.note.is_empty() {
                print!("  note=\"{}\"", a.note);
            }
            println!();
        }
    }
    Ok(())
}

pub async fn cmd_thread_revoke_approval(cli: &Cli, args: ThreadRevokeApprovalArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let (mut client, _repo_path) = open_heddle_client(&repo, &args.remote).await?;
    client.revoke_approval(&args.id).await?;
    if should_output_json(cli, Some(repo.config())) {
        let output = ApprovalRevokeOutput {
            output_kind: "thread_revoke_approval",
            id: args.id,
            deleted: true,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Revoked approval {}.", args.id);
    }
    Ok(())
}

pub async fn cmd_thread_check_merge(cli: &Cli, args: ThreadCheckMergeArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let source_state = thread_head_state(&repo, &args.source)?;
    let (mut client, repo_path) = open_heddle_client(&repo, &args.remote).await?;
    let resp = client
        .check_merge_eligibility(
            &repo_path,
            &args.source,
            &args.target,
            &source_state,
            &args.gated_action,
            args.changed_paths,
            None,
        )
        .await?;

    let unmet: Vec<UnmetOutput> = resp
        .unmet
        .into_iter()
        .map(|u| UnmetOutput {
            policy_id: u.policy_id,
            kind: u.kind,
            group_id: u.group_id,
            reason: u.reason,
            needed: u.needed,
            have: u.have,
        })
        .collect();
    let valid_approvals: Vec<ApprovalOutput> = resp
        .valid_approvals
        .into_iter()
        .map(|a| ApprovalOutput {
            id: a.id,
            repo_path: a.repo_path,
            source_thread: a.source_thread,
            target_thread: a.target_thread,
            source_state: bytes_to_change_id_string(&a.source_state),
            approver_user_id: a.approver_user_id,
            note: a.note,
            approved_at: ts_secs(&a.approved_at),
            expires_at: ts_secs(&a.expires_at),
        })
        .collect();

    if should_output_json(cli, Some(repo.config())) {
        let out = EligibilityOutput {
            allowed: resp.allowed,
            unmet,
            valid_approvals,
        };
        println!("{}", serde_json::to_string(&out)?);
    } else if resp.allowed {
        println!("{} -> {} can merge.", args.source, args.target);
        if !valid_approvals.is_empty() {
            println!("  ({} approval(s) counted)", valid_approvals.len());
        }
    } else {
        println!(
            "{} -> {} BLOCKED by {} unmet requirement(s):",
            args.source,
            args.target,
            unmet.len()
        );
        for u in unmet {
            println!(
                "  [{kind}] {reason} (have {have}/{needed})",
                kind = u.kind,
                reason = u.reason,
                have = u.have,
                needed = u.needed,
            );
        }
        // Non-zero exit code so scripts can branch on it.
        std::process::exit(2);
    }
    Ok(())
}
