//! Immutable locally admitted check originals, query indexes, and command receipts.
//! The native codec and current original-author gate run at admission; delivery
//! credentials never replace that original proof. No transaction spans network IO.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use objects::object::{ContentHash, OperationId, StateId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE device_check_records (
      kind INTEGER NOT NULL CHECK(kind IN(1,2)), id TEXT NOT NULL, thread BLOB NOT NULL CHECK(length(thread)=32), revision BLOB NOT NULL CHECK(length(revision)=32),
      original BLOB NOT NULL, version BLOB NOT NULL CHECK(length(version)=32), admitted_at INTEGER NOT NULL,
      PRIMARY KEY(kind,id));
      CREATE INDEX device_check_revision ON device_check_records(thread,revision,kind,id);
      CREATE TABLE device_check_versions(thread BLOB NOT NULL,revision BLOB NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(thread,revision));
      CREATE TRIGGER device_check_version_insert AFTER INSERT ON device_check_records BEGIN
       INSERT INTO device_check_versions VALUES(NEW.thread,NEW.revision,1) ON CONFLICT(thread,revision) DO UPDATE SET generation=generation+1;
      END;
      CREATE TRIGGER device_check_version_delete AFTER DELETE ON device_check_records BEGIN
       INSERT INTO device_check_versions VALUES(OLD.thread,OLD.revision,1) ON CONFLICT(thread,revision) DO UPDATE SET generation=generation+1;
      END;")
}

pub struct Command<'a> {
    pub namespace: &'a str,
    pub id: OperationId,
    pub method: &'a str,
    pub request_hash: [u8; 32],
}
pub struct Original<'a> {
    pub kind: i32,
    pub id: uuid::Uuid,
    pub thread: ContentHash,
    pub revision: StateId,
    pub bytes: &'a [u8],
    pub version: ContentHash,
}

/// Persist verified originals and the exact response in one immediate commit.
/// `validate` checks original authority only when this exact immutable original
/// lacks prior local admission, while always enforcing dependent record rules.
pub fn accept(
    directory: &Path,
    command: Command<'_>,
    original: Original<'_>,
    validate: impl FnOnce(&Connection, bool) -> Result<()>,
    response: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    ensure!(
        !command.namespace.is_empty()
            && command.namespace.len() <= 1024
            && !command.namespace.contains('\0'),
        "authenticated check receipt namespace required"
    );
    ensure!(
        matches!(original.kind, 1 | 2)
            && !original.id.is_nil()
            && !original.bytes.is_empty()
            && original.bytes.len() <= 256 * 1024,
        "check original bounds"
    );
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let prior: Option<(String, Vec<u8>, Vec<u8>, bool)> = tx.query_row("SELECT verb,request_hash,response,pending FROM operation_receipts WHERE namespace=?1 AND operation_id=?2", params![command.namespace, command.id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    if let Some((method, hash, response, pending)) = prior {
        ensure!(
            method == command.method && hash == command.request_hash && !pending,
            "check command ID reused or still executing"
        );
        return Ok(response);
    }
    let retained = get_in(&tx, original.kind, original.id)?;
    if let Some(bytes) = &retained {
        ensure!(
            bytes == original.bytes,
            "immutable check identity already names another original"
        );
    }
    validate(&tx, retained.is_some())?;
    if retained.is_none() {
        tx.execute("INSERT INTO device_check_records(kind,id,revision,original,version,admitted_at,thread) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![original.kind,original.id.to_string(),original.revision.as_bytes(),original.bytes,original.version.as_bytes(),chrono::Utc::now().timestamp(),original.thread.as_bytes()])?;
    }
    let result = response()?;
    ensure!(result.len() <= 1024 * 1024, "check response bound");
    tx.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?1,?2,?3,?4,?5,?6,?7,0)", params![command.namespace,command.id.to_string(),crate::operation_dedup::receipt_record_key(command.namespace,command.id).as_bytes(),command.method,command.request_hash.as_slice(),result,chrono::Utc::now().timestamp()])?;
    tx.commit()?;
    drop(connection);
    objects::fs_atomic::write_file_atomic_secret(
        &directory.join(crate::local_metadata::CHANGE_MARKER_NAME),
        uuid::Uuid::now_v7().as_bytes(),
    )?;
    Ok(result)
}

pub fn get_in(connection: &Connection, kind: i32, id: uuid::Uuid) -> Result<Option<Vec<u8>>> {
    Ok(connection
        .query_row(
            "SELECT original FROM device_check_records WHERE kind=?1 AND id=?2",
            params![kind, id.to_string()],
            |row| row.get(0),
        )
        .optional()?)
}
pub fn get(directory: &Path, kind: i32, id: uuid::Uuid) -> Result<Option<Vec<u8>>> {
    let connection =
        crate::local_metadata::open_existing(directory)?.context("local metadata absent")?;
    get_in(&connection, kind, id)
}
/// Exact revision projection; no file scans or historical operation decoding.
pub fn page(
    directory: &Path,
    thread: ContentHash,
    revision: StateId,
    after: Option<(i32, uuid::Uuid)>,
    limit: usize,
) -> Result<Vec<(i32, uuid::Uuid, Vec<u8>)>> {
    ensure!((1..=257).contains(&limit), "check page bound");
    let connection =
        crate::local_metadata::open_existing(directory)?.context("local metadata absent")?;
    let (kind, id) = after
        .map(|(kind, id)| (kind, id.to_string()))
        .unwrap_or((0, String::new()));
    let mut statement = connection.prepare("SELECT kind,id,original FROM device_check_records WHERE thread=?5 AND revision=?1 AND (kind,id)>(?2,?3) ORDER BY kind,id LIMIT ?4")?;
    let rows = statement.query_map(
        params![
            revision.as_bytes(),
            kind,
            id,
            i64::try_from(limit)?,
            thread.as_bytes()
        ],
        |row| Ok((row.get(0)?, row.get::<_, String>(1)?, row.get(2)?)),
    )?;
    let mut result = Vec::new();
    let mut bytes_used = 0usize;
    for row in rows {
        let (kind, id, bytes): (i32, String, Vec<u8>) = row?;
        bytes_used = bytes_used
            .checked_add(bytes.len())
            .context("evidence page byte overflow")?;
        ensure!(
            bytes_used <= 8 * 1024 * 1024,
            "evidence page byte budget exhausted; request a smaller page"
        );
        result.push((kind, uuid::Uuid::parse_str(&id)?, bytes));
    }
    Ok(result)
}

pub fn verify_original_author(
    authority: &crate::device_authority::DeviceAuthority,
    author: &objects::object::check_evidence::CheckAuthor,
    method: &str,
    path: &str,
    now: i64,
) -> Result<()> {
    let owner = crate::verify_account_owner_observation(&authority.owner, now)?;
    heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
        &author.authority_envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &owner,
            account_uuid: author.actor.principal_id.as_bytes(),
            publisher: &author.publisher,
            agent_id: author.actor.agent_id.as_deref(),
            method,
            spool_path: path,
            now,
        },
        &authority.mint_roots,
        |kind| authority.is_revoked(kind),
    )?;
    Ok(())
}

