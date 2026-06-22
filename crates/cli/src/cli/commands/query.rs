// SPDX-License-Identifier: Apache-2.0
//! `heddle query` handler — structured query over the operation log.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use heddle_core::{QueryRequest, query};

use crate::cli::{
    cli_args::{Cli, QueryArgs},
    execution_context_from_cli, render, should_output_json,
};

pub async fn run(cli: &Cli, args: &QueryArgs) -> Result<()> {
    if let Some(file) = &args.attribution {
        return super::blame::cmd_query_attribution(
            cli,
            file.clone(),
            args.state.clone(),
            args.context,
        );
    }

    let ctx = execution_context_from_cli(cli)?;
    let report = query(
        &ctx,
        QueryRequest {
            actor: args.actor.clone().unwrap_or_default(),
            symbol: args.symbol.clone().unwrap_or_default(),
            signal_kind: args.signal.clone().unwrap_or_default(),
            thread: args.thread.clone().unwrap_or_default(),
            verbs: args.verbs.clone(),
            since_secs: parse_timestamp(args.since.as_deref())?,
            until_secs: parse_timestamp(args.until.as_deref())?,
            limit: args.limit,
            include_checkpoints: args.include_checkpoints,
        },
    )?;

    if should_output_json(cli, None) {
        render::query::query_json(&report)?;
    } else {
        render::query::query_text(&report)?;
    }
    Ok(())
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
