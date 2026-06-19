// SPDX-License-Identifier: Apache-2.0
//! Transaction replay — startup-time crash recovery for the on-disk
//! transaction sentinel directory.
//!
//! The local `TransactionService` records every in-flight transaction as a
//! TOML sentinel at `<heddle_dir>/state/transactions/<id>.toml` (see
//! `grpc_local_impl::transaction`). The sentinel is a state machine:
//!
//! ```text
//!                  begin                    commit
//!     NoSentinel ─────────► Active(buffered_ops) ─────────► Committed
//!                                │
//!                                │ abort
//!                                ▼
//!                             Aborted
//! ```
//!
//! Sentinels are persisted via `objects::fs_atomic::write_file_atomic`,
//! whose protocol is `write tmp → fsync tmp → rename tmp → fsync parent`.
//! `rename` is the atomic step. After a crash (`kill -9`, host loss,
//! power cut) the on-disk shape is exactly one of:
//!
//! 1. Target file at its old state (rename never observed).
//! 2. Target file at its new state (rename committed).
//! 3. An orphan `.<name>.tmp-<pid>-<nanos>-<counter>` left in the
//!    sentinel directory because the tmp write never reached rename.
//!
//! A partially-written target is impossible by construction.
//!
//! The replay protocol's job is to take a sentinel directory that may
//! contain stuck `active` transactions from a prior process and arrive at
//! a consistent, terminal state with no orphan tmp files. Today the
//! conservative shipping policy is to abort every stuck `active`
//! transaction with reason `"recovered from crash on startup"` rather
//! than auto-replay its `buffered_ops` — the CLI verbs already executed
//! their side-effects as they ran, and the sentinel's `buffered_ops`
//! list is forensic metadata, not a redo log.
//!
//! This module is the only consumer of the sentinel format that lives
//! outside of `grpc_local_impl::transaction`. Field names must stay in
//! sync with the writer; the matching `Serialize`/`Deserialize` impl
//! enforces that at compile time once the field set is consulted.

use std::path::PathBuf;

use objects::{fs_atomic::write_file_atomic, object::TransactionId};
use oplog::OpRecord;
use repo::Repository;
use serde::{Deserialize, Serialize};

/// Sentinel field names mirror
/// `grpc_local_impl::transaction::TransactionSentinel`. Keep them in
/// lockstep; a mismatch on either side produces a parse error and the
/// sentinel falls through to `unparseable_sentinels` instead of being
/// recovered.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplaySentinel {
    transaction_id: String,
    repo_path: String,
    thread: String,
    message: String,
    state: String,
    started_at_secs: i64,
    started_by_email: String,
    base_state: String,
    #[serde(default)]
    buffered_ops: Vec<String>,
    #[serde(default)]
    aborted_reason: Option<String>,
}

const STATE_ACTIVE: &str = "active";
const STATE_ABORTED: &str = "aborted";

/// Reason recorded on every sentinel that replay flipped from `active`
/// to `aborted`. Stable so the audit trail (sentinel `aborted_reason`,
/// oplog `TransactionAbort.reason`) can be grepped post-hoc.
pub const REPLAY_RECOVERY_REASON: &str = "recovered from crash on startup";

