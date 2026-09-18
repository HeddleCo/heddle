// SPDX-License-Identifier: Apache-2.0
//! Signed decisions and readiness for one native Thread.
#![cfg(feature = "client")]

use anyhow::{Context, Result, anyhow, bail, ensure};
use api::heddle::api::v1alpha2 as wire;
use crypto::{Ed25519Signer, Signer};
use heddle_cli_args::CliContext as _;
pub(crate) use heddle_cli_contract::cli::commands::wire::thread::{
    ApprovalOutput, ApprovalRevokeOutput, EligibilityOutput, UnmetOutput,
};
use hosted_client::client::{HostedAuthMode, HostedClient, ReviewSnapshot};
use objects::object::{ContentHash, StateId, thread_replication::SourceAuthor};
use repo::Repository;
use thread_api::thread_control::{Author, Control, PreparedControl, Review, ReviewKind};
#[path = "review_outbox.rs"]
mod review_outbox;
use review_outbox::{ReviewOutbox, StoredReview};

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

async fn open_hosted_session(
    repo: &Repository,
    remote_name: &str,
) -> Result<(HostedClient, String)> {
    let (target, server_key) = resolve_remote_with_key(repo, Some(remote_name))?;
    let (authority, address) = match target {
        RemoteTarget::Network {
            authority,
            repo_path,
        } => (
            authority,
            repo_path.context("hosted remote must include a Spool address")?,
        ),
        RemoteTarget::Local(_) => {
            bail!("Thread review requires a hosted remote; choose one with `heddle remote list`")
        }
    };
    let config = UserConfig::load_default()?;
    let client = HostedClient::open_session(
        &authority,
        &config,
        server_key,
        HostedAuthMode::CredentialFallback,
    )
    .await?
    .with_human_signature_callback(hosted_client::client::headless_human_signature_callback());
    Ok((client, address))
}

fn revision_state(revision: &wire::RevisionRef) -> Result<StateId> {
    let Some(wire::revision_ref::Revision::State(state)) = &revision.revision else {
        bail!("review comparison requires an exact state revision")
    };
    let bytes: [u8; 32] = state
        .value
        .as_slice()
        .try_into()
        .context("review revision must contain 32 bytes")?;
    Ok(StateId::from_bytes(bytes))
}

struct ReviewComparison {
    source: StateId,
    base: StateId,
    policy: ContentHash,
}

fn comparison(snapshot: &ReviewSnapshot) -> Result<ReviewComparison> {
    let value = snapshot.comparison.as_ref().context(
        "current review comparison is unavailable; publish one accepted source head, then retry",
    )?;
    let thread = snapshot
        .overview
        .r#ref
        .as_ref()
        .context("Thread identity absent")?;
    let source = value.source.as_ref().context("comparison source absent")?;
    let base = value.base.as_ref().context("comparison base absent")?;
    ensure!(
        source.spool == thread.spool && base.spool == thread.spool,
        "review comparison belongs to another Spool"
    );
    let bytes: [u8; 32] = value
        .policy_version
        .as_slice()
        .try_into()
        .context("review policy version must contain 32 bytes")?;
    ensure!(
        value.policy_version == snapshot.overview.review_policy_version,
        "review policy changed during observation; retry"
    );
    Ok(ReviewComparison {
        source: revision_state(source)?,
        base: revision_state(base)?,
        policy: ContentHash::from_bytes(bytes),
    })
}

fn current_author(spool: &wire::SpoolRef) -> Result<(Ed25519Signer, SourceAuthor)> {
    let home = repo::identity::heddle_home_dir();
    let device = repo::identity::load_device(&repo::identity::device_identity_path())?
        .context("current device signing key unavailable; pair or enroll this device")?;
    let signer = Ed25519Signer::from_pem(&device.private_key_pem)?;
    let publisher: [u8; 32] = signer
        .public_key()
        .try_into()
        .context("device signing key must be Ed25519")?;
    let spool_id = uuid::Uuid::parse_str(&spool.id).context("Spool ID must be UUID")?;
    let author = repo::identity::source_author::load(&home, &publisher, spool_id)?;
    ensure!(
        matches!(&author, SourceAuthor::Account { spool, .. } if *spool == spool_id),
        "this device has no current account author proof for the Spool; refresh or enroll it"
    );
    Ok((signer, author))
}

