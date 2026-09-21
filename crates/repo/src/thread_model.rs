// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::actor_presence::AgentUsageSummary;
pub use objects::thread_record::*;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadRuntimeOverlay {
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub execution_path: Option<PathBuf>,
    #[serde(default)]
    pub materialized_path: Option<PathBuf>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub heddle_session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub native_actor_key: Option<String>,
    #[serde(default)]
    pub native_parent_actor_key: Option<String>,
    #[serde(default)]
    pub probe_source: Option<String>,
    #[serde(default)]
    pub probe_confidence: Option<f32>,
    #[serde(default)]
    pub usage_summary: Option<AgentUsageSummary>,
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub report_flush_state: Option<String>,
    #[serde(default)]
    pub attach_reason: Option<String>,
    #[serde(default)]
    pub thread_mode: Option<ThreadMode>,
    #[serde(default)]
    pub thread_state: Option<ThreadState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub record: ThreadRecord,
    pub runtime: ThreadRuntimeOverlay,
    pub is_current: bool,
    pub is_isolated: bool,
}

impl ThreadView {
    pub fn from_record(
        record: ThreadRecord,
        runtime: ThreadRuntimeOverlay,
        is_current: bool,
    ) -> Self {
        let is_isolated = path_present(runtime.path.as_ref())
            || path_present(runtime.execution_path.as_ref())
            || path_present(runtime.materialized_path.as_ref());
        Self {
            record,
            runtime,
            is_current,
            is_isolated,
        }
    }
}

fn path_present(path: Option<&PathBuf>) -> bool {
    path.is_some_and(|path| !path.as_os_str().is_empty())
}

#[cfg(test)]
mod thread_id_tests {
    use super::*;

    #[test]
    fn accepts_safe_slugs() {
        for ok in [
            "feature/x",
            "v1.2",
            "a_b-c.d",
            "team@scope",
            "main",
            "wip+1=2",
        ] {
            assert!(
                ThreadId::new(ok).is_ok(),
                "expected '{ok}' to be a valid thread id"
            );
        }
    }

    #[test]
    fn rejects_reserved_heddle_namespace() {
        for bad in ["heddle/frontier/main/hc-abc", "Heddle/x", "heddle/notes"] {
            assert!(
                ThreadId::new(bad).is_err(),
                "expected '{bad}' to be rejected as a reserved heddle/ name"
            );
        }
        assert!(
            ThreadId::new("heddle").is_ok(),
            "a bare 'heddle' thread remains a user name"
        );
        assert!(ThreadId::new("main@hd-abc").is_ok());
    }

    #[test]
    fn rejects_whitespace_metachars_traversal_and_empty() {
        for bad in [
            "my feature", // space
            "a;b",        // shell separator
            "a|b",        // pipe
            "a$(x)",      // command substitution
            "a\nb",       // newline
            "a&b",        // background
            "`x`",        // backtick
            "..",         // bare traversal
            "a/../b",     // traversal segment
            "/abs",       // leading slash
            "-foo",       // leading dash — parses as a CLI flag in breadcrumbs
            "--bar",      // leading double-dash
            "",           // empty
        ] {
            assert!(
                ThreadId::new(bad).is_err(),
                "expected '{bad}' to be rejected as an invalid thread id"
            );
        }
    }

    #[test]
    fn error_message_carries_a_valid_rename_hint() {
        let err = ThreadId::new("my feature").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("my feature"),
            "names the offending input: {msg}"
        );
        assert!(msg.contains("try 'my-feature'"), "suggests a rename: {msg}");
        // The suggestion itself must be a valid thread id.
        assert!(ThreadId::new(err.suggestion()).is_ok());
    }

    #[test]
    fn deserialize_trusts_persisted_ids_without_revalidating() {
        // A record written before this rule existed (or by a future version)
        // must still deserialize — validation is at creation, not on read.
        let id: ThreadId = serde_json::from_str("\"legacy id\"").unwrap();
        assert_eq!(id.as_str(), "legacy id");
    }
}
