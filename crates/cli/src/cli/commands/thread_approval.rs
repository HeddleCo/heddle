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
use api::heddle::api::v1alpha1::{RepositoryRef, StateId as ApiStateId, repository_ref::Reference};
use heddle_cli_args::CliContext as _;
use objects::object::ThreadName;
use repo::Repository;
use verbs::approval_plan::{
    EligibilitySummary, approval_recorded_message, approval_revoked_message,
    approvals_empty_message, approvals_header, eligibility_allowed_message,
    eligibility_approvals_counted_message, eligibility_blocked_message, format_unix_secs_display,
    format_unix_secs_label, plan_eligibility_summary, short_state_id, state_id_bytes_to_string,
    timestamp_secs_u64, unmet_requirement_line,
};

use super::RecoveryAdvice;
use super::next_action::{NextActionValidationContext, write_full_command_json};
use crate::{
    cli::{
        Cli,
        cli_args::{
            ThreadApprovalsArgs, ThreadApproveArgs, ThreadCheckMergeArgs, ThreadRevokeApprovalArgs,
        },
        should_output_json,
    },
    config::UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};
use hosted_client::client::{HostedAuthMode, HostedClient};

// The approval wire payloads live in cli-contract so the schema registry
// registers the real serialization types.
pub(crate) use heddle_cli_contract::cli::commands::wire::thread::{
    ApprovalOutput, ApprovalRevokeOutput, EligibilityOutput, UnmetOutput,
};

fn ts_secs(ts: &Option<prost_types::Timestamp>) -> u64 {
    timestamp_secs_u64(ts.as_ref().map(|t| t.seconds))
}

fn repository_ref_string(repository: Option<RepositoryRef>) -> String {
    match repository.and_then(|repository| repository.reference) {
        Some(Reference::HostedId(id) | Reference::CanonicalPath(id)) => id,
        None => String::new(),
    }
}

fn api_state_id_string(state_id: &Option<ApiStateId>) -> String {
    state_id
        .as_ref()
        .map(|state_id| state_id_bytes_to_string(&state_id.value))
        .unwrap_or_default()
}

/// Resolve the named remote and its repo_path. Errors if the remote
/// is local (approvals are a hosted-server concept) or has no path.
async fn open_hosted_session(
    repo: &Repository,
    remote_name: &str,
) -> Result<(HostedClient, String)> {
    let (target, server_key) = resolve_remote_with_key(repo, Some(remote_name))?;
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
    // Authenticated thread-workflow RPCs are proof-of-possession gated, so use
    // CredentialFallback (resolves the credential store's proof key) rather
    // than a token-only ConfigToken session.
    let client = HostedClient::open_session(
        addr,
        &user_config,
        server_key,
        HostedAuthMode::CredentialFallback,
    )
    .await?
    .with_human_signature_callback(hosted_client::client::cli_human_signature_callback());
    Ok((client, repo_path))
}

/// Read a thread's head state. The head is what the gate pins
/// approvals against — push a new state and `stale_on_update`
/// will invalidate the prior approval.
fn thread_head_state(repo: &Repository, thread: &str) -> Result<String> {
    repo.refs()
        .get_thread(&ThreadName::new(thread))?
        .map(|state_id| state_id.to_string())
        .ok_or_else(|| anyhow!("thread '{thread}' has no head state"))
}

pub async fn cmd_thread_approve(cli: &Cli, args: ThreadApproveArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let source_state = thread_head_state(&repo, &args.source)?;
    let (mut client, repo_path) = open_hosted_session(&repo, &args.remote).await?;
    let approval = client
        .approve_thread(
            &repo_path,
            &args.source,
            &args.target,
            &source_state,
            args.note.as_deref(),
            cli.operation_id_wire(),
        )
        .await;
    client.close().await;
    let approval = approval?;

    if should_output_json(cli, Some(repo.config())) {
        let out = ApprovalOutput {
            id: approval.id,
            repo_path: repository_ref_string(approval.repo_path),
            source_thread: approval.source_thread,
            target_thread: approval.target_thread,
            source_state: api_state_id_string(&approval.source_state),
            approver_user_id: approval.approver_user_id,
            note: approval.note,
            approved_at: ts_secs(&approval.approved_at),
            expires_at: ts_secs(&approval.expires_at),
        };
        write_full_command_json(
            &out,
            NextActionValidationContext::without_repo(&["thread", "approve"]),
        )?;
    } else {
        println!(
            "{}",
            approval_recorded_message(&args.source, &args.target, &source_state)
        );
        println!("  approval id: {}", approval.id);
        let exp_secs = ts_secs(&approval.expires_at);
        if exp_secs > 0 {
            println!("  expires at:  {}", format_unix_secs_label(exp_secs));
        }
        if !approval.note.is_empty() {
            println!("  note:        {}", approval.note);
        }
    }
    Ok(())
}