fn sign_decision(
    snapshot: &ReviewSnapshot,
    kind: ReviewKind,
    comparison: ReviewComparison,
    explanation: String,
    revokes: Option<uuid::Uuid>,
    operation_id: uuid::Uuid,
) -> Result<wire::RecordReviewRequest> {
    let ReviewComparison {
        source,
        base,
        policy,
    } = comparison;
    let spool = snapshot
        .overview
        .r#ref
        .as_ref()
        .and_then(|thread| thread.spool.as_ref())
        .context("Thread has no Spool identity")?;
    let (signer, author) = current_author(spool)?;
    let SourceAuthor::Account {
        actor, authority, ..
    } = author
    else {
        bail!("current account author proof unavailable")
    };
    let prepared = PreparedControl::sign(
        &snapshot.overview,
        Control::Review(Review {
            id: operation_id,
            source,
            target: base,
            policy_version: policy,
            kind,
            explanation,
            revokes,
            expires_at_unix_seconds: None,
            coverage: None,
        }),
        Author {
            account: actor.principal_id,
            agent_id: actor.agent_id.as_deref(),
            authority_envelope: &authority,
        },
        operation_id,
        chrono::Utc::now().timestamp_millis(),
        &signer,
    )?;
    Ok(prepared.record_review()?)
}

fn operation_id(cli: &Cli) -> Result<uuid::Uuid> {
    let requested = cli.operation_id_wire();
    if requested.is_empty() {
        Ok(uuid::Uuid::now_v7())
    } else {
        uuid::Uuid::parse_str(&requested).context("--op-id must be a UUID")
    }
}

async fn review_scope(
    client: &HostedClient,
    address: &str,
) -> Result<(wire::SpoolRef, Vec<u8>, String)> {
    let spool = client.resolve_spool_ref(address).await?;
    let (_, author) = current_author(&spool)?;
    let SourceAuthor::Account { actor, .. } = author else {
        bail!("current account author proof unavailable")
    };
    let endpoint = client
        .native()
        .await?
        .description
        .endpoint
        .context("hosted endpoint identity absent")?
        .public_key;
    ensure!(endpoint.len() == 32, "hosted endpoint key is invalid");
    Ok((spool, endpoint, actor.principal_id.to_string()))
}

fn stored_reference(
    decision: &wire::ReviewDecision,
    spool: &wire::SpoolRef,
) -> Result<wire::ThreadRef> {
    let reference = decision
        .thread
        .as_ref()
        .context("stored decision has no Thread")?;
    ensure!(
        reference.spool.as_ref() == Some(spool),
        "stored review belongs to another Spool"
    );
    Ok(reference.clone())
}

fn replay_matches(
    request: &wire::RecordReviewRequest,
    reference: &wire::ThreadRef,
    principal: &str,
    kind: wire::review_decision::Kind,
    revokes: Option<uuid::Uuid>,
    note: Option<&str>,
) -> Result<()> {
    let decision = request
        .decision
        .as_ref()
        .context("stored review decision absent")?;
    ensure!(
        decision.thread.as_ref() == Some(reference) && decision.principal_id == principal,
        "operation ID belongs to another Thread or principal"
    );
    ensure!(
        decision.kind == kind as i32,
        "operation ID belongs to another review action"
    );
    ensure!(
        decision.revokes.as_ref().map(|r| r.id.as_str())
            == revokes.as_ref().map(|id| id.to_string()).as_deref(),
        "operation ID names another revoked review"
    );
    if let Some(note) = note {
        ensure!(
            decision.explanation == note,
            "operation ID names another review note"
        );
    }
    Ok(())
}

