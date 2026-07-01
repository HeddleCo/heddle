// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` handler.
//!
//! Invokes the W2 [`LocalDiscussionService`](daemon::grpc_local_impl::LocalDiscussionService)
//! in-process so the CLI works without a running daemon. The same service
//! is reachable over the local UDS transport — calling it directly here
//! avoids the round-trip overhead while preserving the exact contract.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use daemon::grpc_local_impl::{GrpcLocalService, LocalDiscussionService};
use grpc::heddle::v1::{
    AppendTurnRequest, GetDiscussionRequest, ListDiscussionsByStateRequest,
    ListDiscussionsBySymbolRequest, OpenDiscussionRequest, PathSymbolRef, ResolveDiscussionRequest,
    discussion_service_server::DiscussionService,
};
use repo::{RepositoryCapability, operation_dedup::OperationDedupStore};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, history_target::resolve_state_id, snapshot::ensure_current_state,
};
use crate::{
    cli::{
        cli_args::{
            Cli, DiscussAppendArgs, DiscussCommands, DiscussListArgs, DiscussOpenArgs,
            DiscussResolveArgs, DiscussShowArgs, ResolveModeArg,
        },
        should_output_json,
    },
    config::UserConfig,
};

pub async fn run(cli: &Cli, command: &DiscussCommands) -> Result<()> {
    let svc = open_service(cli)?;
    match command {
        DiscussCommands::Open(args) => run_open(cli, &svc, args).await,
        DiscussCommands::Append(args) => run_append(cli, &svc, args).await,
        DiscussCommands::Resolve(args) => run_resolve(cli, &svc, args).await,
        DiscussCommands::List(args) => run_list(cli, &svc, args).await,
        DiscussCommands::Show(args) => run_show(cli, &svc, args).await,
    }
}

#[derive(Serialize)]
struct DiscussionOutput {
    id: String,
    file: String,
    symbol: String,
    opened_against_state: String,
    opened_at_secs: i64,
    visibility: String,
    body_changed_since_open: bool,
    orphaned: bool,
    resolution: ResolutionView,
    turns: Vec<TurnView>,
    resolved_annotation_id: Option<String>,
}

#[derive(Serialize)]
struct ResolutionView {
    kind: String,
    annotation_id: Option<String>,
    change_id: Option<String>,
    reason: Option<String>,
}

#[derive(Serialize)]
struct TurnView {
    author_name: String,
    author_email: String,
    body: String,
    posted_at_secs: i64,
}

#[derive(Serialize)]
struct DiscussionListOutput {
    output_kind: &'static str,
    discussions: Vec<DiscussionOutput>,
}

/// Envelope used by per-discussion verbs (`open`/`append`/`resolve`/`show`).
/// The verb's `output_kind` rides on top of the discussion payload; the
/// inner fields stay flat for wire compat with PR #251's discussion
/// schema.
#[derive(Serialize)]
struct DiscussionEnvelope<'a> {
    output_kind: &'static str,
    #[serde(flatten)]
    discussion: &'a DiscussionOutput,
}

fn open_service(cli: &Cli) -> Result<LocalDiscussionService> {
    let repo = cli.open_repo().context("open Heddle repository")?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalDiscussionService::new(inner))
}

