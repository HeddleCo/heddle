//! Durable exact signed review requests, scoped by endpoint and author.
use std::{fs::OpenOptions, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::{RecordReviewRequest, ReviewDecision};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_PENDING: i64 = 512;
const MAX_COMPLETED: i64 = 65_536;

pub(super) enum StoredReview {
    Pending(RecordReviewRequest),
    Completed(ReviewDecision),
}

pub(super) struct ReviewOutbox {
    connection: Connection,
}

impl ReviewOutbox {
    pub(super) fn open() -> Result<Self> {
        let directory = repo::identity::heddle_home_dir().join("state");
        Self::open_at(directory)
    }

    fn open_at(directory: PathBuf) -> Result<Self> {
        objects::fs_atomic::create_private_dir_all(&directory)
            .context("create private review command state")?;
        let path: PathBuf = directory.join("review-outbox.sqlite3");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create private review outbox"),
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.file_type().is_file(),
            "review outbox must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "review outbox must not be readable by other users"
            );
        }
        let connection = Connection::open(path).context("open review outbox")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        ensure!(mode == "wal", "review outbox requires durable WAL mode");
        connection.execute_batch(
            "PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS prepared_review (
               endpoint BLOB NOT NULL CHECK(length(endpoint)=32),
               principal TEXT NOT NULL,
               operation_id TEXT NOT NULL,
               request BLOB NOT NULL CHECK(length(request)<=262144),
               PRIMARY KEY(endpoint,principal,operation_id)
             );
             CREATE TABLE IF NOT EXISTS completed_review (
               endpoint BLOB NOT NULL CHECK(length(endpoint)=32),
               principal TEXT NOT NULL,
               operation_id TEXT NOT NULL,
               decision BLOB NOT NULL CHECK(length(decision)<=262144),
               PRIMARY KEY(endpoint,principal,operation_id)
             );",
        )?;
        Ok(Self { connection })
    }

    pub(super) fn load(
        &self,
        endpoint: &[u8],
        principal: &str,
        operation_id: uuid::Uuid,
    ) -> Result<Option<StoredReview>> {
        let completed: Option<Vec<u8>> = self.connection.query_row(
            "SELECT decision FROM completed_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        ).optional()?;
        if let Some(bytes) = completed {
            ensure!(bytes.len() <= MAX_REQUEST_BYTES, "stored completed review exceeds bound");
            let decision = ReviewDecision::decode(bytes.as_slice())?;
            ensure!(decision.encode_to_vec() == bytes, "stored completed review is not canonical protobuf");
            return Ok(Some(StoredReview::Completed(decision)));
        }
        let bytes: Option<Vec<u8>> = self.connection.query_row(
            "SELECT request FROM prepared_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        ).optional()?;
        bytes
            .map(|bytes| {
                ensure!(
                    bytes.len() <= MAX_REQUEST_BYTES,
                    "stored review exceeds bound"
                );
                let request = RecordReviewRequest::decode(bytes.as_slice())?;
                ensure!(
                    request.encode_to_vec() == bytes,
                    "stored review is not canonical protobuf"
                );
                ensure!(
                    request.client_operation_id == operation_id.to_string(),
                    "stored review operation ID differs"
                );
                Ok(StoredReview::Pending(request))
            })
            .transpose()
    }

    pub(super) fn save(
        &mut self,
        endpoint: &[u8],
        principal: &str,
        operation_id: uuid::Uuid,
        request: &RecordReviewRequest,
    ) -> Result<()> {
        ensure!(
            endpoint.len() == 32 && !principal.is_empty(),
            "review outbox scope incomplete"
        );
        ensure!(
            request.client_operation_id == operation_id.to_string(),
            "review operation ID differs"
        );
        let bytes = request.encode_to_vec();
        ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "prepared review exceeds bound"
        );
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let completed: Option<i64> = transaction.query_row(
            "SELECT 1 FROM completed_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        ).optional()?;
        ensure!(completed.is_none(), "operation ID already completed; reuse its recorded result");
        let existing: Option<i64> = transaction.query_row(
            "SELECT 1 FROM prepared_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        ).optional()?;
        if existing.is_none() {
            let count: i64 = transaction.query_row("SELECT COUNT(*) FROM prepared_review", [], |row| row.get(0))?;
            ensure!(count < MAX_PENDING, "too many pending signed reviews; retry or reconcile earlier operations");
        }
        transaction.execute(
            "INSERT OR IGNORE INTO prepared_review(endpoint,principal,operation_id,request) VALUES(?1,?2,?3,?4)",
            params![endpoint, principal, operation_id.to_string(), &bytes],
        )?;
        let stored: Vec<u8> = transaction.query_row(
            "SELECT request FROM prepared_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        )?;
        if stored != bytes {
            bail!("operation ID already names a different prepared review")
        }
        transaction.commit()?;
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        endpoint: &[u8],
        principal: &str,
        operation_id: uuid::Uuid,
    ) -> Result<()> {
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bytes: Vec<u8> = transaction.query_row(
            "SELECT request FROM prepared_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
            |row| row.get(0),
        ).context("completed review has no prepared request")?;
        let request = RecordReviewRequest::decode(bytes.as_slice())?;
        let decision = request.decision.context("prepared review has no decision")?;
        let count: i64 = transaction.query_row("SELECT COUNT(*) FROM completed_review", [], |row| row.get(0))?;
        ensure!(count < MAX_COMPLETED, "completed review history is full; retain operation IDs before preparing more reviews");
        transaction.execute(
            "INSERT INTO completed_review(endpoint,principal,operation_id,decision) VALUES(?1,?2,?3,?4)",
            params![endpoint, principal, operation_id.to_string(), decision.encode_to_vec()],
        )?;
        transaction.execute(
            "DELETE FROM prepared_review WHERE endpoint=?1 AND principal=?2 AND operation_id=?3",
            params![endpoint, principal, operation_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_keeps_exact_signed_bytes_when_comparison_changes() {
        let home = tempfile::tempdir().expect("private test home");
        let mut outbox = ReviewOutbox::open_at(home.path().join("state")).expect("outbox");
        let endpoint = [7; 32];
        let principal = uuid::Uuid::from_bytes([8; 16]).to_string();
        let id = uuid::Uuid::from_bytes([9; 16]);
        let first = RecordReviewRequest {
            client_operation_id: id.to_string(),
            decision: Some(api::heddle::api::v2alpha1::ReviewDecision {
                policy_version: vec![1; 32],
                ..Default::default()
            }),
            ..Default::default()
        };
        outbox
            .save(&endpoint, &principal, id, &first)
            .expect("persist before send");
        let changed = RecordReviewRequest {
            decision: Some(api::heddle::api::v2alpha1::ReviewDecision {
                policy_version: vec![2; 32],
                ..Default::default()
            }),
            ..first.clone()
        };
        assert!(
            outbox.save(&endpoint, &principal, id, &changed).is_err(),
            "same operation cannot acquire newly observed comparison"
        );
        let reopened = ReviewOutbox::open_at(home.path().join("state")).expect("restart");
        assert!(matches!(reopened.load(&endpoint, &principal, id).expect("load"), Some(StoredReview::Pending(request)) if request == first));
        assert!(reopened.load(&[6; 32], &principal, id).expect("other endpoint").is_none());
        assert!(reopened.load(&endpoint, "another-principal", id).expect("other actor").is_none());
        let mut reopened = reopened;
        reopened.complete(&endpoint, &principal, id).expect("receipt recorded");
        assert!(matches!(reopened.load(&endpoint, &principal, id).expect("completed"), Some(StoredReview::Completed(decision)) if Some(decision.clone()) == first.decision));
        assert!(reopened.save(&endpoint, &principal, id, &first).is_err(), "completed operation ID cannot be signed again");
        let restarted = ReviewOutbox::open_at(home.path().join("state")).expect("restart after receipt");
        assert!(matches!(restarted.load(&endpoint, &principal, id).expect("completed after restart"), Some(StoredReview::Completed(_))));
    }

    #[test]
    fn pending_bound_rejects_new_command_without_replacing_existing() {
        let home = tempfile::tempdir().expect("private test home");
        let mut outbox = ReviewOutbox::open_at(home.path().join("state")).expect("outbox");
        let tx = outbox.connection.transaction().expect("fill pending set");
        for index in 0..MAX_PENDING {
            tx.execute(
                "INSERT INTO prepared_review(endpoint,principal,operation_id,request) VALUES(?1,?2,?3,?4)",
                params![[7u8; 32].as_slice(), "first", index.to_string(), [1u8].as_slice()],
            ).expect("pending row");
        }
        tx.commit().expect("full set");
        let id = uuid::Uuid::now_v7();
        let request = RecordReviewRequest {
            client_operation_id: id.to_string(),
            ..Default::default()
        };
        assert!(outbox.save(&[8; 32], "second", id, &request).is_err());
        let count: i64 = outbox.connection.query_row(
            "SELECT COUNT(*) FROM prepared_review", [], |row| row.get(0),
        ).expect("unchanged count");
        assert_eq!(count, MAX_PENDING);
    }
}
