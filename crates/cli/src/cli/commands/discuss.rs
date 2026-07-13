// SPDX-License-Identifier: Apache-2.0
//! `heddle discuss` — append-only local collaboration.

use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use objects::object::{
    CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey, CollaborationOperationBodyV1,
    CollaborationOperationEnvelope, CollaborationResolution, DiscussionRecordId, DiscussionTurnV1,
    MaterializedDiscussion, StateId, VisibilityTier,
};
use repo::{
    CollaborationStore, CollaborationWriteDisposition, CollaborationWriteOutcome,
    RepositoryCapability, migrate_legacy_discussions_once,
};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, history_target::resolve_state_id, snapshot::ensure_current_state,
};
use crate::{
    cli::{
        cli_args::{
            Cli, DiscussAppendArgs, DiscussCommands, DiscussListArgs, DiscussOpenArgs,
            DiscussReopenArgs, DiscussResolveArgs, DiscussShowArgs, ResolveModeArg,
        },
        should_output_json,
    },
    config::UserConfig,
};

pub async fn run(cli: &Cli, command: &DiscussCommands) -> Result<()> {
    let repo = cli.open_repo().context("open Heddle repository")?;
    let store = open_store(&repo)?;
    match command {
        DiscussCommands::Open(args) => run_open(cli, &repo, &store, args),
        DiscussCommands::Append(args) => run_append(cli, &repo, &store, args),
        DiscussCommands::Resolve(args) => run_resolve(cli, &repo, &store, args),
        DiscussCommands::Reopen(args) => run_reopen(cli, &repo, &store, args),
        DiscussCommands::List(args) => run_list(cli, &repo, &store, args),
        DiscussCommands::Show(args) => run_show(cli, &store, args),
    }
}

