// SPDX-License-Identifier: Apache-2.0
//! `heddle transaction begin|commit|abort|status` (A12) handler.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use grpc::heddle::v1::{
    AbortTransactionRequest, BeginTransactionRequest, CommitTransactionRequest,
    GetTransactionStatusRequest, transaction_service_server::TransactionService,
};
use repo::{Repository, operation_dedup::OperationDedupStore};
use serde::Serialize;
use daemon::grpc_local_impl::{GrpcLocalService, LocalTransactionService};

use crate::cli::{
    cli_args::{
        Cli, TransactionAbortArgs, TransactionBeginArgs, TransactionCommands, TransactionIdArgs,
    },
    should_output_json,
};

#[derive(Serialize)]
struct BeginOutput {
    transaction_id: String,
    started_at_secs: i64,
}

#[derive(Serialize)]
struct CommitOutput {
    change_id: String,
    op_count: u32,
}

#[derive(Serialize)]
struct AbortOutput {
    aborted: bool,
}

#[derive(Serialize)]
struct StatusOutput {
    transaction_id: String,
    state: String,
    started_at_secs: i64,
    buffered_ops: u32,
}

pub async fn run(cli: &Cli, command: &TransactionCommands) -> Result<()> {
    let svc = open_service()?;
    match command {
        TransactionCommands::Begin(args) => run_begin(cli, &svc, args).await,
        TransactionCommands::Commit(args) => run_commit(cli, &svc, args).await,
        TransactionCommands::Abort(args) => run_abort(cli, &svc, args).await,
        TransactionCommands::Status(args) => run_status(cli, &svc, args).await,
    }
}

async fn run_begin(
    cli: &Cli,
    svc: &LocalTransactionService,
    args: &TransactionBeginArgs,
) -> Result<()> {
    let resp = svc
        .begin_transaction(tonic::Request::new(BeginTransactionRequest {
            repo_path: String::new(),
            thread: args.thread.clone().unwrap_or_default(),
            message: args.message.clone().unwrap_or_default(),
            client_operation_id: crate::operation_id::wire(cli),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let out = BeginOutput {
        transaction_id: resp.transaction_id,
        started_at_secs: resp.started_at.as_ref().map(|t| t.seconds).unwrap_or(0),
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&out).context("serialize begin output")?
        );
    } else {
        println!("transaction {}", out.transaction_id);
        println!("  started_at: {}", out.started_at_secs);
    }
    Ok(())
}

async fn run_commit(
    cli: &Cli,
    svc: &LocalTransactionService,
    args: &TransactionIdArgs,
) -> Result<()> {
    let resp = svc
        .commit_transaction(tonic::Request::new(CommitTransactionRequest {
            repo_path: String::new(),
            transaction_id: args.transaction_id.clone(),
            client_operation_id: crate::operation_id::wire(cli),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let out = CommitOutput {
        change_id: objects::object::ChangeId::try_from_slice(&resp.state_id)
            .map(|id| id.to_string_full())
            .unwrap_or_default(),
        op_count: resp.op_count,
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&out).context("serialize commit output")?
        );
    } else {
        println!("committed {} ({} ops)", out.change_id, out.op_count);
    }
    Ok(())
}

async fn run_abort(
    cli: &Cli,
    svc: &LocalTransactionService,
    args: &TransactionAbortArgs,
) -> Result<()> {
    let resp = svc
        .abort_transaction(tonic::Request::new(AbortTransactionRequest {
            repo_path: String::new(),
            transaction_id: args.transaction_id.clone(),
            reason: args.reason.clone(),
            client_operation_id: crate::operation_id::wire(cli),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let out = AbortOutput {
        aborted: resp.aborted,
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&out).context("serialize abort output")?
        );
    } else if out.aborted {
        println!("aborted transaction {}", args.transaction_id);
    } else {
        println!("transaction {} was not aborted", args.transaction_id);
    }
    Ok(())
}

async fn run_status(
    cli: &Cli,
    svc: &LocalTransactionService,
    args: &TransactionIdArgs,
) -> Result<()> {
    let resp = svc
        .get_transaction_status(tonic::Request::new(GetTransactionStatusRequest {
            repo_path: String::new(),
            transaction_id: args.transaction_id.clone(),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let out = StatusOutput {
        transaction_id: resp.transaction_id,
        state: resp.state,
        started_at_secs: resp.started_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        buffered_ops: resp.buffered_ops,
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&out).context("serialize status output")?
        );
    } else {
        println!("transaction {}", out.transaction_id);
        println!("  state: {}", out.state);
        println!("  started_at: {}", out.started_at_secs);
        println!("  buffered_ops: {}", out.buffered_ops);
    }
    Ok(())
}

fn open_service() -> Result<LocalTransactionService> {
    let cwd = std::env::current_dir().context("get current working directory")?;
    let repo = Repository::open(&cwd).context("open Heddle repository")?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalTransactionService::new(inner))
}

fn status_to_anyhow(status: tonic::Status) -> anyhow::Error {
    anyhow!("{}: {}", status.code(), status.message())
}