/// Outcome of a single replay pass over the sentinel directory.
///
/// Success and partial-failure modes are split into distinct fields so
/// callers (and operators reading the startup log) can tell a clean
/// recovery from one that left work undone.
#[derive(Debug, Default, Clone)]
pub struct ReplayReport {
    /// Transaction ids that were in state `active` and have been
    /// transitioned to `aborted` with reason
    /// [`REPLAY_RECOVERY_REASON`].
    pub recovered_transaction_ids: Vec<String>,
    /// Number of `.<name>.tmp-*` files removed from the sentinel
    /// directory (orphans of an interrupted [`write_file_atomic`]).
    pub orphan_temp_files_removed: usize,
    /// Orphan `.<name>.tmp-*` paths whose `remove_file` returned an
    /// error (read-only filesystem, permission-restricted directory,
    /// disk I/O). They remain on disk; the next startup will retry.
    /// Surface these so callers do not treat the pass as clean when
    /// orphan cleanup actually stalled.
    pub failed_orphan_deletes: Vec<PathBuf>,
    /// Sentinel paths that could not be read or parsed as TOML.
    /// Replay leaves them alone — they are not classified as
    /// recoverable until an operator inspects them.
    pub unparseable_sentinels: Vec<PathBuf>,
    /// Sentinels that were `active` on disk but whose rewrite to
    /// `aborted` failed (e.g. read-only filesystem, disk full). They
    /// remain `active` on disk; the next startup will retry. Surface
    /// these so callers do not treat the pass as clean when crash
    /// recovery actually stalled.
    pub failed_sentinel_writes: Vec<PathBuf>,
    /// Transaction ids whose sentinel rewrite to `aborted` succeeded
    /// but whose `OpRecord::TransactionAbort` append to the oplog
    /// failed. The recovery itself completed; the audit-trail entry
    /// for those transactions was lost. Subsequent replay passes will
    /// skip the now-`aborted` sentinel, so this is the only signal
    /// callers get that the audit event is missing.
    pub failed_oplog_appends: Vec<String>,
    /// `Some(err)` when `read_dir` on the sentinel directory failed
    /// for a reason other than the directory being missing
    /// (permission denied, I/O error, path is not a directory). When
    /// set, no scan ran and the other fields are uninformative.
    pub scan_error: Option<String>,
    /// Count of directory entries that `read_dir` yielded as `Err`
    /// (transient I/O / permission issues mid-iteration). Each one
    /// might have been a recoverable `active` sentinel; surfacing the
    /// count prevents a silent clean-pass when entries cannot be
    /// examined at all.
    pub unreadable_entries: usize,
}

impl ReplayReport {
    /// `true` when the pass had nothing to do — every sentinel was
    /// already terminal, no orphan tmp files were removed, and no
    /// recoverable failure modes were observed. Useful for
    /// idempotence assertions in tests.
    pub fn is_clean(&self) -> bool {
        self.recovered_transaction_ids.is_empty()
            && self.orphan_temp_files_removed == 0
            && self.failed_orphan_deletes.is_empty()
            && self.unparseable_sentinels.is_empty()
            && self.failed_sentinel_writes.is_empty()
            && self.failed_oplog_appends.is_empty()
            && self.scan_error.is_none()
            && self.unreadable_entries == 0
    }

    /// `true` when the pass hit a hard-failure condition that
    /// operators must triage immediately: either the directory scan
    /// itself never ran ([`Self::scan_error`]) or a recovered
    /// transaction's audit-trail oplog append was lost
    /// ([`Self::failed_oplog_appends`]). Both are non-retryable on
    /// the next startup.
    pub fn has_hard_failures(&self) -> bool {
        self.scan_error.is_some() || !self.failed_oplog_appends.is_empty()
    }

    /// `true` when the pass hit a recoverable failure that left
    /// on-disk state for the next startup to retry or for an
    /// operator to inspect: a failed sentinel rewrite, a failed
    /// orphan-tmp delete, an unparseable sentinel, or a directory
    /// entry that `read_dir` could not yield.
    pub fn has_recoverable_failures(&self) -> bool {
        !self.failed_sentinel_writes.is_empty()
            || !self.failed_orphan_deletes.is_empty()
            || !self.unparseable_sentinels.is_empty()
            || self.unreadable_entries > 0
    }
}

/// Returns `true` when `name` looks like an orphan from
/// [`write_file_atomic`]. The temp-path convention is documented in
/// `objects::fs_atomic::temp_path`:
///
/// ```text
/// parent/.{file_name}.tmp-{pid}-{nanos}-{counter}
/// ```
///
/// Replay needs a stricter predicate than "starts with a dot" — the
/// directory could in theory hold other dotfiles. We anchor on the
/// literal `.tmp-` infix because `temp_path` always emits it and no
/// committed sentinel name would.
fn is_orphan_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp-")
}