fn decision_output(decision: &wire::ReviewDecision, thread_name: &str) -> Result<ApprovalOutput> {
    let kind = match wire::review_decision::Kind::try_from(decision.kind)? {
        wire::review_decision::Kind::Approval => "approval",
        wire::review_decision::Kind::Rejection => "rejection",
        wire::review_decision::Kind::Opinion => "opinion",
        wire::review_decision::Kind::Revocation => "revocation",
        wire::review_decision::Kind::Read => "read",
        wire::review_decision::Kind::AgentPreview => "agent_preview",
        wire::review_decision::Kind::AgentCoReview => "agent_co_review",
        wire::review_decision::Kind::Unspecified => bail!("review has no decision kind"),
    };
    Ok(ApprovalOutput {
        id: decision
            .r#ref
            .as_ref()
            .context("review ID absent")?
            .id
            .clone(),
        thread: thread_name.to_owned(),
        source_revision: revision_state(decision.source.as_ref().context("review source absent")?)?
            .to_string(),
        base_revision: revision_state(decision.target.as_ref().context("review base absent")?)?
            .to_string(),
        policy_version: hex::encode(&decision.policy_version),
        principal_id: decision.principal_id.clone(),
        kind: kind.into(),
        explanation: decision.explanation.clone(),
        expires_at: decision
            .expires_at
            .as_ref()
            .and_then(|time| u64::try_from(time.seconds).ok())
            .unwrap_or_default(),
    })
}

pub async fn cmd_thread_approve(cli: &Cli, args: ThreadApproveArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let (client, address) = open_hosted_session(&repo, &args.remote).await?;
    let id = operation_id(cli)?;
    let (spool, endpoint, principal) = review_scope(&client, &address).await?;
    let mut outbox = ReviewOutbox::open()?;
    let request = match outbox.load(&endpoint, &principal, id)? {
        Some(StoredReview::Completed(selector, decision)) => {
            ensure!(
                selector == args.thread,
                "operation ID belongs to another Thread selector"
            );
            let reference = stored_reference(&decision, &spool)?;
            let previous = wire::RecordReviewRequest {
                client_operation_id: id.to_string(),
                decision: Some(decision.clone()),
                ..Default::default()
            };
            replay_matches(
                &previous,
                &reference,
                &principal,
                wire::review_decision::Kind::Approval,
                None,
                args.note.as_deref(),
            )?;
            client.close().await;
            let output = decision_output(&decision, &args.thread)?;
            if should_output_json(cli, Some(repo.config())) {
                write_full_command_json(
                    &output,
                    NextActionValidationContext::without_repo(&["thread", "approve"]),
                )?;
            } else {
                println!(
                    "Already approved Thread '{}' at {}",
                    args.thread, output.source_revision
                );
                println!("  review id: {}", output.id);
            }
            return Ok(());
        }
        Some(StoredReview::Pending(selector, stored)) => {
            ensure!(
                selector == args.thread,
                "operation ID belongs to another Thread selector"
            );
            let reference = stored_reference(
                stored
                    .decision
                    .as_ref()
                    .context("stored review decision absent")?,
                &spool,
            )?;
            replay_matches(
                &stored,
                &reference,
                &principal,
                wire::review_decision::Kind::Approval,
                None,
                args.note.as_deref(),
            )?;
            stored
        }
        None => {
            let reference = client.resolve_thread_ref(&address, &args.thread).await?;
            let snapshot = client.observe_review(&address, &args.thread).await?;
            ensure!(
                snapshot.endpoint_key == endpoint,
                "hosted endpoint changed during review preparation"
            );
            let prepared = sign_decision(
                &snapshot,
                ReviewKind::Approval,
                comparison(&snapshot)?,
                args.note.unwrap_or_default(),
                None,
                id,
            )?;
            ensure!(
                prepared
                    .decision
                    .as_ref()
                    .and_then(|decision| decision.thread.as_ref())
                    == Some(&reference),
                "review target changed during preparation"
            );
            outbox.save(&endpoint, &principal, id, &args.thread, &prepared)?;
            prepared
        }
    };
    let decision = request
        .decision
        .clone()
        .context("prepared review decision absent")?;
    let result = client.record_review(&request).await;
    client.close().await;
    result.with_context(|| {
        format!("review request may be pending; retry the exact original with --op-id {id}")
    })?;
    outbox.complete(&endpoint, &principal, id)?;
    let output = decision_output(&decision, &args.thread)?;
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["thread", "approve"]),
        )?;
    } else {
        println!(
            "Approved Thread '{}' at {}",
            args.thread, output.source_revision
        );
        println!("  review id: {}", output.id);
        println!("  compared base: {}", output.base_revision);
    }
    Ok(())
}

