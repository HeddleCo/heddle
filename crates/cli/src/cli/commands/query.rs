// SPDX-License-Identifier: Apache-2.0
//! `heddle query` handler — structured query over the operation log.

use anyhow::Context;
use anyhow::{Result, anyhow};
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use heddle_query::OperationLogQuery;
use repo::Repository;
use serde::Serialize;

use crate::cli::cli_args::{Cli, QueryArgs};
use crate::cli::should_output_json;

#[derive(Serialize)]
struct QueryOutput {
    output_kind: &'static str,
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
    if let Some(file) = &args.attribution {
        return super::blame::cmd_query_attribution(
            cli,
            file.clone(),
            args.state.clone(),
            args.context,
        );
    }

    let cwd = std::env::current_dir().context("get current working directory")?;
    let repo = Repository::open(&cwd).context("open Heddle repository")?;
    let mut query = OperationLogQuery {
        actor: args.actor.clone(),
        symbol: args.symbol.clone(),
        signal_kind: args.signal.clone(),
        thread: args.thread.clone(),
        verbs: (!args.verbs.is_empty()).then(|| args.verbs.clone()),
        since: parse_timestamp(args.since.as_deref())?,
        until: parse_timestamp(args.until.as_deref())?,
        limit: (args.limit > 0).then_some(args.limit as usize),
    };
    heddle_query::apply_checkpoint_filter(&mut query, args.include_checkpoints);
    let hits = heddle_query::query_operations(repo.heddle_dir(), &query)?;
    let output = QueryOutput {
        output_kind: "query",
        hits: hits
            .iter()
            .map(|h| HitView {
                seq: h.seq,
                timestamp_secs: h.timestamp_secs,
                verb: h.verb.clone(),
                actor_email: h.actor_email.clone(),
                operation_id: h.operation_id.map(|op_id| op_id.to_string()),
                thread: h.thread.clone(),
                symbols: h.symbols.clone(),
                signal_kinds: h.signal_kinds.clone(),
                change_id: h.change_id.map(|id| id.to_string_full()),
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

fn parse_timestamp(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // RFC3339 first. Falls through to humantime for human shortcuts.
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(Some(dt.with_timezone(&Utc)));
    }
    // Humantime: "1h", "30m", "2d" — relative to now.
    let lower = trimmed.to_ascii_lowercase();
    if let Some(secs) = parse_relative(&lower) {
        let when = Utc::now() - chrono::Duration::seconds(secs);
        return Ok(Some(when));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamp_rfc3339() {
        let when = parse_timestamp(Some("2026-05-04T12:00:00Z"))
            .unwrap()
            .expect("timestamp");
        assert!(when.timestamp() > 0);
    }

    #[test]
    fn parse_timestamp_humantime_hour() {
        let when = parse_timestamp(Some("1h")).unwrap().expect("timestamp");
        let now = Utc::now().timestamp();
        // Should be roughly an hour ago, within a few seconds.
        assert!((now - when.timestamp() - 3600).abs() < 5);
    }

    #[test]
    fn parse_timestamp_humantime_day() {
        let when = parse_timestamp(Some("2d")).unwrap().expect("timestamp");
        let now = Utc::now().timestamp();
        assert!((now - when.timestamp() - 2 * 86400).abs() < 5);
    }

    #[test]
    fn parse_timestamp_unset_is_none() {
        assert_eq!(parse_timestamp(None).unwrap(), None);
        assert_eq!(parse_timestamp(Some("")).unwrap(), None);
    }

    #[test]
    fn parse_timestamp_garbage_errors() {
        assert!(parse_timestamp(Some("not-a-time")).is_err());
    }
}