/// Scan the sentinel directory and reach a terminal, consistent state.
///
/// For every parseable sentinel still in state `active`:
/// 1. Rewrite the sentinel in-place with `state = "aborted"`,
///    `aborted_reason = Some(REPLAY_RECOVERY_REASON)`, and an empty
///    `buffered_ops` list.
/// 2. Append `OpRecord::TransactionAbort` to the repository's oplog
///    so the recovered abort surfaces in `heddle log`.
///
/// For every orphan `.<name>.tmp-*` file: remove it.
///
/// Idempotent: calling twice in a row leaves [`ReplayReport::is_clean`]
/// returning `true` on the second call.
///
/// Per-sentinel errors are tracing-warned and surfaced in the returned
/// report (under `unparseable_sentinels`, `failed_sentinel_writes`,
/// `failed_oplog_appends`, or `failed_orphan_deletes` depending on
/// which step failed) so a single corrupt sentinel or undeletable
/// orphan cannot block daemon startup but its failure is still visible
/// to the operator.
pub fn replay_active_transactions(repo: &Repository) -> ReplayReport {
    let mut report = ReplayReport::default();
    let dir = repo.heddle_dir().join("state").join("transactions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return report,
        Err(err) => {
            tracing::warn!(error = %err, dir = %dir.display(),
                "transaction-replay: failed to read sentinel directory");
            report.scan_error = Some(err.to_string());
            return report;
        }
    };

    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, dir = %dir.display(),
                    "transaction-replay: failed to read directory entry");
                report.unreadable_entries += 1;
                continue;
            }
        };
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if is_orphan_temp_name(name) {
            match std::fs::remove_file(&path) {
                Ok(()) => report.orphan_temp_files_removed += 1,
                Err(err) => {
                    tracing::warn!(error = %err, path = %path.display(),
                        "transaction-replay: failed to remove orphan temp file");
                    report.failed_orphan_deletes.push(path.clone());
                }
            }
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                report.unparseable_sentinels.push(path.clone());
                continue;
            }
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => {
                report.unparseable_sentinels.push(path.clone());
                continue;
            }
        };
        let mut sentinel: ReplaySentinel = match toml::from_str(text) {
            Ok(s) => s,
            Err(_) => {
                report.unparseable_sentinels.push(path.clone());
                continue;
            }
        };

        if sentinel.state != STATE_ACTIVE {
            continue;
        }

        let txn_id = sentinel.transaction_id.clone();
        sentinel.state = STATE_ABORTED.to_string();
        sentinel.aborted_reason = Some(REPLAY_RECOVERY_REASON.to_string());
        sentinel.buffered_ops.clear();

        let serialized = match toml::to_string_pretty(&sentinel) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, txn = %txn_id,
                    "transaction-replay: failed to serialize recovered sentinel");
                report.failed_sentinel_writes.push(path.clone());
                continue;
            }
        };
        if let Err(err) = write_file_atomic(&path, serialized.as_bytes()) {
            tracing::warn!(error = %err, txn = %txn_id,
                "transaction-replay: failed to persist recovered sentinel");
            report.failed_sentinel_writes.push(path.clone());
            continue;
        }
        // Sentinel is now `aborted` on disk; from here the recovery
        // itself has succeeded. An oplog append failure only loses the
        // audit-trail entry — track it on the report rather than
        // pretending the transaction is unrecovered.
        if let Err(err) = repo.oplog().record_batch(vec![OpRecord::TransactionAbort {
            transaction_id: TransactionId::new(txn_id.clone()),
            reason: REPLAY_RECOVERY_REASON.to_string(),
        }]) {
            tracing::warn!(error = %err, txn = %txn_id,
                "transaction-replay: failed to record TransactionAbort");
            report.failed_oplog_appends.push(txn_id.clone());
        }
        report.recovered_transaction_ids.push(txn_id);
    }

    report
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use oplog::LocalOpLogBackend;
    use proptest::prelude::*;
    use repo::Repository;
    use tempfile::TempDir;

    use super::*;

    /// Fixed-shape sentinel writer, used to seed the on-disk state
    /// without going through the gRPC service. Mirrors the writer in
    /// `grpc_local_impl::transaction` byte-for-byte (TOML key order
    /// matches `toml::to_string_pretty` output for `TransactionSentinel`).
    fn write_sentinel_raw(dir: &Path, id: &str, state: &str, buffered_ops: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        let buffered = if buffered_ops.is_empty() {
            "[]".to_string()
        } else {
            let inner = buffered_ops
                .iter()
                .map(|op| format!("\"{op}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        };
        let body = format!(
            r#"transaction_id = "{id}"
repo_path = ""
thread = ""
message = ""
state = "{state}"
started_at_secs = 0
started_by_email = ""
base_state = ""
buffered_ops = {buffered}
"#
        );
        fs::write(dir.join(format!("{id}.toml")), body).unwrap();
    }

    fn fresh_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn sentinel_dir(repo: &Repository) -> PathBuf {
        repo.heddle_dir().join("state").join("transactions")
    }

    fn read_state(dir: &Path, id: &str) -> String {
        let body = fs::read_to_string(dir.join(format!("{id}.toml"))).unwrap();
        let sentinel: ReplaySentinel = toml::from_str(&body).unwrap();
        sentinel.state
    }

    fn read_reason(dir: &Path, id: &str) -> Option<String> {
        let body = fs::read_to_string(dir.join(format!("{id}.toml"))).unwrap();
        let sentinel: ReplaySentinel = toml::from_str(&body).unwrap();
        sentinel.aborted_reason
    }

    fn count_orphan_tmps(dir: &Path) -> usize {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        entries
            .flatten()
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(is_orphan_temp_name)
                    .unwrap_or(false)
            })
            .count()
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn empty_directory_is_a_no_op() {
        let (_t, repo) = fresh_repo();
        let report = replay_active_transactions(&repo);
        assert!(report.is_clean());
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn aborts_active_sentinel_from_prior_run() {
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        write_sentinel_raw(&dir, "tx-stuck", "active", &["capture", "merge"]);

        let report = replay_active_transactions(&repo);
        assert_eq!(
            report.recovered_transaction_ids,
            vec!["tx-stuck".to_string()]
        );
        assert_eq!(report.orphan_temp_files_removed, 0);
        assert!(report.unparseable_sentinels.is_empty());

        // Sentinel: flipped to aborted, reason set, buffered_ops drained.
        assert_eq!(read_state(&dir, "tx-stuck"), STATE_ABORTED);
        assert_eq!(
            read_reason(&dir, "tx-stuck").as_deref(),
            Some(REPLAY_RECOVERY_REASON)
        );

        // Oplog: tail carries a TransactionAbort with the recovery reason.
        let tail = repo.oplog().recent(64).unwrap();
        let last = tail.last().expect("oplog non-empty");
        match &last.operation {
            OpRecord::TransactionAbort {
                transaction_id,
                reason,
            } => {
                assert_eq!(transaction_id.as_str(), "tx-stuck");
                assert_eq!(reason, REPLAY_RECOVERY_REASON);
            }
            other => panic!("expected TransactionAbort, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn leaves_terminal_sentinels_alone() {
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        write_sentinel_raw(&dir, "tx-committed", "committed", &[]);
        write_sentinel_raw(&dir, "tx-aborted", "aborted", &[]);

        let before_oplog = repo.oplog().recent(64).unwrap().len();
        let report = replay_active_transactions(&repo);

        assert!(report.is_clean());
        assert_eq!(read_state(&dir, "tx-committed"), "committed");
        assert_eq!(read_state(&dir, "tx-aborted"), "aborted");
        // No oplog entry should have been appended for terminal sentinels.
        assert_eq!(repo.oplog().recent(64).unwrap().len(), before_oplog);
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn removes_orphan_temp_files() {
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        fs::create_dir_all(&dir).unwrap();
        // Three orphans with the canonical temp-path shape from
        // `objects::fs_atomic::temp_path`.
        fs::write(dir.join(".tx-a.toml.tmp-100-200-1"), b"partial").unwrap();
        fs::write(dir.join(".tx-b.toml.tmp-100-200-2"), b"partial").unwrap();
        fs::write(dir.join(".tx-c.toml.tmp-999-1234-5"), b"").unwrap();

        let report = replay_active_transactions(&repo);
        assert_eq!(report.orphan_temp_files_removed, 3);
        assert_eq!(count_orphan_tmps(&dir), 0);
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn is_idempotent() {
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        write_sentinel_raw(&dir, "tx-1", "active", &["capture"]);
        fs::write(dir.join(".tx-1.toml.tmp-1-1-1"), b"x").unwrap();

        let first = replay_active_transactions(&repo);
        assert_eq!(first.recovered_transaction_ids.len(), 1);
        assert_eq!(first.orphan_temp_files_removed, 1);

        let second = replay_active_transactions(&repo);
        assert!(
            second.is_clean(),
            "second pass should be a no-op (was {second:?})"
        );
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn scan_error_set_when_sentinel_dir_is_not_a_directory() {
        // read_dir on a non-directory path returns a non-NotFound
        // error. Replay must surface it on the report rather than
        // returning a clean default that looks like a no-op.
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        // Plant a regular file where the transactions/ directory
        // would be — read_dir will yield ErrorKind::NotADirectory.
        fs::write(&dir, b"not a directory").unwrap();

        let report = replay_active_transactions(&repo);
        assert!(
            report.scan_error.is_some(),
            "expected scan_error to be set, got {report:?}"
        );
        assert!(!report.is_clean());
        assert!(report.recovered_transaction_ids.is_empty());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(process_global)]
    fn failed_sentinel_write_surfaced_on_readonly_dir() {
        // Read-only sentinel directory: the sentinel parses fine but
        // write_file_atomic cannot create its tmp file. Replay must
        // surface the failure path on the report instead of treating
        // the pass as clean — the transaction is still `active` on
        // disk and crash recovery has actually stalled.
        use std::os::unix::fs::PermissionsExt;

        // Root bypasses DAC permission checks, so r-x on the dir
        // would not actually block tmp-file creation and the test's
        // assertion would be a no-op. Skip rather than mislead.
        // SAFETY: getuid() is always safe.
        if unsafe { libc::getuid() } == 0 {
            eprintln!(
                "skipping failed_sentinel_write_surfaced_on_readonly_dir: \
                 running as root, DAC checks bypassed"
            );
            return;
        }

        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        write_sentinel_raw(&dir, "tx-ro", "active", &[]);
        let path = dir.join("tx-ro.toml");

        // Make the parent r-x: existing files still readable, but no
        // new files can be created (so write_file_atomic's tmp-create
        // step fails).
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        let original = perms.mode();
        perms.set_mode(0o555);
        fs::set_permissions(&dir, perms).unwrap();

        let report = replay_active_transactions(&repo);

        // Restore writable perms so TempDir can clean up.
        let mut restore = fs::metadata(&dir).unwrap().permissions();
        restore.set_mode(original);
        fs::set_permissions(&dir, restore).unwrap();

        assert_eq!(report.failed_sentinel_writes, vec![path]);
        // The sentinel write failed, so the recovery did not complete
        // and the txn id must NOT appear in recovered_transaction_ids.
        assert!(report.recovered_transaction_ids.is_empty());
        assert!(!report.is_clean());
        // Sentinel is still in `active` on disk so the next startup
        // retries.
        assert_eq!(read_state(&dir, "tx-ro"), STATE_ACTIVE);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(process_global)]
    fn failed_orphan_delete_surfaced_on_readonly_dir() {
        // Read-only sentinel directory: the orphan tmp file is
        // present and matches `is_orphan_temp_name`, but
        // `remove_file` cannot unlink it because the parent denies
        // write. Replay must surface the failure path on the report
        // instead of silently incrementing nothing — the orphan is
        // still on disk and the operator needs to know.
        use std::os::unix::fs::PermissionsExt;

        // Root bypasses DAC permission checks, so r-x on the dir
        // would not actually block file deletion and the test's
        // assertion would be a no-op. Skip rather than mislead.
        // SAFETY: getuid() is always safe.
        if unsafe { libc::getuid() } == 0 {
            eprintln!(
                "skipping failed_orphan_delete_surfaced_on_readonly_dir: \
                 running as root, DAC checks bypassed"
            );
            return;
        }

        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join(".tx-stuck.toml.tmp-1-2-3");
        fs::write(&orphan, b"partial").unwrap();

        // Make the parent r-x: existing files still readable, but no
        // new files can be created AND existing files cannot be
        // unlinked (unlink requires write+execute on the directory).
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        let original = perms.mode();
        perms.set_mode(0o555);
        fs::set_permissions(&dir, perms).unwrap();

        let report = replay_active_transactions(&repo);

        // Restore writable perms so TempDir can clean up.
        let mut restore = fs::metadata(&dir).unwrap().permissions();
        restore.set_mode(original);
        fs::set_permissions(&dir, restore).unwrap();

        assert_eq!(report.failed_orphan_deletes, vec![orphan.clone()]);
        // The delete failed, so the removed-count must NOT be
        // incremented.
        assert_eq!(report.orphan_temp_files_removed, 0);
        assert!(!report.is_clean());
        assert!(report.has_recoverable_failures());
        assert!(!report.has_hard_failures());
        // Orphan is still on disk so the next startup retries.
        assert!(orphan.exists());
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn leaves_unparseable_sentinels_in_place() {
        let (_t, repo) = fresh_repo();
        let dir = sentinel_dir(&repo);
        fs::create_dir_all(&dir).unwrap();
        // A `.toml` file that is not a valid sentinel — replay
        // surfaces it in the report and does NOT delete it.
        let bad = dir.join("tx-garbage.toml");
        fs::write(&bad, b"not = valid toml = oops").unwrap();

        let report = replay_active_transactions(&repo);
        assert!(report.recovered_transaction_ids.is_empty());
        assert_eq!(report.unparseable_sentinels, vec![bad.clone()]);
        // File preserved for an operator to inspect.
        assert!(bad.exists());
    }

    /// Crash matrix model. Each variant corresponds to a distinct
    /// point at which an interrupted `write_file_atomic` could leave
    /// on-disk state for a sentinel that was being written.
    #[derive(Debug, Clone)]
    enum CrashKind {
        /// Sentinel pre-existed in state `active`; a subsequent
        /// `append`/`commit`/`abort` crashed mid-tmp-write. Target
        /// file is at its old `active` content; an orphan tmp file
        /// holds the partial bytes.
        DuringTmpWrite,
        /// Sentinel pre-existed; tmp write completed and fsync'd but
        /// the rename never executed. Target is still at the old
        /// content; an orphan tmp holds the *complete* new content.
        BeforeRename,
        /// Rename executed; only the parent fsync did not. After
        /// kill -9 the new content is in the page cache so reads
        /// see it. Models a clean-but-not-durable transition.
        AfterRename,
        /// No crash — the sentinel is just sitting at `active`
        /// because the prior process exited normally without
        /// reaching commit/abort. (e.g. the user closed their
        /// laptop with a transaction open.)
        NoCrash,
    }

    fn arb_crash_kind() -> impl Strategy<Value = CrashKind> {
        prop_oneof![
            Just(CrashKind::DuringTmpWrite),
            Just(CrashKind::BeforeRename),
            Just(CrashKind::AfterRename),
            Just(CrashKind::NoCrash),
        ]
    }

    fn arb_ops() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[a-z]{1,8}", 0..6)
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        })]

        /// For any combination of crash phase + buffered op sequence,
        /// running replay must produce:
        /// - every sentinel in a terminal state (committed | aborted),
        /// - zero orphan `.tmp-*` files in the sentinel directory,
        /// - one `TransactionAbort` oplog entry per recovered txn id,
        /// - an idempotent second call (no-op).
        ///
        /// This is the crash-matrix property: write/sync/rename of the
        /// sentinel file is the only mutation under transaction
        /// control, and `write_file_atomic`'s tmp+rename protocol means
        /// any partial state collapses into one of the four cases
        /// modelled above.
        #[test]
        #[serial_test::serial(process_global)]
        fn crash_matrix_replay_reaches_consistent_terminal_state(
            crash in arb_crash_kind(),
            ops in arb_ops(),
        ) {
            let temp = TempDir::new().unwrap();
            let repo = Repository::init_default(temp.path()).unwrap();
            let dir = sentinel_dir(&repo);
            fs::create_dir_all(&dir).unwrap();

            let txn_id = "tx-crashed";
            let op_refs: Vec<&str> = ops.iter().map(|s| s.as_str()).collect();

            // Seed the directory with the on-disk shape for this
            // crash kind. All four kinds end with a parseable
            // sentinel in state `active`; the tmp-file presence
            // varies.
            match crash {
                CrashKind::DuringTmpWrite => {
                    write_sentinel_raw(&dir, txn_id, "active", &op_refs);
                    fs::write(
                        dir.join(format!(".{txn_id}.toml.tmp-1-2-3")),
                        b"partial bytes\n",
                    )
                    .unwrap();
                }
                CrashKind::BeforeRename => {
                    write_sentinel_raw(&dir, txn_id, "active", &op_refs);
                    // The tmp file holds the *complete* would-be new
                    // sentinel content. Replay must not adopt it; it
                    // is a leftover from a write that never
                    // linearized.
                    let pretend_new = r#"transaction_id = "tx-crashed"
repo_path = ""
thread = ""
message = ""
state = "committed"
started_at_secs = 0
started_by_email = ""
base_state = ""
buffered_ops = []
"#;
                    fs::write(
                        dir.join(format!(".{txn_id}.toml.tmp-4-5-6")),
                        pretend_new,
                    )
                    .unwrap();
                }
                CrashKind::AfterRename => {
                    // Rename committed; sentinel is at the new
                    // (still `active`) content; no tmp left over.
                    write_sentinel_raw(&dir, txn_id, "active", &op_refs);
                }
                CrashKind::NoCrash => {
                    write_sentinel_raw(&dir, txn_id, "active", &op_refs);
                }
            }

            // Run replay.
            let report = replay_active_transactions(&repo);

            // Every recovered txn id should be exactly the one we seeded.
            prop_assert_eq!(
                report.recovered_transaction_ids.clone(),
                vec![txn_id.to_string()]
            );
            // No orphan tmp files left behind.
            prop_assert_eq!(count_orphan_tmps(&dir), 0);
            // Sentinel reached the aborted terminal state with the
            // recovery reason.
            prop_assert_eq!(read_state(&dir, txn_id), STATE_ABORTED);
            let recovered_reason = read_reason(&dir, txn_id);
            prop_assert_eq!(
                recovered_reason.as_deref(),
                Some(REPLAY_RECOVERY_REASON)
            );
            // Oplog tail carries the abort entry.
            let tail = repo.oplog().recent(64).unwrap();
            let last = tail.last().expect("oplog non-empty after recovery");
            match &last.operation {
                OpRecord::TransactionAbort {
                    transaction_id,
                    reason,
                } => {
                    prop_assert_eq!(transaction_id.as_str(), txn_id);
                    prop_assert_eq!(reason, REPLAY_RECOVERY_REASON);
                }
                _ => prop_assert!(false, "expected TransactionAbort at oplog tail"),
            }

            // Second pass is a no-op.
            let again = replay_active_transactions(&repo);
            prop_assert!(again.is_clean(), "second pass not clean: {:?}", again);
        }
    }
}