pub async fn cmd_thread_approvals(cli: &Cli, args: ThreadApprovalsArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let (client, address) = open_hosted_session(&repo, &args.remote).await?;
    let snapshot = client.observe_review(&address, &args.thread).await;
    client.close().await;
    let rows: Vec<_> = snapshot?
        .decisions
        .iter()
        .map(|row| decision_output(row, &args.thread))
        .collect::<Result<_>>()?;
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            &rows,
            NextActionValidationContext::without_repo(&["thread", "approvals"]),
        )?;
    } else if rows.is_empty() {
        println!("No review decisions recorded for Thread '{}'", args.thread);
    } else {
        println!(
            "{} review decisions for Thread '{}'",
            rows.len(),
            args.thread
        );
        for row in rows {
            println!(
                "  {}  {}  principal={}  source={}",
                row.id, row.kind, row.principal_id, row.source_revision
            );
            if !row.explanation.is_empty() {
                println!("    {}", row.explanation);
            }
        }
    }
    Ok(())
}

pub async fn cmd_thread_revoke_approval(cli: &Cli, args: ThreadRevokeApprovalArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let id = uuid::Uuid::parse_str(&args.id).context("review ID must be UUID")?;
    let (client, address) = open_hosted_session(&repo, &args.remote).await?;
    let operation = operation_id(cli)?;
    let (spool, endpoint, principal) = review_scope(&client, &address).await?;
    let mut outbox = ReviewOutbox::open()?;
    let request = match outbox.load(&endpoint, &principal, operation)? {
        Some(StoredReview::Completed(selector, decision)) => {
            ensure!(
                selector == args.thread,
                "operation ID belongs to another Thread selector"
            );
            let reference = stored_reference(&decision, &spool)?;
            let previous = wire::RecordReviewRequest {
                client_operation_id: operation.to_string(),
                decision: Some(decision),
                ..Default::default()
            };
            replay_matches(
                &previous,
                &reference,
                &principal,
                wire::review_decision::Kind::Revocation,
                Some(id),
                None,
            )?;
            client.close().await;
            if should_output_json(cli, Some(repo.config())) {
                write_full_command_json(
                    &ApprovalRevokeOutput {
                        output_kind: "thread_revoke_approval",
                        id: args.id,
                        revoked: true,
                    },
                    NextActionValidationContext::without_repo(&["thread", "revoke-approval"]),
                )?;
            } else {
                println!(
                    "Approval {} was already revoked for Thread '{}'",
                    args.id, args.thread
                );
            }
            return Ok(());
        }
        Some(StoredReview::Pending(selector, stored)) => {
            ensure!(
                selector == args.thread,
                "operation ID belongs to another Thread selector"
            );
            let reference = stored_reference(
                stored
                    .decision
                    .as_ref()
                    .context("stored review decision absent")?,
                &spool,
            )?;
            replay_matches(
                &stored,
                &reference,
                &principal,
                wire::review_decision::Kind::Revocation,
                Some(id),
                None,
            )?;
            stored
        }
        None => {
            let reference = client.resolve_thread_ref(&address, &args.thread).await?;
            let snapshot = client.observe_review(&address, &args.thread).await?;
            ensure!(
                snapshot.endpoint_key == endpoint,
                "hosted endpoint changed during review preparation"
            );
            let prior = snapshot
                .decisions
                .iter()
                .find(|decision| {
                    decision
                        .r#ref
                        .as_ref()
                        .is_some_and(|reference| reference.id == id.to_string())
                })
                .context("review ID is not visible on this Thread")?;
            ensure!(
                prior.kind == wire::review_decision::Kind::Approval as i32,
                "only an approval can be revoked by this command"
            );
            let policy: [u8; 32] = snapshot
                .overview
                .review_policy_version
                .as_slice()
                .try_into()
                .context("current review policy version absent")?;
            let prepared = sign_decision(
                &snapshot,
                ReviewKind::Revocation,
                ReviewComparison {
                    source: revision_state(
                        prior.source.as_ref().context("approval source absent")?,
                    )?,
                    base: revision_state(prior.target.as_ref().context("approval base absent")?)?,
                    policy: ContentHash::from_bytes(policy),
                },
                String::new(),
                Some(id),
                operation,
            )?;
            ensure!(
                prepared
                    .decision
                    .as_ref()
                    .and_then(|decision| decision.thread.as_ref())
                    == Some(&reference),
                "review target changed during preparation"
            );
            outbox.save(&endpoint, &principal, operation, &args.thread, &prepared)?;
            prepared
        }
    };
    let result = client.record_review(&request).await;
    client.close().await;
    result.with_context(|| {
        format!("revocation may be pending; retry the exact original with --op-id {operation}")
    })?;
    outbox.complete(&endpoint, &principal, operation)?;
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            &ApprovalRevokeOutput {
                output_kind: "thread_revoke_approval",
                id: args.id,
                revoked: true,
            },
            NextActionValidationContext::without_repo(&["thread", "revoke-approval"]),
        )?;
    } else {
        println!("Revoked approval {} for Thread '{}'", args.id, args.thread);
    }
    Ok(())
}

