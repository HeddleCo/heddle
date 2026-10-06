//! Receiver-owned witness trust, serialized with durable native mutations.
//!
//! Part 2 selects the HTTPS authority/descriptor root through its independent
//! descriptor trust path, then calls `select_root`. Carried proof bundles never
//! enroll a root. `mutate` authenticates a fresh complete set against durable
//! high-water/seals/tombstones and clock floors while holding SQLite's IMMEDIATE
//! transaction. Resolve staged originals anew inside that same transaction.
//! This replaces evergreen executor-key enrollment for HYBRID records.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use api::{
    heddle::api::common::SignedHostedWitnessSetV1,
    hybrid_codec::{self, Reject},
    witness_trust::{self, SetExpectation, VerifiedWitnessSet},
};
use prost::Message;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use super::{
    Error, Result,
    install_artifacts::{InstallArtifacts, Installation, InstallationLock, checkpoint},
};

pub(crate) const SCHEMA:&str="
CREATE TABLE IF NOT EXISTS hosted_installation(singleton INTEGER PRIMARY KEY CHECK(singleton=1),id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS hosted_witness_trust(
 authority TEXT PRIMARY KEY,root_id TEXT NOT NULL,root_key BLOB NOT NULL CHECK(length(root_key)=32),
 root_epoch INTEGER NOT NULL CHECK(root_epoch>0),history_root_id TEXT NOT NULL,history_root_key BLOB NOT NULL CHECK(length(history_root_key)=32),
 signed_set BLOB CHECK(signed_set IS NULL OR length(signed_set)<=1048576),clock_floor INTEGER NOT NULL CHECK(clock_floor>=0));
CREATE TABLE IF NOT EXISTS hosted_import_job_keys(public_key BLOB PRIMARY KEY CHECK(length(public_key)=32),logical_job BLOB NOT NULL CHECK(length(logical_job)=16));
CREATE TABLE IF NOT EXISTS hosted_import_proofs(thread BLOB PRIMARY KEY CHECK(length(thread)=32),authority TEXT NOT NULL,bundle BLOB NOT NULL CHECK(length(bundle)<=1048576));
CREATE TABLE IF NOT EXISTS hosted_native_proofs(thread BLOB PRIMARY KEY CHECK(length(thread)=32),authority TEXT NOT NULL,bundle BLOB NOT NULL CHECK(length(bundle)<=1048576));
CREATE TABLE IF NOT EXISTS pending_native_genesis_bindings(thread BLOB PRIMARY KEY CHECK(length(thread)=32),binding BLOB NOT NULL CHECK(length(binding)<=65536));
CREATE TABLE IF NOT EXISTS hosted_native_genesis_bindings(thread BLOB PRIMARY KEY CHECK(length(thread)=32),binding BLOB NOT NULL CHECK(length(binding)<=65536));
CREATE TABLE IF NOT EXISTS hosted_spool_lineage(spool BLOB PRIMARY KEY CHECK(length(spool)=16),genesis BLOB NOT NULL CHECK(length(genesis)=32),initial_owner BLOB NOT NULL CHECK(length(initial_owner)=32));
CREATE TABLE IF NOT EXISTS hosted_import_admissions(operation BLOB PRIMARY KEY CHECK(length(operation)=32),authority TEXT NOT NULL,statement BLOB NOT NULL CHECK(length(statement)<=131072),proof BLOB CHECK(proof IS NULL OR length(proof)<=4096));
CREATE TABLE IF NOT EXISTS hosted_import_slots(logical_job BLOB NOT NULL,ref TEXT NOT NULL,slot BLOB NOT NULL CHECK(length(slot)=8),operation_digest BLOB NOT NULL CHECK(length(operation_digest)=32),PRIMARY KEY(logical_job,ref,slot));
";

/// Independent descriptor-root selection, never copied from an incoming set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSelection {
    pub authority: String,
    pub root_id: String,
    pub public_key: [u8; 32],
}