pub async fn cmd_thread_approvals(cli: &Cli, args: ThreadApprovalsArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let (mut client, repo_path) = open_hosted_session(&repo, &args.remote).await?;
    let approvals = client
        .list_thread_approvals(&repo_path, &args.source, &args.target)
        .await;
    client.close().await;
    let approvals = approvals?;

    if should_output_json(cli, Some(repo.config())) {
        let out: Vec<ApprovalOutput> = approvals
            .into_iter()
            .map(|a| ApprovalOutput {
                id: a.id,
                repo_path: repository_ref_string(a.repo_path),
                source_thread: a.source_thread,
                target_thread: a.target_thread,
                source_state: api_state_id_string(&a.source_state),
                approver_user_id: a.approver_user_id,
                note: a.note,
                approved_at: ts_secs(&a.approved_at),
                expires_at: ts_secs(&a.expires_at),
            })
            .collect();
        write_full_command_json(
            &out,
            NextActionValidationContext::without_repo(&["thread", "approvals"]),
        )?;
    } else if approvals.is_empty() {
        println!("{}", approvals_empty_message(&args.source, &args.target));
    } else {
        println!(
            "{}",
            approvals_header(approvals.len(), &args.source, &args.target)
        );
        for a in approvals {
            let approved_secs = ts_secs(&a.approved_at);
            let when = format_unix_secs_display(approved_secs);
            let state_str = api_state_id_string(&a.source_state);
            print!(
                "  {id}  approver={user}  state={state}  approved_at={when}",
                id = a.id,
                user = a.approver_user_id,
                state = short_state_id(&state_str),
            );
            let exp_secs = ts_secs(&a.expires_at);
            if exp_secs > 0 {
                print!("  expires_at={}", format_unix_secs_display(exp_secs));
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
    let repo = cli.open_repo()?;
    let (mut client, _repo_path) = open_hosted_session(&repo, &args.remote).await?;
    let result = client
        .revoke_approval(&args.id, cli.operation_id_wire())
        .await;
    client.close().await;
    result?;
    if should_output_json(cli, Some(repo.config())) {
        let output = ApprovalRevokeOutput {
            output_kind: "thread_revoke_approval",
            id: args.id,
            deleted: true,
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["thread", "revoke-approval"]),
        )?;
    } else {
        println!("{}", approval_revoked_message(&args.id));
    }
    Ok(())
}

pub async fn cmd_thread_check_merge(cli: &Cli, args: ThreadCheckMergeArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let source_state = thread_head_state(&repo, &args.source)?;
    let (mut client, repo_path) = open_hosted_session(&repo, &args.remote).await?;
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
        .await;
    client.close().await;
    let resp = resp?;

    let unmet: Vec<UnmetOutput> = resp
        .unmet
        .into_iter()
        .map(|u| UnmetOutput {
            policy_id: u.policy_id,
            kind: match api::heddle::api::v1alpha1::UnmetRequirementKind::try_from(u.kind)
                .unwrap_or_default()
            {
                api::heddle::api::v1alpha1::UnmetRequirementKind::FlatRole => "flat_role",
                api::heddle::api::v1alpha1::UnmetRequirementKind::Group => "group",
                api::heddle::api::v1alpha1::UnmetRequirementKind::OpenDiscussion => {
                    "open_discussion"
                }
                api::heddle::api::v1alpha1::UnmetRequirementKind::Unspecified => "unspecified",
            }
            .to_string(),
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
            repo_path: repository_ref_string(a.repo_path),
            source_thread: a.source_thread,
            target_thread: a.target_thread,
            source_state: api_state_id_string(&a.source_state),
            approver_user_id: a.approver_user_id,
            note: a.note,
            approved_at: ts_secs(&a.approved_at),
            expires_at: ts_secs(&a.expires_at),
        })
        .collect();

    let allowed = resp.allowed;
    if should_output_json(cli, Some(repo.config())) {
        let out = EligibilityOutput {
            allowed,
            unmet,
            valid_approvals,
        };
        write_full_command_json(
            &out,
            NextActionValidationContext::without_repo(&["thread", "check-merge"]),
        )?;
    } else {
        match plan_eligibility_summary(allowed, valid_approvals.len(), unmet.len()) {
            EligibilitySummary::Allowed { approval_count } => {
                println!(
                    "{}",
                    eligibility_allowed_message(&args.source, &args.target)
                );
                if approval_count > 0 {
                    println!("{}", eligibility_approvals_counted_message(approval_count));
                }
            }
            EligibilitySummary::Blocked { unmet_count } => {
                println!(
                    "{}",
                    eligibility_blocked_message(&args.source, &args.target, unmet_count)
                );
                for u in &unmet {
                    println!(
                        "{}",
                        unmet_requirement_line(&u.kind, &u.reason, u.have, u.needed)
                    );
                }
            }
        }
    }
    // Non-zero exit so scripts can branch. Use DataErr (65) — exit 2 is
    // reserved for panic / set -e fallout and must not be intentional.
    // Report already rendered; main maps OutcomeExit without a second envelope.
    if !allowed {
        return Err(anyhow!(crate::exit::OutcomeExit::data_err()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn workflow_mutations_forward_the_cli_operation_id() {
        let source = include_str!("thread_approval.rs");
        let implementation = source
            .split("#[cfg(test)]")
            .next()
            .expect("implementation section");
        assert_eq!(
            implementation.matches("cli.operation_id_wire()").count(),
            2,
            "approve and revoke must both forward the caller's --op-id"
        );
    }
}