pub async fn cmd_thread_check_merge(cli: &Cli, args: ThreadCheckMergeArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let (client, address) = open_hosted_session(&repo, &args.remote).await?;
    let snapshot = client
        .observe_landing_assessment(&address, &args.thread, &args.target)
        .await;
    client.close().await;
    let snapshot = snapshot?;
    let assessment = snapshot
        .overview
        .landing_assessment
        .context("target-bound landing assessment unavailable; refresh and retry")?;
    let source_revision = revision_state(
        assessment
            .source
            .as_ref()
            .context("landing source absent")?,
    )?
    .to_string();
    let target_revision = revision_state(
        assessment
            .expected_target
            .as_ref()
            .context("landing target head absent")?,
    )?
    .to_string();
    let policy_version = hex::encode(&assessment.policy_version);
    let readiness = match wire::ReviewReadiness::try_from(assessment.readiness)? {
        wire::ReviewReadiness::Eligible => "eligible",
        wire::ReviewReadiness::NeedsApproval => "needs_approval",
        wire::ReviewReadiness::Blocked => "blocked",
        wire::ReviewReadiness::Unknown | wire::ReviewReadiness::Unspecified => "unknown",
    };
    let requirements: Vec<_> = assessment
        .requirements
        .into_iter()
        .map(|requirement| UnmetOutput {
            policy_id: requirement.policy.map(|record| record.id),
            kind: wire::RequirementKind::try_from(requirement.kind)
                .map(|kind| kind.as_str_name().to_ascii_lowercase())
                .unwrap_or_else(|_| "unknown".into()),
            explanation: requirement.explanation,
            recovery_method: requirement.recovery_method,
        })
        .collect();
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            &EligibilityOutput {
                thread: args.thread.clone(),
                target: args.target.clone(),
                source_revision: source_revision.clone(),
                target_revision: target_revision.clone(),
                policy_version,
                readiness: readiness.into(),
                requirements,
            },
            NextActionValidationContext::without_repo(&["thread", "readiness"]),
        )?;
    } else {
        println!(
            "Thread '{}' → '{}' readiness: {readiness}",
            args.thread, args.target
        );
        println!("  source: {source_revision}");
        println!("  target head: {target_revision}");
        for requirement in &requirements {
            println!("  {}: {}", requirement.kind, requirement.explanation);
        }
    }
    if readiness != "eligible" {
        return Err(anyhow!(crate::exit::OutcomeExit::data_err()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn review_cli_selects_one_thread_and_readiness_selects_exact_target() {
        let approve = Cli::try_parse_from(["heddle", "thread", "approve", "feature"])
            .expect("one Thread review");
        assert!(matches!(approve.command,
            crate::cli::cli_args::Commands::Thread {
                command: crate::cli::cli_args::ThreadCommands::Approve(ThreadApproveArgs { thread, .. })
            } if thread == "feature"));
        let readiness = Cli::try_parse_from(["heddle", "thread", "readiness", "feature", "main"])
            .expect("explicit target selection");
        assert!(matches!(readiness.command,
            crate::cli::cli_args::Commands::Thread {
                command: crate::cli::cli_args::ThreadCommands::CheckMerge(ThreadCheckMergeArgs { thread, target, .. })
            } if thread == "feature" && target == "main"));
        assert!(Cli::try_parse_from(["heddle", "thread", "readiness", "feature"]).is_err());
        assert!(
            Cli::try_parse_from(["heddle", "thread", "check-merge", "feature", "main"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "heddle",
                "thread",
                "readiness",
                "feature",
                "main",
                "--path",
                "src/lib.rs"
            ])
            .is_err()
        );
    }
}