async fn run_open(cli: &Cli, svc: &LocalDiscussionService, args: &DiscussOpenArgs) -> Result<()> {
    let state_id = resolve_open_state(cli, args.state.as_deref())?;
    let req = OpenDiscussionRequest {
        repo_path: String::new(),
        state_id,
        anchor: Some(PathSymbolRef {
            file: args.file.clone(),
            symbol: args.symbol.clone(),
        }),
        body: args.body.clone(),
        visibility: args.visibility.clone().unwrap_or_default(),
        thread_ref: args.thread.clone().unwrap_or_default(),
        client_operation_id: crate::operation_id::wire(cli),
    };
    let resp = svc
        .open_discussion(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?;
    emit_discussion(cli, "discuss_open", &to_view(&resp.into_inner()))
}

async fn run_append(
    cli: &Cli,
    svc: &LocalDiscussionService,
    args: &DiscussAppendArgs,
) -> Result<()> {
    let req = AppendTurnRequest {
        repo_path: String::new(),
        discussion_id: args.discussion_id.clone(),
        body: args.body.clone(),
        client_operation_id: crate::operation_id::wire(cli),
    };
    let resp = svc
        .append_turn(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?;
    emit_discussion(cli, "discuss_append", &to_view(&resp.into_inner()))
}

async fn run_resolve(
    cli: &Cli,
    svc: &LocalDiscussionService,
    args: &DiscussResolveArgs,
) -> Result<()> {
    use grpc::heddle::v1::resolve_discussion_request::{
        Resolution, ResolveByEdit, ResolveDismissed, ResolveIntoAnnotation,
    };
    let resolution = match args.mode {
        ResolveModeArg::IntoAnnotation => {
            let kind_str = args.annotation_kind.as_deref().ok_or_else(|| {
                anyhow!(RecoveryAdvice::discuss_resolve_missing_annotation_kind())
            })?;
            let kind = parse_annotation_kind(kind_str)?;
            let content = args.annotation_content.clone().ok_or_else(|| {
                anyhow!(RecoveryAdvice::discuss_resolve_missing_annotation_content())
            })?;
            let tags = args
                .annotation_tags
                .as_deref()
                .map(|raw| {
                    raw.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Resolution::IntoAnnotation(ResolveIntoAnnotation {
                kind: kind as i32,
                content,
                tags,
            })
        }
        ResolveModeArg::ByEdit => Resolution::ByEdit(ResolveByEdit {
            state_id: resolve_state(cli, args.state.as_deref())?,
        }),
        ResolveModeArg::Dismiss => Resolution::Dismissed(ResolveDismissed {
            reason: args
                .reason
                .clone()
                .ok_or_else(|| anyhow!(RecoveryAdvice::discuss_resolve_missing_dismiss_reason()))?,
        }),
    };
    let req = ResolveDiscussionRequest {
        repo_path: String::new(),
        discussion_id: args.discussion_id.clone(),
        resolution: Some(resolution),
        client_operation_id: crate::operation_id::wire(cli),
    };
    let resp = svc
        .resolve_discussion(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?;
    emit_discussion(cli, "discuss_resolve", &to_view(&resp.into_inner()))
}

async fn run_list(cli: &Cli, svc: &LocalDiscussionService, args: &DiscussListArgs) -> Result<()> {
    let discussions = if let (Some(file), Some(symbol)) = (&args.file, &args.symbol) {
        let req = ListDiscussionsBySymbolRequest {
            repo_path: String::new(),
            anchor: Some(PathSymbolRef {
                file: file.clone(),
                symbol: symbol.clone(),
            }),
            status: args.status.clone(),
        };
        let resp = svc
            .list_by_symbol(tonic::Request::new(req))
            .await
            .map_err(status_to_anyhow)?;
        resp.into_inner().discussions
    } else {
        let state_id = resolve_state(cli, args.state.as_deref())?;
        let req = ListDiscussionsByStateRequest {
            repo_path: String::new(),
            state_id,
            status: args.status.clone(),
        };
        let resp = svc
            .list_by_state(tonic::Request::new(req))
            .await
            .map_err(status_to_anyhow)?;
        resp.into_inner().discussions
    };
    let output = DiscussionListOutput {
        output_kind: "discuss_list",
        discussions: discussions.iter().map(to_view).collect(),
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serialize discussion list")?
        );
    } else if output.discussions.is_empty() {
        println!("(no discussions)");
    } else {
        for d in &output.discussions {
            println!(
                "{} [{}] {}:{} — {}",
                d.id,
                d.resolution.kind,
                d.file,
                d.symbol,
                d.turns
                    .first()
                    .map(|t| t.body.lines().next().unwrap_or(""))
                    .unwrap_or("")
            );
        }
    }
    Ok(())
}

async fn run_show(cli: &Cli, svc: &LocalDiscussionService, args: &DiscussShowArgs) -> Result<()> {
    // Empty state_id = HEAD (the default). An explicit `--state` resolves the
    // discussion against a prior state (#836) via the canonical resolver, so
    // short/full ids and marker names all work.
    let state_id = match args.state.as_deref() {
        Some(s) => resolve_state(cli, Some(s))?,
        None => Vec::new(),
    };
    let req = GetDiscussionRequest {
        repo_path: String::new(),
        discussion_id: args.discussion_id.clone(),
        state_id,
    };
    let resp = svc
        .get_discussion(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?;
    emit_discussion(cli, "discuss_show", &to_view(&resp.into_inner()))
}

fn emit_discussion(cli: &Cli, output_kind: &'static str, view: &DiscussionOutput) -> Result<()> {
    if should_output_json(cli, None) {
        let envelope = DiscussionEnvelope {
            output_kind,
            discussion: view,
        };
        println!(
            "{}",
            serde_json::to_string(&envelope).context("serialize discussion")?
        );
    } else {
        println!("discussion {}", view.id);
        println!("  anchor: {}:{}", view.file, view.symbol);
        println!("  state: {}", view.opened_against_state);
        println!("  visibility: {}", view.visibility);
        println!("  status: {}", view.resolution.kind);
        if view.body_changed_since_open {
            println!("  ⚠ body changed since open");
        }
        if view.orphaned {
            println!("  ⚠ symbol no longer present (orphaned)");
        }
        for (i, turn) in view.turns.iter().enumerate() {
            println!(
                "  [{i}] {} <{}> @ {}",
                turn.author_name, turn.author_email, turn.posted_at_secs
            );
            for line in turn.body.lines() {
                println!("      {line}");
            }
        }
        if let Some(annotation_id) = &view.resolved_annotation_id {
            println!("  resolved into annotation {annotation_id}");
        }
    }
    Ok(())
}

fn to_view(d: &grpc::heddle::v1::Discussion) -> DiscussionOutput {
    use grpc::heddle::v1::discussion_resolution::State;
    let anchor = d.anchor.clone().unwrap_or_default();
    let resolution_view = match d.resolution.as_ref().and_then(|r| r.state.as_ref()) {
        Some(State::Open(_)) | None => ResolutionView {
            kind: "open".into(),
            annotation_id: None,
            change_id: None,
            reason: None,
        },
        Some(State::IntoAnnotation(p)) => ResolutionView {
            kind: "resolved_into_annotation".into(),
            annotation_id: opt_string(p.annotation_id.clone()),
            change_id: None,
            reason: None,
        },
        Some(State::ByEdit(p)) => ResolutionView {
            kind: "resolved_by_edit".into(),
            annotation_id: None,
            change_id: if p.state_id.is_empty() {
                None
            } else {
                objects::object::ChangeId::try_from_slice(&p.state_id)
                    .ok()
                    .map(|id| id.to_string_full())
            },
            reason: None,
        },
        Some(State::Dismissed(p)) => ResolutionView {
            kind: "dismissed".into(),
            annotation_id: None,
            change_id: None,
            reason: opt_string(p.reason.clone()),
        },
    };
    DiscussionOutput {
        id: d.id.clone(),
        file: anchor.file,
        symbol: anchor.symbol,
        opened_against_state: objects::object::ChangeId::try_from_slice(&d.opened_against_state)
            .map(|id| id.to_string_full())
            .unwrap_or_default(),
        opened_at_secs: d.opened_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        visibility: d.visibility.clone(),
        body_changed_since_open: d.body_changed_since_open,
        orphaned: d.orphaned,
        resolution: resolution_view,
        turns: d
            .turns
            .iter()
            .map(|t| TurnView {
                author_name: t.author_name.clone(),
                author_email: t.author_email.clone(),
                body: t.body.clone(),
                posted_at_secs: t.posted_at.as_ref().map(|x| x.seconds).unwrap_or(0),
            })
            .collect(),
        resolved_annotation_id: opt_string(d.resolved_annotation_id.clone()),
    }
}

fn opt_string(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn resolve_state(cli: &Cli, explicit: Option<&str>) -> Result<Vec<u8>> {
    let repo = cli.open_repo().context("open Heddle repository")?;
    if let Some(s) = explicit {
        // Routes through the canonical resolver so short/full IDs and
        // marker names all work — matches `heddle log --output json` output.
        return Ok(resolve_state_id(&repo, s)?.as_bytes().to_vec());
    }
    let head = repo
        .head()
        .context("read HEAD")?
        .ok_or_else(|| anyhow!(RecoveryAdvice::repository_no_head_anchor_first("discuss")))?;
    Ok(head.as_bytes().to_vec())
}

fn resolve_open_state(cli: &Cli, explicit: Option<&str>) -> Result<Vec<u8>> {
    let repo = cli.open_repo().context("open Heddle repository")?;
    if let Some(s) = explicit {
        return Ok(resolve_state_id(&repo, s)?.as_bytes().to_vec());
    }
    if let Some(head) = repo.head().context("read HEAD")? {
        return Ok(head.as_bytes().to_vec());
    }
    if repo.capability() == RepositoryCapability::GitOverlay
        && repo
            .git_overlay_worktree_status()?
            .is_some_and(|status| status.is_clean())
    {
        let state_id = ensure_current_state(
            &repo,
            &UserConfig::load_default()?,
            Some("Bootstrap git-overlay before opening discussion".to_string()),
        )?;
        return Ok(state_id.as_bytes().to_vec());
    }
    Err(anyhow!(RecoveryAdvice::repository_no_head_anchor_first(
        "discuss"
    )))
}

fn status_to_anyhow(status: tonic::Status) -> anyhow::Error {
    anyhow!("{}: {}", status.code(), status.message())
}

fn parse_annotation_kind(value: &str) -> Result<grpc::heddle::v1::ContextAnnotationKind> {
    use grpc::heddle::v1::ContextAnnotationKind;
    match value.trim().to_ascii_lowercase().as_str() {
        "constraint" => Ok(ContextAnnotationKind::Constraint),
        "invariant" => Ok(ContextAnnotationKind::Invariant),
        "rationale" => Ok(ContextAnnotationKind::Rationale),
        other => Err(anyhow!(
            "invalid --annotation-kind '{other}': expected constraint|invariant|rationale"
        )),
    }
}
