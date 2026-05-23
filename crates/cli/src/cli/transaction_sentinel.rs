// SPDX-License-Identifier: Apache-2.0
//! Transaction sentinel detection for the dispatch path.
//!
//! Every state-changing CLI verb should consult [`active_transactions`]
//! before executing. When the answer is non-empty, the verb is running
//! against a repo with one or more open transactions; the verb should
//! either (a) refuse to mutate (the conservative shipping shape), or
//! (b) append itself to the sentinel's `buffered_ops` and defer.
//!
//! This module ships only the *detection* primitive — recording each
//! verb into the sentinel's buffered_ops list (and replaying them at
//! commit) is the larger follow-on the local transaction service
//! already flags as future work. Startup-time crash recovery of stuck
//! `active` sentinels (write/sync/rename crash matrix) is implemented
//! in `daemon::transaction_replay`, which the local daemon invokes
//! before accepting RPCs.
//!
//! Sentinel files live at `<heddle_dir>/state/transactions/<id>.toml`
//! (the same path the local `TransactionService` uses), so the
//! detection here is consistent with the gRPC view.

use std::path::PathBuf;

use repo::Repository;
use serde::{Deserialize, Serialize};

/// One open transaction, hydrated from its on-disk sentinel. Field
/// names match the writer in
/// `crates/daemon/src/grpc_local_impl/transaction.rs::TransactionSentinel`
/// — keep this struct in sync if the writer's shape changes. The
/// `Serialize` impl is used by [`append_op_to_active_for_thread`] to
/// rewrite the sentinel after appending a buffered op.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActiveTransaction {
    pub transaction_id: String,
    pub repo_path: String,
    pub thread: String,
    pub message: String,
    pub state: String,
    pub started_at_secs: i64,
    pub started_by_email: String,
    pub base_state: String,
    /// Verbs the CLI has appended via [`append_op_to_active_for_thread`].
    /// Commit reads this list and emits a single `OpRecord` summary
    /// (the deeper "execute the buffered ops at commit time" routing
    /// is the larger follow-on; today the verbs execute as they
    /// run AND are logged here for audit).
    #[serde(default)]
    pub buffered_ops: Vec<String>,
    #[serde(default)]
    pub aborted_reason: Option<String>,
}

/// Walk the sentinel directory and return every transaction whose
/// state is still `"active"`. Returns an empty vec when the directory
/// is missing (no transactions ever started in this repo).
///
/// Cheap: one stat + N small file reads. Safe to call from hot CLI
/// paths.
pub fn active_transactions(repo: &Repository) -> Vec<ActiveTransaction> {
    let dir = sentinels_dir(repo);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let txn: ActiveTransaction = match toml::from_str(&String::from_utf8_lossy(&bytes)) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if txn.state == "active" {
            out.push(txn);
        }
    }
    out
}

/// Return the active transaction whose `thread` matches `thread_name`,
/// if any. Used by state-changing verbs that target a specific thread:
/// the verb should either route into that transaction's buffer (full
/// wiring is follow-on work) or refuse to proceed (today's shape).
pub fn active_for_thread(repo: &Repository, thread_name: &str) -> Option<ActiveTransaction> {
    active_transactions(repo)
        .into_iter()
        .find(|t| t.thread == thread_name)
}

fn sentinels_dir(repo: &Repository) -> PathBuf {
    repo.heddle_dir().join("state").join("transactions")
}

/// Append `verb_name` to the `buffered_ops` list of every active
/// transaction whose `thread` matches `thread_name`. Returns the
/// transaction ids that were updated. Errors are tracing-warned and
/// swallowed — buffering is best-effort metadata; the verb still
/// executes regardless.
///
/// Callers that only care about "did this verb get buffered into an
/// open transaction?" can use [`buffered_for_thread`], which wraps
/// this and returns a boolean.
///
/// Thread-name matching is exact. Pass `""` for transactions started
/// without a thread context (head-anchored transactions); the empty
/// string matches the sentinel writer's "no thread" sentinel value.
pub fn append_op_to_active_for_thread(
    repo: &Repository,
    thread_name: &str,
    verb_name: &str,
) -> Vec<String> {
    let dir = sentinels_dir(repo);
    let mut updated = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return updated,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut txn: ActiveTransaction = match toml::from_str(&String::from_utf8_lossy(&bytes)) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if txn.state != "active" || txn.thread != thread_name {
            continue;
        }
        txn.buffered_ops.push(verb_name.to_string());
        let serialized = match toml::to_string_pretty(&txn) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, txn = %txn.transaction_id,
                    "transaction-sentinel: failed to serialize after append");
                continue;
            }
        };
        if let Err(err) = std::fs::write(&path, serialized) {
            tracing::warn!(error = %err, txn = %txn.transaction_id,
                "transaction-sentinel: failed to persist appended op");
            continue;
        }
        updated.push(txn.transaction_id);
    }
    updated
}