#[derive(Serialize)]
struct DiscussionOutput {
    id: String,
    title: String,
    anchor: AnchorOutput,
    visibility: String,
    status: &'static str,
    resolution: Option<ResolutionOutput>,
    conflict_operation_ids: Vec<String>,
    head_operation_ids: Vec<String>,
    display_head_operation_id: String,
    turns: Vec<TurnOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum AnchorOutput {
    Repository,
    State {
        state_id: String,
    },
    Change {
        change_id: String,
    },
    Path {
        state_id: String,
        path: String,
    },
    Symbol {
        state_id: String,
        path: String,
        symbol: String,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum ResolutionOutput {
    AddressedByState { state_id: String },
    AddressedByChange { change_id: String },
    Dismissed { reason: String },
    Annotation { annotation_id: String },
}

#[derive(Serialize)]
struct TurnOutput {
    operation_id: String,
    author_name: String,
    author_email: String,
    agent: Option<String>,
    occurred_at_ms: i64,
    body: String,
    content_hash: String,
}

#[derive(Serialize)]
struct DiscussionWriteOutput {
    output_kind: &'static str,
    operation_id: String,
    disposition: CollaborationWriteDisposition,
    discussion: DiscussionOutput,
}

#[derive(Serialize)]
struct DiscussionShowOutput {
    output_kind: &'static str,
    discussion: DiscussionOutput,
}

#[derive(Serialize)]
struct DiscussionListOutput {
    output_kind: &'static str,
    discussions: Vec<DiscussionOutput>,
}

fn open_store(repo: &repo::Repository) -> Result<CollaborationStore> {
    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    migrate_legacy_discussions_once(repo, &store, repo.get_attribution()?)
        .context("migrate legacy discussions")?;
    Ok(store)
}

fn run_open(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    args: &DiscussOpenArgs,
) -> Result<()> {
    let state_id = resolve_open_state(repo, args.state.as_deref())?;
    let title = args
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            args.body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .unwrap_or(&args.symbol)
        .to_string();
    let discussion_id = DiscussionRecordId::generate();
    let operation = CollaborationOperationEnvelope::new(
        discussion_id,
        Vec::new(),
        idempotency_key(cli)?,
        repo.get_attribution()?,
        now_ms(),
        CollaborationOperationBodyV1::Open {
            title,
            anchor: CollaborationAnchor::Symbol {
                state_id,
                path: args.file.clone(),
                symbol: args.symbol.clone(),
            },
            visibility: parse_visibility(
                args.visibility.as_deref(),
                repo.resolve_capture_default_visibility(),
            )?,
            turn: DiscussionTurnV1::new(args.body.clone())?,
        },
    )?;
    let outcome = store.write_operation(&operation)?;
    emit_write(cli, "discuss_open", store, discussion_id, outcome)
}

fn run_append(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    args: &DiscussAppendArgs,
) -> Result<()> {
    write_descendant(
        cli,
        repo,
        store,
        &args.discussion_id,
        "discuss_append",
        CollaborationOperationBodyV1::AppendTurn {
            turn: DiscussionTurnV1::new(args.body.clone())?,
        },
    )
}

fn run_resolve(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    args: &DiscussResolveArgs,
) -> Result<()> {
    let resolution = match args.mode {
        ResolveModeArg::ByEdit => CollaborationResolution::AddressedByState {
            state_id: resolve_state(repo, args.state.as_deref())?,
        },
        ResolveModeArg::Dismiss => CollaborationResolution::Dismissed {
            reason: args
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .ok_or_else(|| anyhow!(RecoveryAdvice::discuss_resolve_missing_dismiss_reason()))?
                .to_string(),
        },
    };
    write_descendant(
        cli,
        repo,
        store,
        &args.discussion_id,
        "discuss_resolve",
        CollaborationOperationBodyV1::Resolve { resolution },
    )
}

fn run_reopen(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    args: &DiscussReopenArgs,
) -> Result<()> {
    write_descendant(
        cli,
        repo,
        store,
        &args.discussion_id,
        "discuss_reopen",
        CollaborationOperationBodyV1::Reopen {
            reason: args.reason.clone(),
        },
    )
}

fn write_descendant(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    raw_id: &str,
    output_kind: &'static str,
    body: CollaborationOperationBodyV1,
) -> Result<()> {
    let discussion_id = parse_discussion_id(raw_id)?;
    let discussion = store
        .materialize_discussion(&discussion_id)?
        .ok_or_else(|| anyhow!("discussion {discussion_id} not found"))?;
    let operation = CollaborationOperationEnvelope::new(
        discussion_id,
        discussion.heads.iter().copied().collect(),
        idempotency_key(cli)?,
        repo.get_attribution()?,
        now_ms(),
        body,
    )?;
    let outcome = store.write_operation(&operation)?;
    emit_write(cli, output_kind, store, discussion_id, outcome)
}

fn run_list(
    cli: &Cli,
    repo: &repo::Repository,
    store: &CollaborationStore,
    args: &DiscussListArgs,
) -> Result<()> {
    if args.symbol.is_some() && args.file.is_none() {
        return Err(anyhow!("discuss list --symbol requires --file"));
    }
    if !matches!(
        args.status.as_str(),
        "all" | "open" | "resolved" | "conflicted"
    ) {
        return Err(anyhow!(
            "invalid discussion status {:?}; expected open, resolved, conflicted, or all",
            args.status
        ));
    }
    let state_filter = args
        .state
        .as_deref()
        .map(|value| resolve_state(repo, Some(value)))
        .transpose()?;
    let materialized = store.materialize()?;
    let mut discussions = Vec::new();
    for discussion in materialized.discussions.into_values() {
        if !matches_filters(&discussion, args, state_filter.as_ref()) {
            continue;
        }
        discussions.push(to_view(store, &discussion)?);
    }
    let output = DiscussionListOutput {
        output_kind: "discuss_list",
        discussions,
    };
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else if output.discussions.is_empty() {
        println!("(no discussions)");
    } else {
        for discussion in output.discussions {
            println!(
                "{} [{}] {} — {}",
                discussion.id,
                discussion.status,
                anchor_label(&discussion.anchor),
                discussion.title
            );
        }
    }
    Ok(())
}

fn run_show(cli: &Cli, store: &CollaborationStore, args: &DiscussShowArgs) -> Result<()> {
    let discussion_id = parse_discussion_id(&args.discussion_id)?;
    let discussion = store
        .materialize_discussion(&discussion_id)?
        .ok_or_else(|| anyhow!("discussion {discussion_id} not found"))?;
    let output = DiscussionShowOutput {
        output_kind: "discuss_show",
        discussion: to_view(store, &discussion)?,
    };
    emit_show(cli, &output)
}

fn emit_write(
    cli: &Cli,
    output_kind: &'static str,
    store: &CollaborationStore,
    discussion_id: DiscussionRecordId,
    outcome: CollaborationWriteOutcome,
) -> Result<()> {
    let discussion = store
        .materialize_discussion(&discussion_id)?
        .ok_or_else(|| anyhow!("discussion {discussion_id} was not materialized after write"))?;
    let output = DiscussionWriteOutput {
        output_kind,
        operation_id: outcome.operation_id.to_string_full(),
        disposition: outcome.disposition,
        discussion: to_view(store, &discussion)?,
    };
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{} {} ({})",
            output.discussion.status, output.discussion.id, output.operation_id
        );
    }
    Ok(())
}

fn emit_show(cli: &Cli, output: &DiscussionShowOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }
    let discussion = &output.discussion;
    println!("discussion {} [{}]", discussion.id, discussion.status);
    println!("  title: {}", discussion.title);
    println!("  anchor: {}", anchor_label(&discussion.anchor));
    println!("  visibility: {}", discussion.visibility);
    println!("  heads: {}", discussion.head_operation_ids.join(", "));
    for turn in &discussion.turns {
        let actor = turn.agent.as_deref().unwrap_or(&turn.author_name);
        println!(
            "  {} {} @ {}",
            turn.operation_id, actor, turn.occurred_at_ms
        );
        for line in turn.body.lines() {
            println!("    {line}");
        }
    }
    Ok(())
}

