//! Immutable, explicitly retained Run artifacts. Catalog identity and current
//! execution policy authorize reads; paths and content hashes are never authority.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::{RecordRef, RunArtifact, RunPolicy, RunRecord};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, params};

pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RUN_ARTIFACTS: usize = 128;
const MAX_RETENTION_SECONDS: u64 = 365 * 24 * 60 * 60;

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    directory: PathBuf,
    database: PathBuf,
}
pub struct ArtifactRead {
    pub record: RunArtifact,
    pub file: File,
}
pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS run_artifacts (id TEXT PRIMARY KEY, spool TEXT NOT NULL, run TEXT NOT NULL, kind TEXT NOT NULL, digest BLOB NOT NULL CHECK(length(digest)=32), expires INTEGER NOT NULL, record BLOB NOT NULL, phase INTEGER NOT NULL DEFAULT 0 CHECK(phase IN(0,1,2)), UNIQUE(spool,run,kind,digest)); CREATE INDEX IF NOT EXISTS run_artifacts_run ON run_artifacts(spool,run,id); CREATE INDEX IF NOT EXISTS run_artifacts_expiry ON run_artifacts(expires,id) WHERE phase!=2;")
}

impl ArtifactStore {
    pub fn open(heddle_dir: &Path) -> Result<Self> {
        let _runs = crate::device_runs::RunStore::open(heddle_dir)?;
        let this = Self {
            directory: heddle_dir.join("retained-artifacts"),
            database: heddle_dir.join(crate::local_metadata::DATABASE_NAME),
        };

        Ok(this)
    }
    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        let id = uuid::Uuid::parse_str(id)?;
        Ok(self.directory.join(id.to_string()))
    }
    fn changed(&self) -> Result<()> {
        objects::fs_atomic::write_file_atomic_secret(
            &self.database.with_extension("sqlite3.changed"),
            uuid::Uuid::now_v7().as_bytes(),
        )?;
        Ok(())
    }
    /// The actual harness calls this only for its independently bound Run.
    /// Exact retries reuse immutable identity and never extend the original TTL.
    pub fn retain(
        &self,
        run: &RecordRef,
        kind: &str,
        media_type: &str,
        bytes: &[u8],
        now: i64,
    ) -> Result<RunArtifact> {
        let spool = scope(run)?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES
            || kind.is_empty()
            || kind.len() > 128
            || media_type.is_empty()
            || media_type.len() > 128
        {
            bail!("artifact exceeds retention bounds");
        }
        self.purge_expired(now, 128)?;
        let _lock = objects::lock::RepoLock::at(self.database.with_extension("artifacts.lock"))
            .try_write()?
            .context("artifact mutation is already running")?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let policy = policy(&tx, spool)?;
        let duration = policy.raw_retention_seconds;
        if duration == 0 || duration > MAX_RETENTION_SECONDS {
            bail!("raw artifact retention duration is outside bounds");
        }
        let stored: Vec<u8> = tx
            .query_row("SELECT record FROM runs WHERE id=?1", [&run.id], |row| {
                row.get(0)
            })
            .optional()?
            .context("retained artifact Run unavailable")?;
        if RunRecord::decode(stored.as_slice())?.r#ref.as_ref() != Some(run) {
            bail!("artifact belongs to another Run or Spool");
        }
        let digest = blake3::hash(bytes);
        let existing:Option<Vec<u8>>=tx.query_row("SELECT record FROM run_artifacts WHERE spool=?1 AND run=?2 AND kind=?3 AND digest=?4",params![spool,run.id,kind,digest.as_bytes()],|row|row.get(0)).optional()?;
        if let Some(encoded) = existing {
            let result = RunArtifact::decode(encoded.as_slice())?;
            if result.media_type != media_type
                || result
                    .retained_until
                    .as_ref()
                    .is_none_or(|time| time.seconds <= now)
            {
                bail!("retained artifact identity expired or metadata differs");
            }
            tx.commit()?;
            self.publish_bytes(&result, bytes, now)?;
            return Ok(result);
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM run_artifacts WHERE spool=?1 AND run=?2 AND phase!=2",
            params![spool, run.id],
            |row| row.get(0),
        )?;
        if count >= MAX_RUN_ARTIFACTS as i64 {
            bail!("Run artifact count exceeds observation budget");
        }
        let expires = now
            .checked_add(i64::try_from(duration)?)
            .context("retention expiry overflow")?;
        let id = uuid::Uuid::now_v7().to_string();
        let mut record = RunArtifact {
            r#ref: Some(RecordRef {
                spool: run.spool.clone(),
                id: id.clone(),
            }),
            kind: kind.into(),
            media_type: media_type.into(),
            size: bytes.len() as u64,
            content_hash: digest.as_bytes().to_vec(),
            retained_until: Some(Default::default()),
        };
        record
            .retained_until
            .as_mut()
            .context("retention timestamp missing")?
            .seconds = expires;
        tx.execute("INSERT INTO run_artifacts(id,spool,run,kind,digest,expires,record) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,spool,run.id,kind,digest.as_bytes(),expires,record.encode_to_vec()])?;
        tx.commit()?;
        self.publish_bytes(&record, bytes, now)?;
        Ok(record)
    }
    fn publish_bytes(&self, record: &RunArtifact, bytes: &[u8], now: i64) -> Result<()> {
        let reference = record.r#ref.as_ref().context("artifact reference absent")?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        policy(&tx, scope(reference)?)?;
        if record
            .retained_until
            .as_ref()
            .is_none_or(|expiry| expiry.seconds <= now)
        {
            bail!("artifact retention expired before publication");
        }
        // Pending catalog identity already committed, so failed writes or a
        // failed availability commit remain recoverable by retry or expiry.
        objects::fs_atomic::create_private_dir_all(&self.directory)?;
        let path = self.path(&reference.id)?;
        let available: bool = tx.query_row(
            "SELECT phase=1 FROM run_artifacts WHERE id=?1",
            [&reference.id],
            |row| row.get(0),
        )?;
        if !available {
            objects::fs_atomic::write_file_atomic_secret(&path, bytes)?;
        }
        tx.execute(
            "UPDATE run_artifacts SET phase=1 WHERE id=?1 AND phase=0",
            [&reference.id],
        )?;
        tx.commit()?;
        self.changed()?;
        Ok(())
    }
    /// Indexed bounded projection; policy withdrawal hides retained raw material.
    pub fn for_run(&self, run: &RecordRef, now: i64) -> Result<Vec<RunArtifact>> {
        let spool = scope(run)?;
        let connection = self.connection()?;
        if policy(&connection, spool).is_err() {
            return Ok(Vec::new());
        }
        let mut query=connection.prepare("SELECT record FROM run_artifacts WHERE spool=?1 AND run=?2 AND expires>?3 AND phase=1 ORDER BY id LIMIT ?4")?;
        let rows = query.query_map(
            params![spool, run.id, now, (MAX_RUN_ARTIFACTS + 1) as i64],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let records = rows
            .map(|row| Ok(RunArtifact::decode(row?.as_slice())?))
            .collect::<Result<Vec<_>>>()?;
        if records.len() > MAX_RUN_ARTIFACTS {
            bail!("Run artifact projection exceeds budget");
        }
        Ok(records)
    }
    /// Recheck before each protected output, including completion.
    pub fn current(&self, reference: &RecordRef, now: i64) -> Result<RunArtifact> {
        let spool = scope(reference)?;
        let connection = self.connection()?;
        policy(&connection, spool)?;
        let record: Vec<u8> = connection
            .query_row(
                "SELECT record FROM run_artifacts WHERE id=?1 AND spool=?2 AND expires>?3 AND phase=1",
                params![reference.id, spool, now],
                |row| row.get(0),
            )
            .optional()?
            .context("retained artifact unavailable")?;
        let record = RunArtifact::decode(record.as_slice())?;
        if record.r#ref.as_ref() != Some(reference) || record.size > MAX_ARTIFACT_BYTES {
            bail!("invalid retained artifact record");
        }
        Ok(record)
    }
    /// The bounded file is hashed before any bytes are disclosed. The returned
    /// handle is positioned at zero and never resolves a caller supplied path.
    pub fn read(&self, reference: &RecordRef, now: i64) -> Result<ArtifactRead> {
        self.purge_expired(now, 128)?;
        let record = self.current(reference, now)?;
        let mut file = File::open(self.path(&reference.id)?)?;
        if file.metadata()?.len() != record.size {
            bail!("retained artifact length differs from catalog");
        }
        let mut digest = blake3::Hasher::new();
        digest.update_reader((&mut file).take(MAX_ARTIFACT_BYTES + 1))?;
        if digest.finalize().as_bytes().as_slice() != record.content_hash {
            bail!("retained artifact digest differs from catalog");
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(ArtifactRead { record, file })
    }
    /// Indexed next deadline for one shared store, independent of any observer.
    pub fn next_expiry(&self) -> Result<Option<i64>> {
        Ok(self.connection()?.query_row(
            "SELECT MIN(expires) FROM run_artifacts WHERE phase!=2",
            [],
            |row| row.get(0),
        )?)
    }

    /// Bounded physical expiration cleanup, also callable by the daemon's shared
    /// retention scheduler. Expired authority is denied by time even if unlink fails.
    /// Immutable tombstones preserve deduplication after physical deletion.
    pub fn purge_expired(&self, now: i64, limit: usize) -> Result<usize> {
        if limit == 0 || limit > 1024 {
            bail!("invalid artifact cleanup budget");
        }
        let Some(_lock) =
            objects::lock::RepoLock::at(self.database.with_extension("artifacts.lock"))
                .try_write()?
        else {
            return Ok(0);
        };
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ids = {
            let mut query = tx.prepare(
                "SELECT id FROM run_artifacts WHERE expires<=?1 AND phase!=2 ORDER BY expires,id LIMIT ?2",
            )?;
            query
                .query_map(params![now, limit as i64], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in &ids {
            match std::fs::remove_file(self.path(id)?) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            tx.execute("UPDATE run_artifacts SET phase=2 WHERE id=?1", [id])?;
        }
        tx.commit()?;
        if !ids.is_empty() {
            self.changed()?;
        }
        Ok(ids.len())
    }
}
fn scope(reference: &RecordRef) -> Result<&str> {
    if reference.id.is_empty() || reference.id.len() > 256 || reference.id.contains(['/', '\\']) {
        bail!("invalid Run or artifact identity");
    }
    let spool = &reference
        .spool
        .as_ref()
        .context("artifact Spool required")?
        .id;
    uuid::Uuid::parse_str(spool)?;
    Ok(spool)
}
fn policy(connection: &Connection, spool: &str) -> Result<RunPolicy> {
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT policy FROM run_policies WHERE spool=?1",
            [spool],
            |row| row.get(0),
        )
        .optional()?
        .context("raw retention has not been enabled")?;
    let policy = RunPolicy::decode(bytes.as_slice())?;
    if !policy.retain_raw || policy.raw_retention_seconds == 0 {
        bail!("raw retention has not been enabled");
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v2alpha1::{PutRunPolicyRequest, SpoolRef};

    use super::*;
    fn fixture() -> (
        tempfile::TempDir,
        ArtifactStore,
        crate::device_runs::RunStore,
        RecordRef,
    ) {
        let directory = tempfile::tempdir().expect("directory");
        let runs = crate::device_runs::RunStore::open(directory.path()).expect("runs");
        let reference = RecordRef {
            spool: Some(SpoolRef {
                id: uuid::Uuid::from_u128(42).to_string(),
            }),
            id: "run-1".into(),
        };
        runs.put_run(RunRecord {
            r#ref: Some(reference.clone()),
            ..Default::default()
        })
        .expect("Run");
        let artifacts = ArtifactStore::open(directory.path()).expect("artifact catalog");
        (directory, artifacts, runs, reference)
    }
    fn enable(runs: &crate::device_runs::RunStore, run: &RecordRef) {
        runs.put_policy(
            &PutRunPolicyRequest {
                client_operation_id: uuid::Uuid::now_v7().to_string(),
                policy: Some(RunPolicy {
                    spool: run.spool.clone(),
                    retain_raw: true,
                    raw_retention_seconds: 10,
                    ..Default::default()
                }),
                ..Default::default()
            },
            "owning-human",
        )
        .expect("explicit retention opt-in");
    }
    #[test]
    fn retained_artifacts_require_scope_policy_integrity_and_keep_retry_expiry() {
        let (_directory, store, runs, run) = fixture();
        assert!(
            store
                .retain(
                    &run,
                    "session-report",
                    "application/json",
                    b"private report",
                    100
                )
                .is_err(),
            "raw retention needs explicit opt-in"
        );
        assert!(!store.directory.exists(), "no bytes before opt-in");
        enable(&runs, &run);
        let first = store
            .retain(
                &run,
                "session-report",
                "application/json",
                b"private report",
                100,
            )
            .expect("retained");
        assert_eq!(
            first,
            store
                .retain(
                    &run,
                    "session-report",
                    "application/json",
                    b"private report",
                    105
                )
                .expect("exact retry"),
            "retry cannot extend retention"
        );
        assert_eq!(
            store.for_run(&run, 105).expect("discover"),
            vec![first.clone()]
        );
        let reference = first.r#ref.as_ref().expect("reference");
        let mut bytes = Vec::new();
        store
            .read(reference, 105)
            .expect("authorized read")
            .file
            .read_to_end(&mut bytes)
            .expect("bytes");
        assert_eq!(bytes, b"private report");
        let other = RecordRef {
            spool: Some(SpoolRef {
                id: uuid::Uuid::from_u128(43).to_string(),
            }),
            ..reference.clone()
        };
        assert!(
            store.read(&other, 105).is_err(),
            "artifact UUID is not cross-Spool authority"
        );
        std::fs::write(store.path(&reference.id).expect("path"), b"altered report")
            .expect("same-size mutation");
        let error = store.read(reference, 105).err().expect("integrity failure");
        assert!(
            error.to_string().contains("digest"),
            "precise byte integrity failure: {error}"
        );
        std::fs::write(store.path(&reference.id).expect("path"), b"private report")
            .expect("restore bytes");
        let policy = runs
            .policy(&run.spool.as_ref().expect("spool").id)
            .expect("policy")
            .expect("policy exists");
        runs.put_policy(
            &PutRunPolicyRequest {
                client_operation_id: uuid::Uuid::now_v7().to_string(),
                expected_version: policy.version,
                policy: Some(RunPolicy {
                    spool: run.spool.clone(),
                    retain_raw: false,
                    ..Default::default()
                }),
            },
            "owning-human",
        )
        .expect("withdraw raw access");
        assert!(
            store.current(reference, 105).is_err(),
            "current policy withdrawal fences read"
        );
        assert!(store.for_run(&run, 105).expect("hidden").is_empty());
    }
    #[test]
    fn expiration_retries_physical_cleanup_and_expired_retry_cannot_retain_again() {
        let (_directory, store, runs, run) = fixture();
        enable(&runs, &run);
        for round in 0..2 {
            let bytes = format!("private report {round}");
            let record = store
                .retain(
                    &run,
                    "session-report",
                    "application/json",
                    bytes.as_bytes(),
                    100,
                )
                .expect("retain");
            let reference = record.r#ref.as_ref().expect("reference");
            let path = store.path(&reference.id).expect("path");
            assert!(path.is_file());
            let held = path.with_extension("held");
            std::fs::rename(&path, &held).expect("hold original file");
            std::fs::create_dir(&path).expect("inject unlink failure");
            assert!(
                store.purge_expired(110, 128).is_err(),
                "unlink failure remains observable"
            );
            let phase: i32 = store
                .connection()
                .expect("db")
                .query_row(
                    "SELECT phase FROM run_artifacts WHERE id=?1",
                    [&reference.id],
                    |row| row.get(0),
                )
                .expect("retry metadata retained");
            assert_eq!(
                phase, 1,
                "failed physical removal keeps durable cleanup job"
            );
            assert!(
                store.current(reference, 110).is_err(),
                "natural expiry denies before cleanup"
            );
            std::fs::remove_dir(&path).expect("remove injected failure");
            std::fs::rename(&held, &path).expect("restore path");
            assert_eq!(store.purge_expired(110, 128).expect("retry cleanup"), 1);
            assert!(!path.exists(), "successful retry removes actual bytes");
            assert_eq!(store.purge_expired(110, 128).expect("repeat sweep"), 0);
            assert!(
                store
                    .retain(
                        &run,
                        "session-report",
                        "application/json",
                        bytes.as_bytes(),
                        111
                    )
                    .is_err(),
                "expired exact report cannot silently receive new TTL"
            );
        }
    }
    #[test]
    fn pending_catalog_precedes_file_write_and_failed_availability_repairs_exactly() {
        let (_directory, store, runs, run) = fixture();
        enable(&runs, &run);
        store.connection().expect("db").execute_batch("CREATE TRIGGER fail_artifact BEFORE INSERT ON run_artifacts BEGIN SELECT RAISE(ABORT,'injected artifact insert failure'); END;").expect("inject insert failure");
        assert!(
            store
                .retain(&run, "session-report", "application/json", b"original", 100)
                .is_err()
        );
        assert!(
            !store.directory.exists(),
            "failed SQL insert never publishes untracked bytes"
        );
        store.connection().expect("db").execute_batch("DROP TRIGGER fail_artifact; CREATE TRIGGER fail_ready BEFORE UPDATE OF phase ON run_artifacts WHEN NEW.phase=1 BEGIN SELECT RAISE(ABORT,'injected readiness failure'); END;").expect("inject availability failure");
        assert!(
            store
                .retain(&run, "session-report", "application/json", b"original", 100)
                .is_err()
        );
        let (id, phase, record): (String, i32, Vec<u8>) = store
            .connection()
            .expect("db")
            .query_row("SELECT id,phase,record FROM run_artifacts", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("durable pending identity");
        assert_eq!(phase, 0);
        assert_eq!(
            std::fs::read(store.path(&id).expect("path")).expect("pending bytes"),
            b"original"
        );
        assert!(
            store.for_run(&run, 101).expect("projection").is_empty(),
            "pending bytes are not advertised available"
        );
        store
            .connection()
            .expect("db")
            .execute_batch("DROP TRIGGER fail_ready;")
            .expect("restore availability");
        let repaired = store
            .retain(&run, "session-report", "application/json", b"original", 105)
            .expect("repair retry");
        assert_eq!(
            repaired.encode_to_vec(),
            record,
            "retry retains initial identity and expiry"
        );
        assert!(
            store
                .read(repaired.r#ref.as_ref().expect("reference"), 105)
                .is_ok()
        );
    }
}
