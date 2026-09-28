// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use verbs::QueryReport;

use crate::cli::{render::write_stdout, style};

pub fn query_json(report: &QueryReport) -> Result<()> {
    let mut text = serde_json::to_string(report).context("serialize query output")?;
    text.push('\n');
    write_stdout(&text)
}

pub fn query_text(report: &QueryReport) -> Result<()> {
    write_stdout(&format_query_text(report))
}

fn format_query_text(report: &QueryReport) -> String {
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
                hit.seq,
                ts,
                style::human_text(&hit.verb),
                style::human_text(&hit.actor_email)
            ));
            if let Some(thread) = &hit.thread {
                text.push_str(&format!(" thread={}", style::thread_label(thread, None)));
            }
            if let Some(state_id) = &hit.state_id {
                text.push_str(&format!(" -> {}", style::human_text(state_id)));
            }
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use verbs::QueryHit;

    use super::*;

    #[test]
    fn text_renderer_consumes_the_typed_query_report() {
        let report = QueryReport {
            output_kind: "query",
            hits: vec![QueryHit {
                seq: 7,
                timestamp_secs: 0,
                verb: "snapshot".to_string(),
                actor_email: "agent@example.com".to_string(),
                operation_id: None,
                thread: Some("agent/facade".to_string()),
                symbols: Vec::new(),
                signal_kinds: Vec::new(),
                state_id: Some("hs-123".to_string()),
            }],
        };

        assert_eq!(
            format_query_text(&report),
            "#7 1970-01-01T00:00:00+00:00 snapshot <agent@example.com> thread=agent/facade -> hs-123\n"
        );
    }

    #[test]
    fn text_renderer_has_a_stable_empty_report() {
        let report = QueryReport {
            output_kind: "query",
            hits: Vec::new(),
        };
        assert_eq!(format_query_text(&report), "(no matches)\n");
    }

    #[test]
    fn text_renderer_hides_full_thread_and_state_ids() {
        let id = "a".repeat(64);
        let report = QueryReport {
            output_kind: "query",
            hits: vec![QueryHit {
                seq: 1,
                timestamp_secs: 0,
                verb: "capture".to_string(),
                actor_email: "agent@example.com".to_string(),
                operation_id: None,
                thread: Some(id.clone()),
                symbols: Vec::new(),
                signal_kinds: Vec::new(),
                state_id: Some(id.clone()),
            }],
        };
        let text = format_query_text(&report);
        assert!(!text.contains(&id), "{text}");
        assert!(serde_json::to_string(&report).unwrap().contains(&id));
    }
}
