//! Bounded durable upload work in the same SQLite transaction as local runs.
use anyhow::{Context, Result, ensure};
use api::heddle::api::v1alpha2::{
    RegisterTimelineOriginRequest, RunRecord, TimelineAdmissionAcceptance,
    TimelineOriginEndorsement, TimelineRecord, UploadScrubbedTimelineAck,
    UploadScrubbedTimelineRequest,
};
use prost::Message;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::device_run_projection;

pub const MAX_PENDING_BYTES: i64 = 64 * 1024 * 1024;
pub const MAX_PENDING_REQUESTS: i64 = 10_000;
type StoredRunProgress = (Vec<u8>, i64, Vec<u8>, i64, i64, String);
type StoredRegistration = (String, Vec<u8>, Vec<u8>, String, Vec<u8>);

pub fn initialize_schema(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS timeline_upload_runs (
           run TEXT PRIMARY KEY,
           origin BLOB NOT NULL,
           origin_biscuit BLOB NOT NULL,
           target_deployment BLOB NOT NULL,
           registration_operation_id TEXT NOT NULL,
           registered INTEGER NOT NULL DEFAULT 0,
           registration_attempts INTEGER NOT NULL DEFAULT 0,
           registration_due_millis INTEGER NOT NULL DEFAULT 0,
           run_revision INTEGER NOT NULL DEFAULT 0,
           snapshot BLOB NOT NULL DEFAULT X'',
           last_local_position INTEGER NOT NULL DEFAULT -1,
           next_upload_position INTEGER NOT NULL DEFAULT 0,
           acked_position INTEGER NOT NULL DEFAULT 0,
           upload_incomplete TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS timeline_upload_outbox (
           operation_id TEXT PRIMARY KEY,
           run TEXT NOT NULL REFERENCES timeline_upload_runs(run),
           request BLOB NOT NULL CHECK(length(request)<=262144),
           acceptance BLOB NOT NULL DEFAULT X'',
           first_position INTEGER NOT NULL,
           next_position INTEGER NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0,
           due_millis INTEGER NOT NULL DEFAULT 0,
           terminal INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS timeline_upload_outbox_run ON timeline_upload_outbox(run);
         CREATE TABLE IF NOT EXISTS timeline_upload_event_map (
           run TEXT NOT NULL,
           hosted_position INTEGER NOT NULL,
           local_position INTEGER NOT NULL,
           PRIMARY KEY(run,hosted_position)
         );",
    )
}

/// A missing origin at local creation is durable and visible. A later credential
/// cannot silently claim earlier work as its own run.
pub(crate) fn append_in_tx(
    tx: &Transaction<'_>,
    run: &RunRecord,
    creation_origin: Option<(&TimelineOriginEndorsement, &[u8])>,
) -> Result<()> {
    append_with_limits(
        tx,
        run,
        creation_origin,
        MAX_PENDING_BYTES,
        MAX_PENDING_REQUESTS,
    )
}

fn append_with_limits(
    tx: &Transaction<'_>,
    run: &RunRecord,
    creation_origin: Option<(&TimelineOriginEndorsement, &[u8])>,
    byte_limit: i64,
    request_limit: i64,
) -> Result<()> {
    let Some(reference) = run.r#ref.as_ref() else {
        return Ok(());
    };
    if run.thread.is_none() {
        return Ok(());
    }
    let existing: Option<StoredRunProgress> = tx
        .query_row(
            "SELECT origin,run_revision,snapshot,last_local_position,next_upload_position,upload_incomplete FROM timeline_upload_runs WHERE run=?1",
            [&reference.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?;
    if existing.is_none() {
        let (origin_bytes, biscuit, target, incomplete) = match creation_origin {
            Some((origin, biscuit)) => {
                api::timeline_upload::validate_origin(origin)?;
                ensure!(origin.run_id == reference.id, "origin binds another run");
                (
                    origin.encode_to_vec(),
                    biscuit.to_vec(),
                    origin.deployment_public_key.clone(),
                    "",
                )
            }
            None => (Vec::new(), Vec::new(), Vec::new(), "origin_unavailable"),
        };
        tx.execute(
            "INSERT INTO timeline_upload_runs(run,origin,origin_biscuit,target_deployment,registration_operation_id,upload_incomplete) VALUES(?1,?2,?3,?4,?5,?6)",
            params![reference.id, origin_bytes, biscuit, target, uuid::Uuid::now_v7().to_string(), incomplete],
        )?;
    }
    let (origin_bytes, old_revision, old_snapshot, last_local, next_upload, incomplete) = match existing {
        Some(row) => row,
        None => tx.query_row(
            "SELECT origin,run_revision,snapshot,last_local_position,next_upload_position,upload_incomplete FROM timeline_upload_runs WHERE run=?1",
            [&reference.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )?,
    };
    if origin_bytes.is_empty()
        || matches!(
            incomplete.as_str(),
            "missing_local_prefix"
                | "registration_denied"
                | "upload_denied"
                | "resource_gone"
                | "conflict"
        )
    {
        return Ok(());
    }
    let origin = TimelineOriginEndorsement::decode(origin_bytes.as_slice())?;
    let biscuit: Vec<u8> = tx.query_row(
        "SELECT origin_biscuit FROM timeline_upload_runs WHERE run=?1",
        [&reference.id],
        |row| row.get(0),
    )?;
    let snapshot = match device_run_projection::snapshot(run) {
        Ok(value) => value,
        Err(_) => {
            mark_incomplete(tx, &reference.id, "projection_invalid")?;
            return Ok(());
        }
    };
    let snapshot_bytes = snapshot.encode_to_vec();
    let revision = if old_revision == 0 || snapshot_bytes != old_snapshot {
        old_revision
            .checked_add(1)
            .context("run revision exhausted")?
    } else {
        old_revision
    };
    let mut projected = Vec::new();
    let mut projected_local_positions = Vec::new();
    let mut scanned = last_local;
    {
        let mut query = tx.prepare(
            "SELECT position,record FROM run_timeline WHERE run=?1 AND position>?2 ORDER BY position LIMIT 256",
        )?;
        let rows = query.query_map(params![reference.id, last_local], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (position, bytes) = row?;
            if position != scanned + 1 {
                mark_incomplete(tx, &reference.id, "missing_local_prefix")?;
                return Ok(());
            }
            let local = TimelineRecord::decode(bytes.as_slice())?;
            let hosted_position = u64::try_from(next_upload)? + projected.len() as u64;
            match device_run_projection::event(&local, hosted_position) {
                Ok(Some(event)) => {
                    projected.push(event);
                    projected_local_positions.push(position);
                }
                Ok(None) => {}
                Err(_) => {
                    mark_incomplete(tx, &reference.id, "projection_invalid")?;
                    return Ok(());
                }
            }
            scanned = position;
            if projected.len() == api::timeline_upload::MAX_TIMELINE_EVENTS {
                break;
            }
        }
    }
    if projected.is_empty() && revision == old_revision {
        tx.execute(
            "UPDATE timeline_upload_runs SET last_local_position=?2,upload_incomplete='' WHERE run=?1 AND upload_incomplete IN ('','outbox_overflow')",
            params![reference.id, scanned],
        )?;
        return Ok(());
    }
    let request = UploadScrubbedTimelineRequest {
        client_operation_id: uuid::Uuid::now_v7().to_string(),
        thread: run.thread.clone(),
        run: run.r#ref.clone(),
        canonicalization_version: 1,
        run_revision: u64::try_from(revision)?,
        snapshot: Some(snapshot),
        events: projected,
        origin: Some(origin),
        acceptance: None,
        first_position: u64::try_from(next_upload)?,
        origin_credential_biscuit: biscuit,
    };
    let now_micros = i128::from(chrono::Utc::now().timestamp_micros());
    if api::timeline_upload::validate_upload(&request, now_micros).is_err()
        || api::timeline_upload::validate_upload_provenance(&request, false).is_err()
    {
        mark_incomplete(tx, &reference.id, "projection_invalid")?;
        return Ok(());
    }
    let bytes = request.encode_to_vec();
    let (count, used): (i64, i64) = tx.query_row(
        "SELECT COUNT(*),COALESCE(SUM(length(request)),0) FROM timeline_upload_outbox",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count >= request_limit || used.saturating_add(bytes.len() as i64) > byte_limit {
        mark_incomplete(tx, &reference.id, "outbox_overflow")?;
        return Ok(());
    }
    let next = next_upload
        .checked_add(request.events.len() as i64)
        .context("hosted position exhausted")?;
    tx.execute(
        "INSERT INTO timeline_upload_outbox(operation_id,run,request,first_position,next_position) VALUES(?1,?2,?3,?4,?5)",
        params![request.client_operation_id, reference.id, bytes, next_upload, next],
    )?;
    for (offset, local_position) in projected_local_positions.into_iter().enumerate() {
        tx.execute(
            "INSERT INTO timeline_upload_event_map(run,hosted_position,local_position) VALUES(?1,?2,?3)",
            params![reference.id, next_upload + offset as i64, local_position],
        )?;
    }
    tx.execute(
        "UPDATE timeline_upload_runs SET run_revision=?2,snapshot=?3,last_local_position=?4,next_upload_position=?5,upload_incomplete='' WHERE run=?1",
        params![reference.id, revision, snapshot_bytes, scanned, next],
    )?;
    Ok(())
}

fn mark_incomplete(tx: &Transaction<'_>, run: &str, reason: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE timeline_upload_runs SET upload_incomplete=?2 WHERE run=?1",
        params![run, reason],
    )?;
    Ok(())
}

pub struct PendingTimelineUpload {
    pub request: UploadScrubbedTimelineRequest,
    pub target_deployment: Vec<u8>,
}

pub struct PendingTimelineRegistration {
    pub request: RegisterTimelineOriginRequest,
    pub target_deployment: Vec<u8>,
}

#[derive(Default)]
pub struct TimelineUploadHealth {
    pub pending_requests: u64,
    pub incomplete_runs: u64,
}

pub(crate) fn health(connection: &rusqlite::Connection) -> rusqlite::Result<TimelineUploadHealth> {
    let (pending, incomplete): (i64, i64) = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM timeline_upload_outbox WHERE terminal=0),
                (SELECT COUNT(*) FROM timeline_upload_runs WHERE upload_incomplete!='')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(TimelineUploadHealth {
        pending_requests: pending as u64,
        incomplete_runs: incomplete as u64,
    })
}

pub(crate) fn next_registration(
    connection: &rusqlite::Connection,
    now_millis: i64,
) -> Result<Option<PendingTimelineRegistration>> {
    let row: Option<StoredRegistration> = connection
        .query_row(
            "SELECT run,origin,origin_biscuit,registration_operation_id,target_deployment
             FROM timeline_upload_runs WHERE registered=0 AND length(origin)>0
               AND registration_due_millis<=?1
             ORDER BY rowid LIMIT 1",
            [now_millis],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, origin_bytes, biscuit, operation, target)) = row else {
        return Ok(None);
    };
    let origin = TimelineOriginEndorsement::decode(origin_bytes.as_slice())?;
    let spool = api::heddle::api::v1alpha2::SpoolRef {
        id: origin.spool_id.clone(),
    };
    Ok(Some(PendingTimelineRegistration {
        request: RegisterTimelineOriginRequest {
            client_operation_id: operation,
            thread: Some(api::heddle::api::v1alpha2::ThreadRef {
                spool: Some(spool.clone()),
                id: Some(api::heddle::api::v1alpha2::ThreadId {
                    value: origin.thread_id.clone(),
                }),
            }),
            run: Some(api::heddle::api::v1alpha2::RecordRef {
                spool: Some(spool),
                id: run_id,
            }),
            origin: Some(origin),
            origin_credential_biscuit: biscuit,
        },
        target_deployment: target,
    }))
}

pub(crate) fn next_upload(
    connection: &rusqlite::Connection,
    now_millis: i64,
) -> Result<Option<PendingTimelineUpload>> {
    let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = connection.query_row(
        "SELECT q.request,r.target_deployment,q.acceptance FROM timeline_upload_outbox q
         JOIN timeline_upload_runs r ON r.run=q.run
         WHERE q.terminal=0 AND q.due_millis<=?1
           AND NOT EXISTS(SELECT 1 FROM timeline_upload_outbox earlier WHERE earlier.run=q.run
             AND (earlier.first_position<q.first_position OR (earlier.first_position=q.first_position AND earlier.rowid<q.rowid)))
         ORDER BY q.rowid LIMIT 1",
        [now_millis],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    row.map(|(bytes, target, acceptance)| {
        let mut request = UploadScrubbedTimelineRequest::decode(bytes.as_slice())?;
        ensure!(
            request.encode_to_vec() == bytes,
            "stored timeline request is noncanonical"
        );
        if !acceptance.is_empty() {
            request.acceptance = Some(TimelineAdmissionAcceptance::decode(acceptance.as_slice())?);
            api::timeline_upload::validate_upload(
                &request,
                i128::from(chrono::Utc::now().timestamp_micros()),
            )?;
        }
        Ok(PendingTimelineUpload {
            request,
            target_deployment: target,
        })
    })
    .transpose()
}

/// Acceptance is current authority evidence. It never changes the durable
/// logical request or its operation ID/digest and can be renewed for retries.
pub(crate) fn attach_acceptance(
    tx: &Transaction<'_>,
    operation_id: &str,
    acceptance: &TimelineAdmissionAcceptance,
) -> Result<()> {
    let bytes: Vec<u8> = tx.query_row(
        "SELECT request FROM timeline_upload_outbox WHERE operation_id=?1 AND terminal=0",
        [operation_id],
        |row| row.get(0),
    )?;
    let mut request = UploadScrubbedTimelineRequest::decode(bytes.as_slice())?;
    request.acceptance = Some(acceptance.clone());
    api::timeline_upload::validate_upload(
        &request,
        i128::from(chrono::Utc::now().timestamp_micros()),
    )?;
    tx.execute(
        "UPDATE timeline_upload_outbox SET acceptance=?2,due_millis=0 WHERE operation_id=?1",
        params![operation_id, acceptance.encode_to_vec()],
    )?;
    Ok(())
}

pub(crate) fn acknowledge(
    tx: &Transaction<'_>,
    operation_id: &str,
    ack: &UploadScrubbedTimelineAck,
) -> Result<()> {
    let row: (String, i64, i64, Vec<u8>) = tx.query_row(
        "SELECT run,first_position,next_position,request FROM timeline_upload_outbox WHERE operation_id=?1",
        [operation_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let request = UploadScrubbedTimelineRequest::decode(row.3.as_slice())?;
    ensure!(
        request.first_position == row.1 as u64
            && ack.run == request.run
            && ack.run_revision == request.run_revision
            && ack.run_version.len() == 32
            && ack.operation.is_some()
            && ack.accepted_event_count as usize == request.events.len()
            && ack.next_position == row.2 as u64,
        "timeline ack differs from stored logical request"
    );
    let acked: i64 = tx.query_row(
        "SELECT acked_position FROM timeline_upload_runs WHERE run=?1",
        [&row.0],
        |row| row.get(0),
    )?;
    ensure!(acked == row.1, "timeline ack would skip an unacked prefix");
    tx.execute(
        "DELETE FROM timeline_upload_outbox WHERE operation_id=?1",
        [operation_id],
    )?;
    tx.execute(
        "UPDATE timeline_upload_runs SET acked_position=?2 WHERE run=?1",
        params![row.0, row.2],
    )?;
    Ok(())
}

/// A gap is a no-write response. Rebuild the exact missing projected event
/// prefix from retained local positions without changing any queued request.
pub(crate) fn repair_gap(
    tx: &Transaction<'_>,
    operation_id: &str,
    expected_position: u64,
) -> Result<()> {
    let (run_id, first, request_bytes): (String, i64, Vec<u8>) = tx.query_row(
        "SELECT run,first_position,request FROM timeline_upload_outbox WHERE operation_id=?1",
        [operation_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let expected =
        i64::try_from(expected_position).context("server gap position exceeds v1 range")?;
    if expected >= first {
        deny(tx, operation_id, "conflict")?;
        return Ok(());
    }
    tx.execute(
        "UPDATE timeline_upload_runs SET acked_position=MIN(acked_position,?2) WHERE run=?1",
        params![run_id, expected],
    )?;
    let mut request = UploadScrubbedTimelineRequest::decode(request_bytes.as_slice())?;
    request.client_operation_id = uuid::Uuid::now_v7().to_string();
    request.first_position = expected_position;
    request.acceptance = None;
    request.events.clear();
    let end = first.min(expected.saturating_add(api::timeline_upload::MAX_TIMELINE_EVENTS as i64));
    for position in expected..end {
        let local: Option<Vec<u8>> = tx
            .query_row(
                "SELECT t.record FROM timeline_upload_event_map m
             JOIN run_timeline t ON t.run=m.run AND t.position=m.local_position
             WHERE m.run=?1 AND m.hosted_position=?2",
                params![run_id, position],
                |row| row.get(0),
            )
            .optional()?;
        let Some(local) = local else {
            deny(tx, operation_id, "missing_local_prefix")?;
            return Ok(());
        };
        let local = TimelineRecord::decode(local.as_slice())?;
        let Some(projected) = device_run_projection::event(&local, position as u64)? else {
            deny(tx, operation_id, "missing_local_prefix")?;
            return Ok(());
        };
        request.events.push(projected);
    }
    if request.events.is_empty()
        || api::timeline_upload::validate_upload(
            &request,
            i128::from(chrono::Utc::now().timestamp_micros()),
        )
        .is_err()
    {
        deny(tx, operation_id, "missing_local_prefix")?;
        return Ok(());
    }
    let bytes = request.encode_to_vec();
    let (count, used): (i64, i64) = tx.query_row(
        "SELECT COUNT(*),COALESCE(SUM(length(request)),0) FROM timeline_upload_outbox",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count >= MAX_PENDING_REQUESTS || used.saturating_add(bytes.len() as i64) > MAX_PENDING_BYTES
    {
        mark_incomplete(tx, &run_id, "outbox_overflow")?;
        retry(tx, operation_id, chrono::Utc::now().timestamp_millis())?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO timeline_upload_outbox(operation_id,run,request,first_position,next_position) VALUES(?1,?2,?3,?4,?5)",
        params![request.client_operation_id, run_id, bytes, expected, end],
    )?;
    Ok(())
}

pub(crate) fn retry(
    tx: &Transaction<'_>,
    operation_id: &str,
    now_millis: i64,
) -> rusqlite::Result<()> {
    let attempts: i64 = tx.query_row(
        "SELECT attempts FROM timeline_upload_outbox WHERE operation_id=?1",
        [operation_id],
        |row| row.get(0),
    )?;
    let delay_seconds = retry_delay_seconds(attempts);
    tx.execute(
        "UPDATE timeline_upload_outbox SET attempts=attempts+1,due_millis=?2 WHERE operation_id=?1",
        params![
            operation_id,
            now_millis.saturating_add(delay_seconds * 1000)
        ],
    )?;
    Ok(())
}

fn retry_delay_seconds(attempts: i64) -> i64 {
    1_i64
        .checked_shl(u32::try_from(attempts).unwrap_or(31).min(8))
        .unwrap_or(300)
        .min(300)
}

pub(crate) fn retry_registration(
    tx: &Transaction<'_>,
    operation_id: &str,
    now_millis: i64,
) -> rusqlite::Result<()> {
    let attempts: i64 = tx.query_row(
        "SELECT registration_attempts FROM timeline_upload_runs WHERE registration_operation_id=?1",
        [operation_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE timeline_upload_runs SET registration_attempts=registration_attempts+1,registration_due_millis=?2 WHERE registration_operation_id=?1",
        params![operation_id, now_millis.saturating_add(retry_delay_seconds(attempts) * 1000)],
    )?;
    Ok(())
}

pub(crate) fn deny(tx: &Transaction<'_>, operation_id: &str, reason: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE timeline_upload_outbox SET terminal=1 WHERE operation_id=?1",
        [operation_id],
    )?;
    tx.execute(
        "UPDATE timeline_upload_runs SET upload_incomplete=?2 WHERE run=(SELECT run FROM timeline_upload_outbox WHERE operation_id=?1)",
        params![operation_id, reason],
    )?;
    Ok(())
}

pub(crate) fn registered(tx: &Transaction<'_>, operation_id: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE timeline_upload_runs SET registered=1 WHERE registration_operation_id=?1",
        [operation_id],
    )?;
    Ok(())
}

pub(crate) fn deny_registration(tx: &Transaction<'_>, operation_id: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE timeline_upload_runs SET registered=-1 WHERE registration_operation_id=?1",
        [operation_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha2::{
        RecordRef, SpoolRef, ThreadId, ThreadRef, TimelineOriginCredentialClass,
        TimelineOriginCredentialIdentity, TimelineServerIssuedCredential, UploadTimelineEventKind,
        operation_record::State, timeline_admission_acceptance::Authority,
        timeline_origin_credential_identity::Identity,
    };

    use super::*;

    #[test]
    fn permanent_registration_denial_is_visible_in_upload_health() {
        let (_directory, store, run, origin) = fixture();
        store
            .publish_report(run, None, &[], Some((&origin, &[])))
            .expect("local run");
        let registration = store
            .next_timeline_registration(i64::MAX)
            .expect("registration query")
            .expect("pending registration");
        store
            .deny_timeline_registration(&registration.request.client_operation_id)
            .expect("deny registration");
        assert_eq!(store.upload_health().expect("health").incomplete_runs, 1);
        assert!(
            store
                .next_timeline_registration(i64::MAX)
                .expect("registration query")
                .is_none()
        );
    }

    fn fixture() -> (
        tempfile::TempDir,
        crate::device_runs::RunStore,
        RunRecord,
        TimelineOriginEndorsement,
    ) {
        let directory = tempfile::tempdir().expect("directory");
        let store = crate::device_runs::RunStore::open(directory.path()).expect("store");
        let spool = SpoolRef {
            id: uuid::Uuid::from_u128(1).to_string(),
        };
        let thread = ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId { value: vec![2; 32] }),
        };
        let run = RunRecord {
            r#ref: Some(RecordRef {
                spool: Some(spool.clone()),
                id: "run_1".into(),
            }),
            thread: Some(thread),
            principal_id: uuid::Uuid::from_u128(3).to_string(),
            state: State::Running as i32,
            harness: "codex".into(),
            ..Default::default()
        };
        let origin = TimelineOriginEndorsement {
            deployment_public_key: vec![4; 32],
            spool_id: spool.id,
            thread_id: vec![2; 32],
            run_id: "run_1".into(),
            principal_id: run.principal_id.clone(),
            credential_class: TimelineOriginCredentialClass::DirectHuman as i32,
            effective_pop_key_sha256: vec![5; 32],
            credential_identity: Some(TimelineOriginCredentialIdentity {
                identity: Some(Identity::ServerIssued(TimelineServerIssuedCredential {
                    credential_id: b"cred-1".to_vec(),
                })),
            }),
            uploader_device_public_key: vec![6; 32],
            signature: vec![7; 64],
        };
        (directory, store, run, origin)
    }

    fn local_event(run: &RunRecord, kind: &str) -> TimelineRecord {
        TimelineRecord {
            run: run.r#ref.clone(),
            kind: kind.into(),
            recorded_at: Some(prost_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn run_only_revision_queues_a_zero_event_snapshot_at_the_same_position() {
        let (directory, store, mut run, origin) = fixture();
        store
            .publish_report(run.clone(), None, &[], Some((&origin, &[])))
            .expect("first run snapshot");
        run.state = State::Completed as i32;
        store
            .publish_report(run, None, &[], None)
            .expect("run-only revision");
        let connection = crate::local_metadata::open(directory.path()).expect("database");
        let mut query = connection
            .prepare("SELECT request FROM timeline_upload_outbox ORDER BY rowid")
            .expect("query");
        let requests = query
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .expect("rows")
            .map(|row| {
                UploadScrubbedTimelineRequest::decode(row.expect("bytes").as_slice())
                    .expect("request")
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            (
                requests[0].run_revision,
                requests[0].first_position,
                requests[0].events.len()
            ),
            (1, 0, 0)
        );
        assert_eq!(
            (
                requests[1].run_revision,
                requests[1].first_position,
                requests[1].events.len()
            ),
            (2, 0, 0)
        );
        drop(query);
        drop(connection);
        for expected_revision in [1, 2] {
            let pending = store
                .next_timeline_upload(i64::MAX)
                .expect("queue")
                .expect("run-only upload");
            assert_eq!(pending.request.run_revision, expected_revision);
            let ack = UploadScrubbedTimelineAck {
                run: pending.request.run.clone(),
                run_revision: expected_revision,
                run_version: vec![8; 32],
                next_position: 0,
                accepted_event_count: 0,
                operation: pending.request.run.clone(),
            };
            store
                .acknowledge_timeline_upload(&pending.request.client_operation_id, &ack)
                .expect("durable run-only ack");
        }
        assert_eq!(store.upload_health().expect("health").pending_requests, 0);
    }

    #[test]
    fn outbox_overflow_marks_incomplete_without_evicting_unacked_prefix() {
        let (directory, store, run, origin) = fixture();
        let reference = run.r#ref.as_ref().expect("run ref");
        store
            .publish_report(
                run.clone(),
                None,
                &[("opened".into(), local_event(&run, "session_opened"))],
                Some((&origin, &[])),
            )
            .expect("first queued event");
        let first = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("first");
        assert_eq!(
            first.request.events[0].kind,
            UploadTimelineEventKind::RunStarted as i32
        );
        store
            .append_report_timeline(
                reference,
                &[("checkpoint:1".into(), local_event(&run, "Stop"))],
            )
            .expect("second local event");
        let mut connection = crate::local_metadata::open(directory.path()).expect("database");
        let tx = connection.transaction().expect("transaction");
        append_with_limits(&tx, &run, None, MAX_PENDING_BYTES, 1)
            .expect("overflow is local success");
        tx.commit().expect("commit");
        let health = store.upload_health().expect("health");
        assert_eq!((health.pending_requests, health.incomplete_runs), (1, 1));
        assert_eq!(
            store
                .latest_timeline("run_1", 10, crate::device_runs::RunReader::Owner)
                .expect("local events")
                .len(),
            2
        );
        let retained = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("first retained");
        assert_eq!(
            retained.request.client_operation_id,
            first.request.client_operation_id
        );
        assert_eq!(retained.request.events.len(), 1);
    }

    #[test]
    fn gap_repair_rebuilds_the_exact_missing_local_prefix() {
        let (directory, store, run, origin) = fixture();
        let reference = run.r#ref.as_ref().expect("run ref");
        store
            .publish_report(
                run.clone(),
                None,
                &[("opened".into(), local_event(&run, "session_opened"))],
                Some((&origin, &[])),
            )
            .expect("first event");
        let first = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("first");
        store
            .publish_report(
                run.clone(),
                None,
                &[
                    ("opened".into(), local_event(&run, "session_opened")),
                    ("finished".into(), local_event(&run, "session_closed")),
                ],
                None,
            )
            .expect("second event");
        let connection = crate::local_metadata::open(directory.path()).expect("database");
        connection
            .execute(
                "DELETE FROM timeline_upload_outbox WHERE operation_id=?1",
                [&first.request.client_operation_id],
            )
            .expect("simulate lost queue row");
        let later: String = connection
            .query_row(
                "SELECT operation_id FROM timeline_upload_outbox WHERE run=?1",
                [&reference.id],
                |row| row.get(0),
            )
            .expect("later operation");
        drop(connection);
        store.repair_timeline_gap(&later, 0).expect("rebuild gap");
        let repaired = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("repaired prefix");
        assert_eq!(repaired.request.first_position, 0);
        assert_eq!(repaired.request.events.len(), 1);
        assert_eq!(
            repaired.request.events[0].kind,
            UploadTimelineEventKind::RunStarted as i32
        );
        assert_ne!(
            repaired.request.client_operation_id,
            first.request.client_operation_id
        );
    }

    #[test]
    fn missing_local_prefix_is_terminal_incomplete() {
        let (directory, store, run, origin) = fixture();
        store
            .publish_report(
                run.clone(),
                None,
                &[("opened".into(), local_event(&run, "session_opened"))],
                Some((&origin, &[])),
            )
            .expect("first event");
        store
            .publish_report(
                run.clone(),
                None,
                &[
                    ("opened".into(), local_event(&run, "session_opened")),
                    ("finished".into(), local_event(&run, "session_closed")),
                ],
                None,
            )
            .expect("second event");
        let connection = crate::local_metadata::open(directory.path()).expect("database");
        connection
            .execute(
                "DELETE FROM timeline_upload_outbox WHERE first_position=0",
                [],
            )
            .expect("simulate lost queue row");
        connection
            .execute(
                "DELETE FROM run_timeline WHERE run='run_1' AND position=0",
                [],
            )
            .expect("simulate missing local prefix");
        let later: String = connection
            .query_row(
                "SELECT operation_id FROM timeline_upload_outbox WHERE run='run_1'",
                [],
                |row| row.get(0),
            )
            .expect("later operation");
        drop(connection);
        store
            .repair_timeline_gap(&later, 0)
            .expect("detect missing prefix");
        let connection = crate::local_metadata::open(directory.path()).expect("database");
        let reason: String = connection
            .query_row(
                "SELECT upload_incomplete FROM timeline_upload_runs WHERE run='run_1'",
                [],
                |row| row.get(0),
            )
            .expect("incomplete reason");
        assert_eq!(reason, "missing_local_prefix");
        assert_eq!(store.upload_health().expect("health").incomplete_runs, 1);
    }

    #[test]
    fn owner_acceptance_is_durable_evidence_without_changing_the_logical_request() {
        let (directory, store, run, origin) = fixture();
        store
            .publish_report(run, None, &[], Some((&origin, &[])))
            .expect("queued run");
        let original = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("request")
            .request;
        let now = i128::from(chrono::Utc::now().timestamp_micros());
        let digest = api::timeline_upload::logical_request_digest(&original, now).expect("digest");
        let acceptance = TimelineAdmissionAcceptance {
            origin_sha256: api::timeline_upload::origin_digest(&origin)
                .expect("origin digest")
                .to_vec(),
            uploader_device_public_key: origin.uploader_device_public_key.clone(),
            deployment_public_key: origin.deployment_public_key.clone(),
            request_sha256: digest.to_vec(),
            first_position: original.first_position,
            event_count: 0,
            authority: Some(Authority::OwnerDerivedCapability(vec![9])),
            signature: vec![8; 64],
        };
        store
            .attach_timeline_acceptance(&original.client_operation_id, &acceptance)
            .expect("attach acceptance");
        let outgoing = store
            .next_timeline_upload(i64::MAX)
            .expect("queue")
            .expect("request")
            .request;
        assert_eq!(outgoing.acceptance, Some(acceptance));
        assert_eq!(
            api::timeline_upload::logical_request_digest(&outgoing, now).expect("digest"),
            digest
        );
        let connection = crate::local_metadata::open(directory.path()).expect("database");
        let stored: Vec<u8> = connection
            .query_row(
                "SELECT request FROM timeline_upload_outbox WHERE operation_id=?1",
                [&original.client_operation_id],
                |row| row.get(0),
            )
            .expect("stored logical request");
        assert_eq!(stored, original.encode_to_vec());
    }
}
