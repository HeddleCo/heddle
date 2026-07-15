// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use heddle_core::QueryReport;

use crate::cli::render::write_stdout;

pub fn query_json(report: &QueryReport) -> Result<()> {
    let mut text = serde_json::to_string(report).context("serialize query output")?;
    text.push('\n');
    write_stdout(&text)
}

pub fn query_text(report: &QueryReport) -> Result<()> {
    let mut text = String::new();
    if report.hits.is_empty() {
        text.push_str("(no matches)\n");
    } else {
        for hit in &report.hits {
            let ts = Utc
                .timestamp_opt(hit.timestamp_secs, 0)
                .single()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| hit.timestamp_secs.to_string());
            text.push_str(&format!(
                "#{} {} {} <{}>",
                hit.seq, ts, hit.verb, hit.actor_email
            ));
            if let Some(thread) = &hit.thread {
                text.push_str(&format!(" thread={thread}"));
            }
            if let Some(state_id) = &hit.state_id {
                text.push_str(&format!(" -> {state_id}"));
            }
            text.push('\n');
        }
    }
    write_stdout(&text)
}
