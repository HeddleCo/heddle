//! Durable execution state shares the command receipt transaction and caller namespace.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::{OperationRecord, operation_record::State};
use objects::object::{ContentHash, OperationId};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE device_operations (
      namespace TEXT NOT NULL, record_id BLOB NOT NULL CHECK(length(record_id)=32),
      record BLOB NOT NULL, state INTEGER NOT NULL, executor TEXT NOT NULL,
      PRIMARY KEY(namespace,record_id));
      CREATE INDEX device_operations_executor ON device_operations(executor,state,namespace,record_id);")
}
pub struct Command<'a> {
    pub namespace: &'a str,
    pub id: OperationId,
    pub method: &'a str,
    pub request_hash: [u8; 32],
}
pub struct Started {
    pub response: Vec<u8>,
    pub fresh: bool,
}

/// The queued execution and recoverable acceptance receipt appear together.
pub fn start(
    directory: &Path,
    command: Command<'_>,
    executor: &str,
    mut record: OperationRecord,
    response: Vec<u8>,
) -> Result<Started> {
    ensure!(
        !command.namespace.is_empty() && command.namespace.len() <= 1024 && !executor.is_empty(),
        "authenticated executor namespace required"
    );
    ensure!(
        record.state == State::Queued as i32 && response.len() <= 1024 * 1024,
        "queued operation required"
    );
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(prior) = replay(&tx, &command)? {
        return Ok(Started {
            response: prior,
            fresh: false,
        });
    }
    let key = crate::operation_dedup::receipt_record_key(command.namespace, command.id);
    ensure!(
        record
            .r#ref
            .as_ref()
            .is_some_and(|reference| reference.id == key.to_string()),
        "operation record identity mismatch"
    );
    set_version(&mut record);
    tx.execute("INSERT INTO device_operations(namespace,record_id,record,state,executor) VALUES(?1,?2,?3,?4,?5)",params![command.namespace,key.as_bytes(),record.encode_to_vec(),record.state,executor])?;
    receipt(&tx, &command, &response)?;
    tx.commit()?;
    drop(connection);
    wake(directory)?;
    Ok(Started {
        response,
        fresh: true,
    })
}
pub fn replay_response(directory: &Path, command: &Command<'_>) -> Result<Option<Vec<u8>>> {
    let connection = crate::local_metadata::open_existing(directory)?.context("metadata absent")?;
    replay(&connection, command)
}
pub(crate) fn replay(connection: &Connection, command: &Command<'_>) -> Result<Option<Vec<u8>>> {
    let prior: Option<(String,Vec<u8>,Vec<u8>,bool)> = connection.query_row("SELECT verb,request_hash,response,pending FROM operation_receipts WHERE namespace=?1 AND operation_id=?2",params![command.namespace,command.id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    match prior {
        Some((method, hash, response, pending)) => {
            ensure!(
                method == command.method && hash == command.request_hash && !pending,
                "operation command ID reused"
            );
            Ok(Some(response))
        }
        None => Ok(None),
    }
}
pub(crate) fn receipt(connection: &Connection, command: &Command<'_>, response: &[u8]) -> Result<()> {
    connection.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",params![command.namespace,command.id.to_string(),crate::operation_dedup::receipt_record_key(command.namespace,command.id).as_bytes(),command.method,command.request_hash.as_slice(),response,chrono::Utc::now().timestamp()])?;
    Ok(())
}
fn load(
    connection: &Connection,
    namespace: &str,
    key: ContentHash,
) -> Result<Option<OperationRecord>> {
    let bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT record FROM device_operations WHERE namespace=?1 AND record_id=?2",
            params![namespace, key.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| OperationRecord::decode(bytes.as_slice()).map_err(Into::into))
        .transpose()
}
pub fn get(directory: &Path, namespace: &str, key: ContentHash) -> Result<Option<OperationRecord>> {
    let connection = crate::local_metadata::open_existing(directory)?.context("metadata absent")?;
    load(&connection, namespace, key)
}
fn set_version(record: &mut OperationRecord) {
    record.version.clear();
    record.version =
        ContentHash::compute_typed("heddle-device-execution-v2", &record.encode_to_vec())
            .as_bytes()
            .to_vec();
}
fn save(
    connection: &Connection,
    namespace: &str,
    key: ContentHash,
    record: &mut OperationRecord,
) -> Result<()> {
    set_version(record);
    ensure!(
        record.encoded_len() <= 256 * 1024,
        "operation record byte bound"
    );
    ensure!(
        connection.execute(
            "UPDATE device_operations SET record=?3,state=?4 WHERE namespace=?1 AND record_id=?2",
            params![
                namespace,
                key.as_bytes(),
                record.encode_to_vec(),
                record.state
            ]
        )? == 1,
        "operation absent"
    );
    Ok(())
}
/// Cancellation is requested now; only the executing worker acknowledges Canceled.
pub fn cancel(
    directory: &Path,
    command: Command<'_>,
    key: ContentHash,
    expected: &[u8],
    response: Vec<u8>,
) -> Result<Started> {
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(prior) = replay(&tx, &command)? {
        return Ok(Started {
            response: prior,
            fresh: false,
        });
    }
    let mut record =
        load(&tx, command.namespace, key)?.context("operation not found in caller namespace")?;
    ensure!(
        !expected.is_empty() && expected == record.version,
        "operation version changed"
    );
    ensure!(
        record.cancellation_supported
            && matches!(record.state,s if s==State::Queued as i32 || s==State::Running as i32),
        "operation no longer supports cancellation"
    );
    record.cancellation_requested = true;
    save(&tx, command.namespace, key, &mut record)?;
    receipt(&tx, &command, &response)?;
    tx.commit()?;
    drop(connection);
    wake(directory)?;
    Ok(Started {
        response,
        fresh: true,
    })
}
/// The worker's exact namespace and executor incarnation own terminal transitions.
pub fn transition(
    directory: &Path,
    namespace: &str,
    key: ContentHash,
    executor: &str,
    next: State,
    failure: Option<api::heddle::api::v1alpha1::CallFailure>,
) -> Result<OperationRecord> {
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT executor FROM device_operations WHERE namespace=?1 AND record_id=?2",
            params![namespace, key.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        owner.as_deref() == Some(executor),
        "executor does not own operation"
    );
    let mut record = load(&tx, namespace, key)?.context("operation absent")?;
    ensure!(
        matches!(record.state,s if s==State::Queued as i32||s==State::Running as i32),
        "operation is terminal"
    );
    ensure!(
        matches!(
            next,
            State::Running | State::Completed | State::Failed | State::Canceled
        ),
        "invalid executor transition"
    );
    // Publication won its own commit before a cancellation request; do not invent rollback.
    record.state = next as i32;
    record.failure = failure;
    record.cancellation_supported = next == State::Running;
    if next == State::Completed {
        record.completed_units = record.total_units.unwrap_or(1);
    }
    save(&tx, namespace, key, &mut record)?;
    tx.commit()?;
    drop(connection);
    wake(directory)?;
    Ok(record)
}
fn wake(directory: &Path) -> Result<()> {
    objects::fs_atomic::write_file_atomic_secret(
        &directory.join(crate::local_metadata::CHANGE_MARKER_NAME),
        uuid::Uuid::now_v7().as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v2alpha1::{RecordRef, SpoolRef};

    use super::*;
    #[test]
    fn executor_incarnation_distinguishes_live_process_from_stale_work() {
        let identity = executor_identity().expect("current process identity");
        assert!(!executor_is_dead(&identity).expect("current executor"));
        #[cfg(target_os = "linux")]
        assert!(
            executor_is_dead(&format!(
                "{}:previous-boot:old-incarnation",
                std::process::id()
            ))
            .expect("prior incarnation")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_executor_is_failed_without_claiming_cancellation_or_rollback() {
        let temp = tempfile::tempdir().expect("store");
        let id = OperationId::new();
        let namespace = "owner/agent";
        let key = crate::operation_dedup::receipt_record_key(namespace, id);
        let executor = format!("{}:prior-boot:old-executor", std::process::id());
        let record = OperationRecord {
            r#ref: Some(RecordRef {
                spool: Some(SpoolRef {
                    id: uuid::Uuid::new_v4().to_string(),
                }),
                id: key.to_string(),
            }),
            client_operation_id: id.to_string(),
            state: State::Queued as i32,
            cancellation_supported: true,
            ..Default::default()
        };
        start(
            temp.path(),
            Command {
                namespace,
                id,
                method: "StartAnalysis",
                request_hash: [1; 32],
            },
            &executor,
            record,
            vec![1],
        )
        .expect("durable old executor");
        assert!(
            recover_if_dead(temp.path(), namespace, key, &executor)
                .expect("recover stopped executor")
        );
        let record = get(temp.path(), namespace, key)
            .expect("lookup")
            .expect("record");
        assert_eq!(record.state, State::Failed as i32);
        assert!(
            !record.cancellation_requested,
            "crash is not requested cancellation"
        );
        assert_eq!(
            record.completed_units, 0,
            "crash is not proof of completion"
        );
        assert!(
            record
                .failure
                .expect("failure provenance")
                .message
                .contains("before acknowledging")
        );
    }

    #[test]
    fn execution_cancel_is_namespaced_versioned_and_acknowledged_by_worker() {
        let temp = tempfile::tempdir().expect("store");
        let namespace = "owner/agent";
        let operation = OperationId::from_uuid(uuid::Uuid::new_v4());
        let key = crate::operation_dedup::receipt_record_key(namespace, operation);
        let command = || Command {
            namespace,
            id: operation,
            method: "StartAnalysis",
            request_hash: [1; 32],
        };
        let record = OperationRecord {
            r#ref: Some(RecordRef {
                spool: Some(SpoolRef {
                    id: uuid::Uuid::new_v4().to_string(),
                }),
                id: key.to_string(),
            }),
            client_operation_id: operation.to_string(),
            state: State::Queued as i32,
            cancellation_supported: true,
            ..Default::default()
        };
        assert!(
            start(temp.path(), command(), "executor", record.clone(), vec![1])
                .expect("start")
                .fresh
        );
        assert!(
            !start(temp.path(), command(), "executor", record, vec![1])
                .expect("replay")
                .fresh
        );
        let queued = get(temp.path(), namespace, key)
            .expect("lookup")
            .expect("queued");
        assert!(
            get(temp.path(), "different-agent", key)
                .expect("lookup")
                .is_none()
        );
        let cancellation = OperationId::from_uuid(uuid::Uuid::new_v4());
        let cancel_command = |namespace| Command {
            namespace,
            id: cancellation,
            method: "CancelOperation",
            request_hash: [2; 32],
        };
        assert!(
            cancel(
                temp.path(),
                cancel_command("different-agent"),
                key,
                &queued.version,
                vec![2]
            )
            .is_err(),
            "opaque record ID cannot cross caller namespace"
        );
        assert!(
            cancel(
                temp.path(),
                cancel_command(namespace),
                key,
                &[3; 32],
                vec![2]
            )
            .is_err(),
            "cancellation requires observed execution version"
        );
        cancel(
            temp.path(),
            cancel_command(namespace),
            key,
            &queued.version,
            vec![2],
        )
        .expect("request cancellation");
        let requested = get(temp.path(), namespace, key)
            .expect("lookup")
            .expect("record");
        assert_eq!(
            requested.state,
            State::Queued as i32,
            "request alone cannot claim execution stopped"
        );
        assert!(requested.cancellation_requested);
        assert!(
            transition(
                temp.path(),
                namespace,
                key,
                "different-executor",
                State::Canceled,
                None
            )
            .is_err()
        );
        let cancelled = transition(
            temp.path(),
            namespace,
            key,
            "executor",
            State::Canceled,
            None,
        )
        .expect("worker acknowledges");
        assert_eq!(cancelled.state, State::Canceled as i32);
        assert!(!cancelled.cancellation_supported);
        let rows = crate::operation_dedup::observation::page(
            temp.path(),
            namespace,
            &[operation],
            &[],
            None,
            1,
        )
        .expect("observed execution");
        assert_eq!(
            rows[0].execution.as_ref().expect("durable execution").state,
            State::Canceled as i32
        );
    }
}

/// An executor identity includes OS process identity; a reused PID cannot inherit it.
pub fn executor_identity() -> Result<String> {
    let pid = std::process::id();
    Ok(format!(
        "{pid}:{}:{}",
        process_birth(pid)?.unwrap_or_default(),
        uuid::Uuid::new_v4()
    ))
}
#[cfg(target_os = "linux")]
fn process_birth(pid: u32) -> Result<Option<String>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let fields = stat
        .rsplit_once(") ")
        .context("invalid process identity record")?
        .1;
    let start = fields
        .split_whitespace()
        .nth(19)
        .context("process start time absent")?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Some(format!("{}-{start}", boot.trim())))
}
#[cfg(not(target_os = "linux"))]
fn process_birth(pid: u32) -> Result<Option<String>> {
    // Without a portable birth identity, only a definite absent process is dead.
    let pid = i32::try_from(pid)?;
    let result = unsafe { libc::kill(pid, 0) };
    if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Ok(Some("present".into()))
    }
}
pub fn executor_is_dead(executor: &str) -> Result<bool> {
    let mut parts = executor.splitn(3, ':');
    let pid = parts
        .next()
        .context("executor PID absent")?
        .parse::<u32>()?;
    let birth = parts.next().context("executor birth absent")?;
    let current = process_birth(pid)?;
    Ok(current.as_deref() != Some(birth))
}
pub fn recover_if_dead(
    directory: &Path,
    namespace: &str,
    key: ContentHash,
    executor: &str,
) -> Result<bool> {
    if !executor_is_dead(executor)? {
        return Ok(false);
    }
    let failure=api::heddle::api::v1alpha1::CallFailure{code:api::heddle::api::v1alpha1::CallFailureCode::Unavailable as i32,message:"Native executor stopped before acknowledging completion; inspect retained results before retrying".into(),..Default::default()};
    transition(
        directory,
        namespace,
        key,
        executor,
        State::Failed,
        Some(failure),
    )?;
    Ok(true)
}