/// Only current heads (or the original base before capture) contribute. The
/// reverse projection bounds this lookup independently of historical records.
pub fn thread_generation(directory: &Path, thread: ContentHash) -> Result<i64> {
    let connection =
        crate::local_metadata::open_existing(directory)?.context("local metadata absent")?;
    Ok(connection.query_row("SELECT COALESCE(SUM(v.generation),0) FROM device_check_versions v WHERE v.thread=?1 AND v.revision IN (SELECT revision FROM thread_source_head_revisions WHERE thread=?1 UNION SELECT revision FROM thread_source_bases WHERE thread=?1 AND NOT EXISTS(SELECT 1 FROM thread_source_head_revisions WHERE thread=?1))",[thread.as_bytes()],|row|row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_receipt_and_change_cursor_roll_back_together() {
        let temp = tempfile::tempdir().expect("metadata directory");
        let connection = crate::local_metadata::open(temp.path()).expect("fresh schema");
        drop(connection);
        let revision = StateId::from_bytes([1; 32]);
        let id = uuid::Uuid::new_v4();
        let operation = OperationId::new();
        let command = || Command {
            namespace: "owner/agent",
            id: operation,
            method: "RecordEvidence",
            request_hash: [3; 32],
        };
        let original = || Original {
            thread: ContentHash::from_bytes([7; 32]),
            kind: 1,
            id,
            revision,
            bytes: b"verified immutable original",
            version: ContentHash::from_bytes([2; 32]),
        };
        let before =
            crate::operation_dedup::observation::generation(temp.path()).expect("initial cursor");
        assert!(
            accept(
                temp.path(),
                command(),
                original(),
                |_, _| Ok(()),
                || anyhow::bail!("response construction failed")
            )
            .is_err()
        );
        assert!(
            get(temp.path(), 1, id).expect("original lookup").is_none(),
            "failed response must roll back original admission"
        );
        assert_eq!(
            crate::operation_dedup::observation::generation(temp.path()).expect("rollback cursor"),
            before,
            "failed original cannot leave a durable change cursor"
        );
        assert!(
            crate::operation_dedup::observation::page(
                temp.path(),
                "owner/agent",
                &[operation],
                &[],
                None,
                1
            )
            .expect("receipt lookup")
            .is_empty()
        );
        let response = accept(
            temp.path(),
            command(),
            original(),
            |_, admitted| {
                assert!(!admitted);
                Ok(())
            },
            || Ok(vec![7]),
        )
        .expect("committed original");
        assert_eq!(response, vec![7]);
        assert_eq!(
            accept(
                temp.path(),
                command(),
                original(),
                |_, _| anyhow::bail!("replay must not readmit"),
                || anyhow::bail!("replay must not rebuild response")
            )
            .expect("exact replay"),
            response
        );
        assert_eq!(
            get(temp.path(), 1, id)
                .expect("original")
                .expect("admitted"),
            b"verified immutable original"
        );
        assert_eq!(
            page(
                temp.path(),
                ContentHash::from_bytes([7; 32]),
                revision,
                None,
                4
            )
            .expect("origin page")
            .len(),
            1
        );
        assert!(
            page(
                temp.path(),
                ContentHash::from_bytes([8; 32]),
                revision,
                None,
                4
            )
            .expect("same revision in other Thread")
            .is_empty(),
            "same State cannot expose another Thread's authored evidence"
        );
        assert_ne!(
            crate::operation_dedup::observation::generation(temp.path()).expect("commit cursor"),
            before
        );
        assert!(
            temp.path()
                .join(crate::local_metadata::CHANGE_MARKER_NAME)
                .exists()
        );
    }
}