fn to_view(store: &CollaborationStore, value: &MaterializedDiscussion) -> Result<DiscussionOutput> {
    let mut turns = Vec::with_capacity(value.turns.len());
    for (operation_id, turn) in &value.turns {
        let decoded = store
            .read_operation(operation_id)?
            .ok_or_else(|| anyhow!("discussion references missing operation {operation_id}"))?;
        let author = decoded.operation.author;
        turns.push(TurnOutput {
            operation_id: operation_id.to_string_full(),
            author_name: author.principal.name,
            author_email: author.principal.email,
            agent: author.agent.map(|agent| agent.to_string()),
            occurred_at_ms: decoded.operation.occurred_at_ms,
            body: turn.body.clone(),
            content_hash: turn.content_hash.to_hex(),
        });
    }
    let status = if !value.conflict_operations.is_empty() {
        "conflicted"
    } else if value.resolution.is_some() {
        "resolved"
    } else {
        "open"
    };
    Ok(DiscussionOutput {
        id: value.discussion_id.to_string(),
        title: value.title.clone(),
        anchor: anchor_output(&value.anchor),
        visibility: visibility_token(&value.visibility),
        status,
        resolution: value.resolution.as_ref().map(resolution_output),
        conflict_operation_ids: value
            .conflict_operations
            .iter()
            .map(CollabOpId::to_string_full)
            .collect(),
        head_operation_ids: value.heads.iter().map(CollabOpId::to_string_full).collect(),
        display_head_operation_id: value.display_head.to_string_full(),
        turns,
    })
}

fn matches_filters(
    discussion: &MaterializedDiscussion,
    args: &DiscussListArgs,
    state: Option<&StateId>,
) -> bool {
    let status_matches = match args.status.as_str() {
        "open" => discussion.resolution.is_none() && discussion.conflict_operations.is_empty(),
        "resolved" => discussion.resolution.is_some() && discussion.conflict_operations.is_empty(),
        "conflicted" => !discussion.conflict_operations.is_empty(),
        _ => true,
    };
    status_matches
        && state.is_none_or(|state| anchor_state(&discussion.anchor) == Some(state))
        && args.file.as_ref().is_none_or(|path| {
            anchor_path(&discussion.anchor).is_some_and(|candidate| candidate == path)
        })
        && args.symbol.as_ref().is_none_or(|symbol| {
            matches!(&discussion.anchor, CollaborationAnchor::Symbol { symbol: candidate, .. } if candidate == symbol)
        })
}

fn parse_discussion_id(value: &str) -> Result<DiscussionRecordId> {
    value
        .parse()
        .map_err(|error| anyhow!("invalid discussion id {value:?}: {error}"))
}

