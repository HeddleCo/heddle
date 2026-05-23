// SPDX-License-Identifier: Apache-2.0
//! `heddle query` handler — structured query over the operation log (A10).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use daemon::grpc_local_impl::{GrpcLocalService, LocalOperationLogQueryService};
use grpc::heddle::v1::{
    QueryOperationsRequest, operation_log_query_service_server::OperationLogQueryService,
};
use repo::{Repository, operation_dedup::OperationDedupStore};
use serde::Serialize;

use crate::cli::{
    cli_args::{Cli, QueryArgs},
    should_output_json,
};

#[derive(Serialize)]
struct QueryOutput {
    hits: Vec<HitView>,
}

#[derive(Serialize)]
struct HitView {
    seq: u64,
    timestamp_secs: i64,
    verb: String,
    actor_email: String,
    operation_id: Option<String>,
    thread: Option<String>,
    symbols: Vec<String>,
    signal_kinds: Vec<String>,
    change_id: Option<String>,
}

pub async fn run(cli: &Cli, args: &QueryArgs) -> Result<()> {
    let svc = open_service()?;
    let req = QueryOperationsRequest {
        repo_path: String::new(),
        actor: args.actor.clone().unwrap_or_default(),
        symbol: args.symbol.clone().unwrap_or_default(),
        signal_kind: args.signal.clone().unwrap_or_default(),
        thread: args.thread.clone().unwrap_or_default(),
        verbs: args.verbs.clone(),
        since_secs: parse_timestamp(args.since.as_deref())?,
        until_secs: parse_timestamp(args.until.as_deref())?,
        limit: args.limit,
        include_checkpoints: args.include_checkpoints,
    };
    let resp = svc
        .query_operations(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let output = QueryOutput {
        hits: resp
            .hits
            .iter()
            .map(|h| HitView {
                seq: h.seq,
                timestamp_secs: h.timestamp_secs,
                verb: h.verb.clone(),
                actor_email: h.actor_email.clone(),
                operation_id: opt_string(h.operation_id.clone()),
                thread: opt_string(h.thread.clone()),
                symbols: h.symbols.clone(),
                signal_kinds: h.signal_kinds.clone(),
                change_id: opt_change_id(&h.change_id),
            })
            .collect(),
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serialize query output")?
        );
    } else if output.hits.is_empty() {
        println!("(no matches)");
    } else {
        for hit in &output.hits {
            let ts = Utc
                .timestamp_opt(hit.timestamp_secs, 0)
                .single()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| hit.timestamp_secs.to_string());
            print!("#{} {} {} <{}>", hit.seq, ts, hit.verb, hit.actor_email);
            if let Some(thread) = &hit.thread {
                print!(" thread={thread}");
            }
            if let Some(change_id) = &hit.change_id {
                print!(" -> {change_id}");
            }
            println!();
        }
    }
    Ok(())
}

fn open_service() -> Result<LocalOperationLogQueryService> {
    let cwd = std::env::current_dir().context("get current working directory")?;
    let repo = Repository::open(&cwd).context("open Heddle repository")?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalOperationLogQueryService::new(inner))
}

fn parse_timestamp(value: Option<&str>) -> Result<i64> {
    let Some(raw) = value else {
        return Ok(0);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    // RFC3339 first. Falls through to humantime for human shortcuts.
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc).timestamp());
    }
    // Humantime: "1h", "30m", "2d" — relative to now.
    let lower = trimmed.to_ascii_lowercase();
    if let Some(secs) = parse_relative(&lower) {
        let when = Utc::now() - chrono::Duration::seconds(secs);
        return Ok(when.timestamp());
    }
    Err(anyhow!(
        "invalid timestamp '{trimmed}': expected RFC3339 or humantime (e.g. 1h, 2d)"
    ))
}

fn parse_relative(s: &str) -> Option<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        "w" | "wk" | "wks" | "week" | "weeks" => n * 86400 * 7,
        _ => return None,
    };
    Some(secs)
}

fn opt_string(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn opt_change_id(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    objects::object::ChangeId::try_from_slice(bytes)
        .ok()
        .map(|id| id.to_string_full())
}

fn status_to_anyhow(status: tonic::Status) -> anyhow::Error {
    anyhow!("{}: {}", status.code(), status.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamp_rfc3339() {
        let secs = parse_timestamp(Some("2026-05-04T12:00:00Z")).unwrap();
        assert!(secs > 0);
    }

    #[test]
    fn parse_timestamp_humantime_hour() {
        let secs = parse_timestamp(Some("1h")).unwrap();
        let now = Utc::now().timestamp();
        // Should be roughly an hour ago, within a few seconds.
        assert!((now - secs - 3600).abs() < 5);
    }

    #[test]
    fn parse_timestamp_humantime_day() {
        let secs = parse_timestamp(Some("2d")).unwrap();
        let now = Utc::now().timestamp();
        assert!((now - secs - 2 * 86400).abs() < 5);
    }

    #[test]
    fn parse_timestamp_unset_is_zero() {
        assert_eq!(parse_timestamp(None).unwrap(), 0);
        assert_eq!(parse_timestamp(Some("")).unwrap(), 0);
    }

    #[test]
    fn parse_timestamp_garbage_errors() {
        assert!(parse_timestamp(Some("not-a-time")).is_err());
    }
}