/// Read-only preparation context. `previous` retains authenticated history,
/// including expired sets; it never authorizes admission. `mutate` reloads and
/// verifies the newest durable trust after asynchronous proof lookup.
pub struct TrustSnapshot {
    pub root: RootSelection,
    pub root_epoch: u64,
    pub previous: Option<VerifiedWitnessSet>,
    pub clock_floor_millis: i64,
    pub known_job_associations: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Receiver clock. Callers must fail if trustworthy wall time is unavailable.
/// The monotonic reading uses a process-wide epoch across independent handles
/// and detects rollback during a process; SQLite retains the
/// last accepted wall floor across processes and restarts.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> Result<i64>;
    fn elapsed_millis(&self) -> Result<u64>;
}
pub struct SystemClock;
impl Default for SystemClock {
    fn default() -> Self {
        Self
    }
}
impl Clock for SystemClock {
    fn now_millis(&self) -> Result<i64> {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::HostedClock)?
            .as_millis();
        i64::try_from(ms).map_err(|_| Error::HostedClock)
    }
    fn elapsed_millis(&self) -> Result<u64> {
        static START: OnceLock<Instant> = OnceLock::new();
        u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis())
            .map_err(|_| Error::HostedClock)
    }
}
pub struct HostedTrust<C = SystemClock> {
    directory: PathBuf,
    authority: String,
    clock: Arc<C>,
    anchor: ClockAnchor,
}
impl<C> Clone for HostedTrust<C> {
    fn clone(&self) -> Self {
        Self {
            directory: self.directory.clone(),
            authority: self.authority.clone(),
            clock: Arc::clone(&self.clock),
            anchor: Arc::clone(&self.anchor),
        }
    }
}
type TrustRow = (String, Vec<u8>, i64, String, Vec<u8>, Option<Vec<u8>>, i64);

#[derive(Clone, Copy)]
struct ClockSample {
    wall: i64,
    before: u64,
    after: u64,
}
impl ClockSample {
    fn read(clock: &impl Clock) -> Result<Self> {
        let before = clock.elapsed_millis()?;
        let wall = clock.now_millis()?;
        let after = clock.elapsed_millis()?;
        if after < before {
            return Err(Error::HostedClock);
        }
        Ok(Self {
            wall,
            before,
            after,
        })
    }
}
type ClockAnchor = Arc<Mutex<Option<ClockSample>>>;
// Retain anchors for the process lifetime, including after the last handle is
// dropped. A rejected rollback cannot be cleared by opening the store again.
fn shared_anchor(directory: &Path, authority: &str) -> Result<ClockAnchor> {
    type Anchors = BTreeMap<(PathBuf, String), ClockAnchor>;
    static ANCHORS: OnceLock<Mutex<Anchors>> = OnceLock::new();
    let key = (directory.canonicalize()?, authority.to_owned());
    let mut anchors = ANCHORS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| Error::HostedClock)?;
    Ok(Arc::clone(anchors.entry(key).or_default()))
}

fn require_clock_progress(last: ClockSample, now: ClockSample) -> Result<()> {
    // Only time outside both sampling intervals is known to have elapsed
    // between wall readings. Scheduling inside either interval is unbounded.
    let passed = now
        .before
        .checked_sub(last.after)
        .ok_or(Error::HostedClock)?;
    // Independently truncated millisecond readings can differ by one tick.
    // This precision bound never changes the signed set's exclusive expiry.
    let floor = last
        .wall
        .checked_add(i64::try_from(passed.saturating_sub(1)).map_err(|_| Error::HostedClock)?)
        .ok_or(Error::HostedClock)?;
    if now.wall < floor {
        return Err(Error::HostedClock);
    }
    Ok(())
}