/// Convenience wrapper for the common "did this verb get buffered?"
/// question. Calls [`append_op_to_active_for_thread`] and returns
/// `true` when at least one open transaction on `thread_name`
/// recorded the verb. Useful for the dispatch path: when buffering
/// occurs, the surrounding verb can short-circuit the actual
/// mutation (the strict "buffer-instead-of-execute" mode the
/// transaction contract calls for).
pub fn buffered_for_thread(repo: &Repository, thread_name: &str, verb_name: &str) -> bool {
    !append_op_to_active_for_thread(repo, thread_name, verb_name).is_empty()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_sentinel(dir: &std::path::Path, id: &str, thread: &str, state: &str) {
        fs::create_dir_all(dir).unwrap();
        let body = format!(
            r#"transaction_id = "{id}"
repo_path = ""
thread = "{thread}"
message = ""
state = "{state}"
started_at_secs = 0
started_by_email = ""
base_state = ""
buffered_ops = []
"#
        );
        fs::write(dir.join(format!("{id}.toml")), body).unwrap();
    }

    fn fresh_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn returns_empty_when_no_sentinels() {
        let (_t, repo) = fresh_repo();
        assert!(active_transactions(&repo).is_empty());
    }

    #[test]
    fn returns_only_active_sentinels() {
        let (_t, repo) = fresh_repo();
        let dir = repo.heddle_dir().join("state").join("transactions");
        write_sentinel(&dir, "tx-active", "main", "active");
        write_sentinel(&dir, "tx-committed", "main", "committed");
        write_sentinel(&dir, "tx-aborted", "feature", "aborted");

        let active = active_transactions(&repo);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].transaction_id, "tx-active");
    }

    #[test]
    fn active_for_thread_filters_by_thread() {
        let (_t, repo) = fresh_repo();
        let dir = repo.heddle_dir().join("state").join("transactions");
        write_sentinel(&dir, "tx-main", "main", "active");
        write_sentinel(&dir, "tx-feat", "feature", "active");

        assert_eq!(
            active_for_thread(&repo, "main").unwrap().transaction_id,
            "tx-main"
        );
        assert_eq!(
            active_for_thread(&repo, "feature").unwrap().transaction_id,
            "tx-feat"
        );
        assert!(active_for_thread(&repo, "unrelated").is_none());
    }

    #[test]
    fn append_op_to_active_for_thread_writes_only_matching_active_sentinels() {
        let (_t, repo) = fresh_repo();
        let dir = repo.heddle_dir().join("state").join("transactions");
        write_sentinel(&dir, "tx-main", "main", "active");
        write_sentinel(&dir, "tx-other-thread", "feature", "active");
        write_sentinel(&dir, "tx-main-aborted", "main", "aborted");

        let updated = append_op_to_active_for_thread(&repo, "main", "capture");
        assert_eq!(updated, vec!["tx-main".to_string()]);

        // The matched sentinel now carries the verb in `buffered_ops`.
        let main_after = active_for_thread(&repo, "main").unwrap();
        assert_eq!(main_after.buffered_ops, vec!["capture".to_string()]);
        // Sibling thread untouched.
        let feature_after = active_for_thread(&repo, "feature").unwrap();
        assert!(feature_after.buffered_ops.is_empty());
    }

    #[test]
    fn append_appends_in_order() {
        let (_t, repo) = fresh_repo();
        let dir = repo.heddle_dir().join("state").join("transactions");
        write_sentinel(&dir, "tx-main", "main", "active");

        append_op_to_active_for_thread(&repo, "main", "capture");
        append_op_to_active_for_thread(&repo, "main", "merge");
        append_op_to_active_for_thread(&repo, "main", "marker");

        let txn = active_for_thread(&repo, "main").unwrap();
        assert_eq!(
            txn.buffered_ops,
            vec![
                "capture".to_string(),
                "merge".to_string(),
                "marker".to_string()
            ]
        );
    }
}
