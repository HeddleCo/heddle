//! Receiver-owned witness trust, serialized with durable native mutations.
//!
//! Part 2 selects the HTTPS authority/descriptor root through its independent
//! descriptor trust path, then calls `select_root`. Carried proof bundles never
//! enroll a root. `mutate` authenticates a fresh complete set against durable
//! high-water/seals/tombstones and clock floors while holding SQLite's IMMEDIATE
//! transaction. Resolve staged originals anew inside that same transaction.
//! This replaces evergreen executor-key enrollment for HYBRID records.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use api::{
    heddle::api::common::SignedHostedWitnessSetV1,
    hybrid_codec::{self, Reject},
    witness_trust::{self, SetExpectation, VerifiedWitnessSet},
};
use prost::Message;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{Error, Result};

pub(crate) const SCHEMA:&str="
CREATE TABLE IF NOT EXISTS hosted_witness_trust(
 authority TEXT PRIMARY KEY,root_id TEXT NOT NULL,root_key BLOB NOT NULL CHECK(length(root_key)=32),
 root_epoch INTEGER NOT NULL CHECK(root_epoch>0),history_root_id TEXT NOT NULL,history_root_key BLOB NOT NULL CHECK(length(history_root_key)=32),
 signed_set BLOB CHECK(signed_set IS NULL OR length(signed_set)<=1048576),clock_floor INTEGER NOT NULL CHECK(clock_floor>=0));
CREATE TABLE IF NOT EXISTS hosted_import_job_keys(public_key BLOB PRIMARY KEY CHECK(length(public_key)=32),logical_job BLOB NOT NULL CHECK(length(logical_job)=16));
CREATE TABLE IF NOT EXISTS hosted_import_proofs(thread BLOB PRIMARY KEY CHECK(length(thread)=32),authority TEXT NOT NULL,bundle BLOB NOT NULL CHECK(length(bundle)<=1048576));
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

/// Receiver clock. Callers must fail if trustworthy wall time is unavailable.
/// The monotonic reading detects rollback during a process; SQLite retains the
/// last accepted wall floor across processes and restarts.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> Result<i64>;
    fn elapsed_millis(&self) -> Result<u64>;
}
pub struct SystemClock {
    start: Instant,
}
impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
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
        u64::try_from(self.start.elapsed().as_millis()).map_err(|_| Error::HostedClock)
    }
}
pub struct HostedTrust<C = SystemClock> {
    directory: PathBuf,
    authority: String,
    clock: Arc<C>,
    anchor: Arc<Mutex<Option<(i64, u64)>>>,
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

fn require_clock_progress(last: (i64, u64), now: (i64, u64)) -> Result<()> {
    let passed = now.1.checked_sub(last.1).ok_or(Error::HostedClock)?;
    // Independently truncated millisecond readings can differ by one tick.
    // This precision bound never changes the signed set's exclusive expiry.
    let floor = last
        .0
        .checked_add(i64::try_from(passed.saturating_sub(1)).map_err(|_| Error::HostedClock)?)
        .ok_or(Error::HostedClock)?;
    if now.0 < floor {
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
            anchor: Arc::new(Mutex::new(None)),
        })
    }

    /// Authenticate the fresh set and execute a native mutation under one
    /// receiver trust lock/transaction. Errors roll back trust and content.
    pub fn mutate<T>(
        &self,
        signed: &SignedHostedWitnessSetV1,
        mutation: impl FnOnce(&TrustTransaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut anchor = self.anchor.lock().map_err(|_| Error::HostedClock)?;
        let mut connection = crate::local_metadata::open(&self.directory)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row:TrustRow=tx.query_row("SELECT root_id,root_key,root_epoch,history_root_id,history_root_key,signed_set,clock_floor FROM hosted_witness_trust WHERE authority=?1",[&self.authority],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        let now = self.clock.now_millis()?;
        let elapsed = self.clock.elapsed_millis()?;
        if let Some(last) = *anchor {
            require_clock_progress(last, (now, elapsed))?;
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
            set: &set,
            root_public_key: &row.1,
            now,
            associations,
        };
        let result = mutation(&context)?;
        // Expiry and receiver-clock changes during verification cannot leave
        // an earlier opaque context authorizing the eventual durable commit.
        let commit_now = self.clock.now_millis()?;
        let commit_elapsed = self.clock.elapsed_millis()?;
        require_clock_progress((now, elapsed), (commit_now, commit_elapsed))?;
        witness_trust::verify_set(
            signed,
            &SetExpectation {
                now_unix_millis: commit_now,
                ..expected
            },
            Some(&set),
        )?;
        tx.execute("UPDATE hosted_witness_trust SET signed_set=?2,clock_floor=?3,history_root_id=root_id,history_root_key=root_key WHERE authority=?1",params![self.authority,signed.encode_to_vec(),commit_now])?;
        tx.commit()?;
        *anchor = Some((commit_now, commit_elapsed));
        Ok(result)
    }
}

/// One active trust/mutation transaction. Do not retain references beyond it.
pub struct TrustTransaction<'a> {
    tx: &'a Transaction<'a>,
    set: &'a VerifiedWitnessSet,
    root_public_key: &'a [u8],
    now: i64,
    associations: Vec<(Vec<u8>, Vec<u8>)>,
}
impl TrustTransaction<'_> {
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
    let mut connection = crate::local_metadata::open(directory)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_root_role(&tx, &replacement.public_key)?;
    let changed=tx.execute("UPDATE hosted_witness_trust SET root_id=?2,root_key=?3,root_epoch=root_epoch+1 WHERE authority=?1 AND root_id=?4 AND root_key=?5 AND root_epoch<9223372036854775807",params![expected.authority,replacement.root_id,replacement.public_key,expected.root_id,expected.public_key])?;
    if changed != 1 {
        return Err(Error::Hybrid(Reject::StaleContext));
    }
    tx.commit()?;
    Ok(())
}