impl<C: Clock> HostedTrust<C> {
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
    /// Open an already selected authority. This does not enroll any key.
    pub fn open(directory: &Path, authority: &str, clock: C) -> Result<Self> {
        api::import_authority::canonical_https(authority, true)?;
        let connection = crate::local_metadata::open(directory)?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM hosted_witness_trust WHERE authority=?1)",
            [authority],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::Hybrid(Reject::Root));
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            authority: authority.into(),
            clock: Arc::new(clock),
            anchor: shared_anchor(directory, authority)?,
        })
    }

    pub fn snapshot(&self) -> Result<TrustSnapshot> {
        let _serialization = InstallationLock::acquire(&self.directory)?;
        let mut anchor = self.anchor.lock().map_err(|_| Error::HostedClock)?;
        let mut connection = Connection::open_with_flags(
            self.directory.join(crate::local_metadata::DATABASE_NAME),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let tx = connection.transaction()?;
        let row: TrustRow = tx.query_row("SELECT root_id,root_key,root_epoch,history_root_id,history_root_key,signed_set,clock_floor FROM hosted_witness_trust WHERE authority=?1", [&self.authority], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        let sampled = ClockSample::read(self.clock.as_ref())?;
        let now = sampled.wall;
        if now < row.6 {
            return Err(Error::HostedClock);
        }
        if let Some(last) = *anchor {
            require_clock_progress(last, sampled)?;
        }
        let associations = job_associations(&tx)?;
        let jobs = associations
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let root_epoch = u64::try_from(row.2).map_err(|_| Error::Hybrid(Reject::Bounds))?;
        let previous = row
            .5
            .as_ref()
            .map(|bytes| {
                let signed: SignedHostedWitnessSetV1 =
                    hybrid_codec::strict_decode(bytes, witness_trust::MAX_SET_BYTES)?;
                Ok::<_, Error>(witness_trust::restore_history_snapshot(
                    &signed,
                    &SetExpectation {
                        authority: &self.authority,
                        root_id: &row.3,
                        root_public_key: &row.4,
                        root_epoch,
                        now_unix_millis: now,
                        clock_floor_unix_millis: row.6,
                        known_job_keys: &jobs,
                    },
                )?)
            })
            .transpose()?;
        *anchor = Some(sampled);
        Ok(TrustSnapshot {
            root: RootSelection {
                authority: self.authority.clone(),
                root_id: row.0,
                public_key: row
                    .1
                    .try_into()
                    .map_err(|_| Error::Hybrid(Reject::Bounds))?,
            },
            root_epoch,
            previous,
            clock_floor_millis: row.6,
            known_job_associations: associations,
        })
    }

    /// Authenticate the fresh set and execute a native mutation under one
    /// receiver trust lock/transaction. Errors roll back trust and content.
    pub fn mutate<T>(
        &self,
        signed: &SignedHostedWitnessSetV1,
        mutation: impl FnOnce(&TrustTransaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.mutate_validated(signed, mutation, |_, _| Ok(()))
    }

    /// Validate current access with freshly sampled receiver time immediately
    /// before commit, while the same trust/content transaction remains locked.
    pub fn mutate_validated<T>(
        &self,
        signed: &SignedHostedWitnessSetV1,
        mutation: impl FnOnce(&TrustTransaction<'_>) -> Result<T>,
        validate_commit: impl FnOnce(&TrustTransaction<'_>, i64) -> Result<()>,
    ) -> Result<T> {
        self.mutate_with_artifacts(
            signed,
            mutation,
            |_, _| Ok(()),
            |_, _| Ok(()),
            validate_commit,
        )
    }

    pub(super) fn mutate_with_artifacts<T>(
        &self,
        signed: &SignedHostedWitnessSetV1,
        mutation: impl FnOnce(&TrustTransaction<'_>) -> Result<T>,
        validate_install: impl FnOnce(&TrustTransaction<'_>, i64) -> Result<()>,
        before_commit: impl FnOnce(&TrustTransaction<'_>, &mut InstallArtifacts<'_>) -> Result<()>,
        validate_commit: impl FnOnce(&TrustTransaction<'_>, i64) -> Result<()>,
    ) -> Result<T> {
        let serialization = InstallationLock::acquire(&self.directory)?;
        let mut anchor = self.anchor.lock().map_err(|_| Error::HostedClock)?;
        let mut connection = crate::local_metadata::open(&self.directory)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row:TrustRow=tx.query_row("SELECT root_id,root_key,root_epoch,history_root_id,history_root_key,signed_set,clock_floor FROM hosted_witness_trust WHERE authority=?1",[&self.authority],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        let sampled = ClockSample::read(self.clock.as_ref())?;
        let now = sampled.wall;
        if let Some(last) = *anchor {
            require_clock_progress(last, sampled)?;
        } else {
            *anchor = Some(sampled);
        }
        let associations = job_associations(&tx)?;
        let jobs = associations
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        let expected = SetExpectation {
            authority: &self.authority,
            root_id: &row.0,
            root_public_key: &row.1,
            root_epoch: u64::try_from(row.2).map_err(|_| Error::Hybrid(Reject::Bounds))?,
            now_unix_millis: now,
            clock_floor_unix_millis: row.6,
            known_job_keys: &jobs,
        };
        let previous = if let Some(bytes) = &row.5 {
            let previous: SignedHostedWitnessSetV1 =
                hybrid_codec::strict_decode(bytes, witness_trust::MAX_SET_BYTES)?;
            let selected = SetExpectation {
                root_id: &row.3,
                root_public_key: &row.4,
                ..expected
            };
            Some(witness_trust::restore_history_snapshot(
                &previous, &selected,
            )?)
        } else {
            None
        };
        let set = witness_trust::verify_set(signed, &expected, previous.as_ref())?;
        let context = TrustTransaction {
            tx: &tx,
            serialization: &serialization,
            set: &set,
            root_public_key: &row.1,
            now,
            associations,
        };
        let result = mutation(&context)?;
        let install_sample = ClockSample::read(self.clock.as_ref())?;
        let install_now = install_sample.wall;
        require_clock_progress(sampled, install_sample)?;
        if install_now < set.body().issued_at_unix_millis
            || install_now >= set.body().valid_until_unix_millis
        {
            return Err(Error::Hybrid(Reject::Expired));
        }
        validate_install(&context, install_now)?;
        let mut artifacts = Installation::begin(&serialization)?;
        let committed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            before_commit(&context, &mut artifacts.writer())?;
            // Finish filesystem binding checks and stage the SQL marker before
            // sampling final authority freshness; these checks can involve I/O.
            artifacts.mark(&tx)?;
            // Expiry and receiver-clock changes during verification cannot leave
            // an earlier opaque context authorizing the eventual durable commit.
            let commit_sample = ClockSample::read(self.clock.as_ref())?;
            let commit_now = commit_sample.wall;
            require_clock_progress(install_sample, commit_sample)?;
            witness_trust::verify_set(
                signed,
                &SetExpectation {
                    now_unix_millis: commit_now,
                    ..expected
                },
                Some(&set),
            )?;
            // Signature verification can itself take time. Sample again after it,
            // retaining millisecond freshness for the final current-access hook.
            let final_sample = ClockSample::read(self.clock.as_ref())?;
            let final_now = final_sample.wall;
            require_clock_progress(commit_sample, final_sample)?;
            if final_now < set.body().issued_at_unix_millis
                || final_now >= set.body().valid_until_unix_millis
            {
                return Err(Error::Hybrid(Reject::Expired));
            }
            tx.execute("UPDATE hosted_witness_trust SET signed_set=?2,clock_floor=?3,history_root_id=root_id,history_root_key=root_key WHERE authority=?1",params![self.authority,signed.encode_to_vec(),final_now])?;
            validate_commit(&context, final_now)?;
            Ok(final_sample)
        }));
        let final_sample = match committed {
            Ok(Ok(time)) => time,
            rejection => {
                // Destroy SQL before filesystem undo, while cross-process
                // serialization remains held. Cleanup never replaces rejection.
                if let Err(error) = tx.rollback() {
                    tracing::error!(%error, "installation SQL rollback failed");
                }
                drop(connection);
                if let Err(error) = artifacts.rollback() {
                    tracing::error!(%error, "installation undo retained for recovery; repository unavailable");
                }
                match rejection {
                    Ok(Err(error)) => return Err(error),
                    Err(panic) => {
                        drop(artifacts);
                        drop(anchor);
                        drop(serialization);
                        std::panic::resume_unwind(panic);
                    }
                    Ok(Ok(_)) => return Err(Error::Invalid("invalid installation outcome".into())),
                }
            }
        };
        checkpoint("before-commit");
        if let Err(error) = tx.commit() {
            // Transaction::commit consumes and rolls back on failure. Closing
            // the connection also orders destruction before marker-based undo.
            drop(connection);
            drop(artifacts);
            if let Err(recovery) = serialization.recover() {
                tracing::error!(%recovery, "installation commit recovery retained; repository unavailable");
            }
            return Err(error.into());
        }
        checkpoint("commit");
        *anchor = Some(final_sample);
        artifacts.finish()?;
        Ok(result)
    }
}

/// One active trust/mutation transaction. Do not retain references beyond it.
pub struct TrustTransaction<'a> {
    tx: &'a Transaction<'a>,
    serialization: &'a InstallationLock,
    set: &'a VerifiedWitnessSet,
    root_public_key: &'a [u8],
    now: i64,
    associations: Vec<(Vec<u8>, Vec<u8>)>,
}
impl TrustTransaction<'_> {
    /// Current catalog and source access checks borrow the held installation
    /// serialization and SQL snapshot instead of reentering repository readers.
    pub fn device_spool(
        &self,
        home: &Path,
        id: uuid::Uuid,
    ) -> anyhow::Result<crate::device_catalog::DeviceSpool> {
        crate::device_catalog::load_serialized(home, id, self.serialization)
    }
    pub fn device_thread_visible(
        &self,
        thread: super::ContentHash,
        spool: uuid::Uuid,
        principal: uuid::Uuid,
        agent: Option<&str>,
    ) -> Result<bool> {
        let replica = super::ThreadReplica {
            path: self
                .serialization
                .directory()
                .join(crate::local_metadata::DATABASE_NAME),
            thread,
        };
        if replica.ownership_claims_with_admission_in(self.tx)?.len() > 1
            && replica.ownership_resolution_in(self.tx)?.is_none()
        {
            return Ok(false);
        }
        let genesis = replica.genesis_in(self.tx)?;
        if genesis.spool != spool.to_string() {
            return Err(Error::Hybrid(Reject::Scope));
        }
        let local = if let objects::object::thread_replication::GenesisOwner::LocalKey(key) =
            genesis.owner
        {
            super::local::holds_native_owner_key_serialized(&key, self.serialization)?
                .then_some(key)
        } else {
            None
        };
        replica.audience_allows_in(self.tx, principal, agent, true, local.as_ref())
    }

    pub fn property_version(
        &self,
        thread: super::ContentHash,
        property: &objects::object::thread_replication::metadata::Property,
    ) -> Result<super::ContentHash> {
        let replica = super::ThreadReplica {
            path: self
                .serialization
                .directory()
                .join(crate::local_metadata::DATABASE_NAME),
            thread,
        };
        let heads = replica
            .metadata_frontier_in(self.tx, property)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        Ok(
            objects::object::thread_replication::metadata::property_version(
                thread, property, &heads,
            )?,
        )
    }
    /// Compare every accepted owner context with independently selected local
    /// Spool lineage. Neither a witness nor a proof can enroll a foreign root.
    pub fn require_spool_selection(
        &self,
        selection: &heddleco_capability_verifier::import_delegation::Selection<'_>,
    ) -> Result<()> {
        let stored: Option<(Vec<u8>, Vec<u8>)> = self
            .tx
            .query_row(
                "SELECT genesis,initial_owner FROM hosted_spool_lineage WHERE spool=?1",
                [selection.keyring.owner_genesis().spool_uuid()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if stored.as_ref().is_none_or(|(digest, owner)| {
            digest != selection.spool_genesis_digest || owner != selection.initial_owner_id
        }) {
            return Err(Error::Hybrid(Reject::Root));
        }
        Ok(())
    }
    pub fn set(&self) -> &VerifiedWitnessSet {
        self.set
    }
    pub fn now_millis(&self) -> i64 {
        self.now
    }
    pub fn job_associations(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.associations
    }
    pub(super) fn import_witness_pin(&self) -> api::import_authority::ImportWitnessRootPin {
        api::import_authority::ImportWitnessRootPin {
            authority: self.set.body().deployment_authority.clone(),
            root_id: self.set.body().descriptor_root_id.clone(),
            public_key: self.root_public_key.to_vec(),
            epoch: self.set.root_epoch(),
        }
    }
    /// Restore receiver-owned history while the trust and installation lock is
    /// held. Sibling histories share this transaction without replacing each other.
    pub(super) fn import_witness_snapshot(
        &self,
    ) -> Result<Option<api::import_authority::ImportWitnessSnapshot>> {
        let row: (String, Vec<u8>, Option<Vec<u8>>, i64) = self.tx.query_row(
            "SELECT history_root_id,history_root_key,signed_set,clock_floor FROM hosted_witness_trust WHERE authority=?1",
            [&self.set.body().deployment_authority],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let Some(bytes) = row.2 else {
            return Ok(None);
        };
        let mut statement = self
            .tx
            .prepare("SELECT DISTINCT bundle FROM hosted_import_proofs WHERE authority=?1")?;
        let history = statement
            .query_map([&self.set.body().deployment_authority], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .map(|bytes| {
                Ok(hybrid_codec::strict_decode(
                    &bytes?,
                    api::import_authority::MAX_BUNDLE_BYTES,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut pin = self.import_witness_pin();
        if row.0 != pin.root_id || row.1 != pin.public_key {
            pin.epoch = pin
                .epoch
                .checked_sub(1)
                .filter(|e| *e > 0)
                .ok_or(Reject::StaleContext)?;
        }
        pin.root_id = row.0;
        pin.public_key = row.1;
        Ok(Some(api::import_authority::ImportWitnessSnapshot {
            root: pin,
            witness_set: hybrid_codec::strict_decode(&bytes, witness_trust::MAX_SET_BYTES)?,
            clock_floor_unix_millis: row.3,
            job_associations: self.associations.clone(),
            accepted_history: history,
        }))
    }
    pub fn forbidden_job_keys(&self) -> Vec<Vec<u8>> {
        std::iter::once(self.root_public_key.to_vec())
            .chain(self.set.body().entries.iter().map(|e| e.public_key.clone()))
            .collect()
    }
    pub(super) fn sql(&self) -> &Transaction<'_> {
        self.tx
    }
    /// Persist only a cryptographically verified owner/job association.
    pub fn retain_delegation(
        &self,
        delegation: &heddleco_capability_verifier::import_delegation::VerifiedImportDelegation,
    ) -> Result<()> {
        let d = delegation.scope().body();
        if self.forbidden_job_keys().contains(&d.job_public_key) {
            return Err(Error::Hybrid(Reject::KeyRole));
        }
        let existing: Option<Vec<u8>> = self
            .tx
            .query_row(
                "SELECT logical_job FROM hosted_import_job_keys WHERE public_key=?1",
                [&d.job_public_key],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_ref().is_some_and(|j| j != &d.logical_job_id) {
            return Err(Error::Hybrid(Reject::Scope));
        }
        self.tx.execute(
            "INSERT OR IGNORE INTO hosted_import_job_keys(public_key,logical_job) VALUES(?1,?2)",
            params![d.job_public_key, d.logical_job_id],
        )?;
        Ok(())
    }
    pub(super) fn retain_slot(
        &self,
        operation: &api::heddle::api::v1alpha2::SignedDelegatedImportOperationV1,
    ) -> Result<()> {
        let o = operation.body.as_ref().ok_or(Reject::Canonical)?;
        let digest = api::import_authority::signed_operation_digest(operation)?;
        let existing:Option<Vec<u8>>=self.tx.query_row("SELECT operation_digest FROM hosted_import_slots WHERE logical_job=?1 AND ref=?2 AND slot=?3",params![o.logical_job_id,o.ref_name,o.slot_id.to_be_bytes()],|r|r.get(0)).optional()?;
        if existing.as_ref().is_some_and(|d| d != &digest) {
            return Err(Error::Hybrid(Reject::SlotConflict));
        }
        self.tx.execute("INSERT OR IGNORE INTO hosted_import_slots(logical_job,ref,slot,operation_digest) VALUES(?1,?2,?3,?4)",params![o.logical_job_id,o.ref_name,o.slot_id.to_be_bytes(),digest])?;
        Ok(())
    }
}

/// Explicit independent Spool selection. Foreign installation retains public
/// evidence without enrolling this owner as the receiver's account identity.
pub fn select_spool(
    directory: &Path,
    spool: [u8; 16],
    genesis: [u8; 32],
    initial_owner: [u8; 32],
) -> Result<()> {
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT genesis,initial_owner FROM hosted_spool_lineage WHERE spool=?1",
            [spool],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if existing
        .as_ref()
        .is_some_and(|(g, o)| g != &genesis || o != &initial_owner)
    {
        return Err(Error::Hybrid(Reject::Root));
    }
    tx.execute(
        "INSERT OR IGNORE INTO hosted_spool_lineage(spool,genesis,initial_owner) VALUES(?1,?2,?3)",
        params![spool, genesis, initial_owner],
    )?;
    tx.commit()?;
    Ok(())
}

fn job_associations(tx: &Transaction<'_>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut q = tx
        .prepare("SELECT public_key,logical_job FROM hosted_import_job_keys ORDER BY public_key")?;
    Ok(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn require_root_role(tx: &Transaction<'_>, key: &[u8; 32]) -> Result<()> {
    let job: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_import_job_keys WHERE public_key=?1)",
        [key],
        |r| r.get(0),
    )?;
    if job {
        return Err(Error::Hybrid(Reject::KeyRole));
    }
    Ok(())
}

/// Explicit enrollment from an independently authenticated descriptor context.
/// Repeated identical selection is idempotent; replacement is explicit below.
pub fn select_root(directory: &Path, selected: &RootSelection) -> Result<()> {
    api::import_authority::canonical_https(&selected.authority, true)?;
    if selected.root_id.is_empty() || selected.root_id.len() > 256 {
        return Err(Error::Hybrid(Reject::Bounds));
    }
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_root_role(&tx, &selected.public_key)?;
    let old: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT root_id,root_key FROM hosted_witness_trust WHERE authority=?1",
            [&selected.authority],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if old
        .as_ref()
        .is_some_and(|(id, key)| id != &selected.root_id || key != &selected.public_key)
    {
        return Err(Error::Hybrid(Reject::Root));
    }
    tx.execute("INSERT OR IGNORE INTO hosted_witness_trust(authority,root_id,root_key,root_epoch,history_root_id,history_root_key,clock_floor) VALUES(?1,?2,?3,1,?2,?3,0)",params![selected.authority,selected.root_id,selected.public_key])?;
    tx.commit()?;
    Ok(())
}

/// Explicit routine out-of-band replacement preserves the last authenticated
/// history checkpoint. This is not compromised-root recovery; no new archive
/// signed by a compromised root can manufacture pre-compromise provenance.
pub fn replace_root(
    directory: &Path,
    expected: &RootSelection,
    replacement: &RootSelection,
) -> Result<()> {
    if expected.authority != replacement.authority {
        return Err(Error::Hybrid(Reject::Root));
    }
    if expected == replacement {
        return select_root(directory, expected);
    }
    if replacement.root_id.is_empty() || replacement.root_id.len() > 256 {
        return Err(Error::Hybrid(Reject::Bounds));
    }
    let _serialization = InstallationLock::acquire(directory)?;
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_root_role(&tx, &replacement.public_key)?;
    // Retained history may cross exactly one independently selected epoch.
    // Admit a set under that root before selecting another replacement.
    let changed=tx.execute("UPDATE hosted_witness_trust SET root_id=?2,root_key=?3,root_epoch=root_epoch+1 WHERE authority=?1 AND root_id=?4 AND root_key=?5 AND root_epoch<9223372036854775807 AND (signed_set IS NULL OR (root_id=history_root_id AND root_key=history_root_key))",params![expected.authority,replacement.root_id,replacement.public_key,expected.root_id,expected.public_key])?;
    if changed != 1 {
        return Err(Error::Hybrid(Reject::StaleContext));
    }
    tx.commit()?;
    Ok(())
}