fn idempotency_key(cli: &Cli) -> Result<CollaborationIdempotencyKey> {
    CollaborationIdempotencyKey::new(
        cli.op_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    )
    .map_err(anyhow::Error::msg)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn resolve_state(repo: &repo::Repository, explicit: Option<&str>) -> Result<StateId> {
    if let Some(value) = explicit {
        return resolve_state_id(repo, value);
    }
    repo.head()?
        .ok_or_else(|| anyhow!(RecoveryAdvice::repository_no_head_anchor_first("discuss")))
}

fn resolve_open_state(repo: &repo::Repository, explicit: Option<&str>) -> Result<StateId> {
    if explicit.is_some() || repo.head()?.is_some() {
        return resolve_state(repo, explicit);
    }
    if repo.capability() == RepositoryCapability::GitOverlay
        && repo
            .git_overlay_worktree_status()?
            .is_some_and(|status| status.is_clean())
    {
        return ensure_current_state(
            repo,
            &UserConfig::load_default()?,
            Some("Bootstrap git-overlay before opening discussion".to_string()),
        );
    }
    Err(anyhow!(RecoveryAdvice::repository_no_head_anchor_first(
        "discuss"
    )))
}

fn parse_visibility(value: Option<&str>, default: VisibilityTier) -> Result<VisibilityTier> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match value {
        "public" => Ok(VisibilityTier::Public),
        "internal" => Ok(VisibilityTier::Internal),
        _ if value.starts_with("team:") => labelled_visibility(value, "team:", |label| {
            VisibilityTier::TeamScoped { team_id: label }
        }),
        _ if value.starts_with("restricted:") => {
            labelled_visibility(value, "restricted:", |label| VisibilityTier::Restricted {
                scope_label: label,
            })
        }
        _ if value.starts_with("private:") => labelled_visibility(value, "private:", |label| {
            VisibilityTier::Private { scope_label: label }
        }),
        _ => Err(anyhow!(
            "invalid visibility {value:?}; expected public, internal, team:<id>, restricted:<label>, or private:<label>"
        )),
    }
}

fn labelled_visibility(
    value: &str,
    prefix: &str,
    build: impl FnOnce(String) -> VisibilityTier,
) -> Result<VisibilityTier> {
    let label = value.trim_start_matches(prefix).trim();
    if label.is_empty() {
        return Err(anyhow!("visibility {prefix}<label> requires a label"));
    }
    Ok(build(label.to_string()))
}

fn visibility_token(value: &VisibilityTier) -> String {
    match value {
        VisibilityTier::Public => "public".to_string(),
        VisibilityTier::Internal => "internal".to_string(),
        VisibilityTier::TeamScoped { team_id } => format!("team:{team_id}"),
        VisibilityTier::Restricted { scope_label } => format!("restricted:{scope_label}"),
        VisibilityTier::Private { scope_label } => format!("private:{scope_label}"),
    }
}

fn anchor_output(value: &CollaborationAnchor) -> AnchorOutput {
    match value {
        CollaborationAnchor::Repository => AnchorOutput::Repository,
        CollaborationAnchor::State { state_id } => AnchorOutput::State {
            state_id: state_id.to_string_full(),
        },
        CollaborationAnchor::Change { change_id } => AnchorOutput::Change {
            change_id: change_id.to_string_full(),
        },
        CollaborationAnchor::Path { state_id, path } => AnchorOutput::Path {
            state_id: state_id.to_string_full(),
            path: path.clone(),
        },
        CollaborationAnchor::Symbol {
            state_id,
            path,
            symbol,
        } => AnchorOutput::Symbol {
            state_id: state_id.to_string_full(),
            path: path.clone(),
            symbol: symbol.clone(),
        },
    }
}

fn resolution_output(value: &CollaborationResolution) -> ResolutionOutput {
    match value {
        CollaborationResolution::AddressedByState { state_id } => {
            ResolutionOutput::AddressedByState {
                state_id: state_id.to_string_full(),
            }
        }
        CollaborationResolution::AddressedByChange { change_id } => {
            ResolutionOutput::AddressedByChange {
                change_id: change_id.to_string_full(),
            }
        }
        CollaborationResolution::Dismissed { reason } => ResolutionOutput::Dismissed {
            reason: reason.clone(),
        },
        CollaborationResolution::Annotation { annotation_id } => ResolutionOutput::Annotation {
            annotation_id: annotation_id.clone(),
        },
    }
}

fn anchor_state(value: &CollaborationAnchor) -> Option<&StateId> {
    match value {
        CollaborationAnchor::State { state_id }
        | CollaborationAnchor::Path { state_id, .. }
        | CollaborationAnchor::Symbol { state_id, .. } => Some(state_id),
        CollaborationAnchor::Repository | CollaborationAnchor::Change { .. } => None,
    }
}

fn anchor_path(value: &CollaborationAnchor) -> Option<&str> {
    match value {
        CollaborationAnchor::Path { path, .. } | CollaborationAnchor::Symbol { path, .. } => {
            Some(path)
        }
        _ => None,
    }
}

fn anchor_label(value: &AnchorOutput) -> String {
    match value {
        AnchorOutput::Repository => "repository".to_string(),
        AnchorOutput::State { state_id } => state_id.clone(),
        AnchorOutput::Change { change_id } => change_id.clone(),
        AnchorOutput::Path { path, .. } => path.clone(),
        AnchorOutput::Symbol { path, symbol, .. } => format!("{path}:{symbol}"),
    }
}
