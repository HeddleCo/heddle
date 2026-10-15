//! Local Thread creation and capture use the same immutable records as remote peers.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crypto::{
    Ed25519Signer, Signer as _,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    error::HeddleError,
    object::{
        ContentHash, State, StateId, VisibilityTier,
        thread_replication::{
            SourceAuthor, ThreadFacet, ThreadGenesis, ThreadOperation, ThreadOperationBody,
            local_integration::{self, LocalIntegration},
        },
    },
    store::ObjectStore as _,
};
use rusqlite::{OptionalExtension as _, params};

use super::{Admission, Error, Result, ThreadReplica, install_artifacts::InstallationLock};
use crate::Repository;

pub(super) fn holds_native_owner_key_serialized(
    key: &[u8; 32],
    serialization: &InstallationLock,
) -> Result<bool> {
    if let Some(local) = crate::identity::load_local(
        &serialization
            .directory()
            .join(crate::identity::LOCAL_IDENTITY_FILE),
    )? && Ed25519Signer::from_pem(&local.private_key_pem)?.public_key() == key
    {
        return Ok(true);
    }
    if let Some(device) = crate::identity::load_device(&crate::identity::device_identity_path())?
        && Ed25519Signer::from_pem(&device.private_key_pem)?.public_key() == key
    {
        return Ok(true);
    }
    Ok(false)
}

impl Repository {
    /// Read-only proof that this device still holds the unclaimed owner's key.
    /// Looking at a Thread must never mint a replacement identity.
    pub fn holds_native_owner_key(&self, key: &[u8; 32]) -> Result<bool> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        self.holds_native_owner_key_serialized(key, &serialization)
    }
    pub(crate) fn holds_native_owner_key_serialized(
        &self,
        key: &[u8; 32],
        serialization: &InstallationLock,
    ) -> Result<bool> {
        if self.heddle_dir().canonicalize()? != serialization.directory() {
            return Err(Error::Invalid("source repository changed".into()));
        }
        holds_native_owner_key_serialized(key, serialization)
    }
    /// Stable local/hosted spool identity, created before the first Thread.
    pub fn native_spool_id(&self) -> Result<uuid::Uuid> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let id = self.native_spool_id_locked(None)?;
        crate::device_catalog::register(&crate::identity::heddle_home_dir(), self, id)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        Ok(id)
    }
    /// Clone installs the source identity before creating local Thread records.
    pub fn install_native_spool_id(&self, id: uuid::Uuid) -> Result<()> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        self.native_spool_id_locked(Some(id))?;
        crate::device_catalog::register(&crate::identity::heddle_home_dir(), self, id)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        Ok(())
    }
    fn native_spool_id_locked(&self, desired: Option<uuid::Uuid>) -> Result<uuid::Uuid> {
        if desired.is_some_and(|id| id.is_nil()) {
            return Err(Error::Invalid("spool identity must not be nil".into()));
        }
        let path = self.heddle_dir().join("spool-id");
        match std::fs::read_to_string(&path) {
            Ok(value) => {
                let id = uuid::Uuid::parse_str(value.trim())
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                if id.is_nil() || desired.is_some_and(|expected| id != expected) {
                    return Err(Error::Invalid(
                        "spool identity conflicts with this repository".into(),
                    ));
                }
                Ok(id)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let id = desired.unwrap_or_else(uuid::Uuid::now_v7);
                objects::fs_atomic::write_file_atomic(&path, id.to_string().as_bytes())?;
                Ok(id)
            }
            Err(error) => Err(error.into()),
        }
    }
    // Global order: repo.lock + recovery, then native-identity.lock, then SQL.
    // Callers retain serialization and the identity guard through all reads,
    // catalog registration, signing and admission. Capture already owns
    // repo.lock; its same-thread reentrant acquisition preserves this order.
    fn native_identity_lock(
        &self,
        _serialization: &InstallationLock,
    ) -> Result<objects::lock::WriteLockGuard> {
        objects::fs_atomic::create_dir_all_durable(self.heddle_dir())?;
        objects::lock::RepoLock::at(self.heddle_dir().join("locks/native-identity.lock"))
            .write()
            .map_err(|error| Error::Invalid(error.to_string()))
    }
    /// Enrollment never changes the signer of an unclaimed local Thread.
    pub fn native_thread_signer(&self, replica: &ThreadReplica) -> Result<Ed25519Signer> {
        self.native_thread_signer_at(replica, &crate::identity::heddle_home_dir())
    }
    /// Recover the immutable local owner's retained signer for an explicit
    /// conflict resolution, even while effective ownership is unavailable.
    pub fn native_original_owner_signer(&self, replica: &ThreadReplica) -> Result<Ed25519Signer> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        let objects::object::thread_replication::GenesisOwner::LocalKey(owner) =
            replica.genesis()?.owner
        else {
            return Err(Error::Invalid(
                "Thread genesis has no local owner key".into(),
            ));
        };
        if let Some(local) = crate::identity::load_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )? {
            let signer = Ed25519Signer::from_pem(&local.private_key_pem)?;
            if signer.public_key() == owner {
                return Ok(signer);
            }
        }
        if let Some(device) =
            crate::identity::load_device(&crate::identity::device_identity_path())?
        {
            let signer = Ed25519Signer::from_pem(&device.private_key_pem)?;
            if signer.public_key() == owner {
                return Ok(signer);
            }
        }
        Err(Error::Invalid(
            "original local owner key is unavailable on this device".into(),
        ))
    }
    pub(crate) fn native_thread_signer_at(
        &self,
        replica: &ThreadReplica,
        home: &std::path::Path,
    ) -> Result<Ed25519Signer> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        let objects::object::thread_replication::GenesisOwner::LocalKey(owner) =
            replica.effective_owner()?
        else {
            let pem = match crate::identity::load_device(
                &home.join(crate::identity::DEVICE_IDENTITY_FILE),
            )? {
                Some(device) => device.private_key_pem,
                None => {
                    crate::identity::load_local(
                        &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
                    )?
                    .ok_or_else(|| {
                        Error::Invalid("account Thread signing key is not available".into())
                    })?
                    .private_key_pem
                }
            };
            return Ok(Ed25519Signer::from_pem(&pem)?);
        };
        let local = crate::identity::load_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )?;
        if let Some(local) = local {
            let signer = Ed25519Signer::from_pem(&local.private_key_pem)?;
            if signer.public_key() == owner {
                return Ok(signer);
            }
        }
        if let Some(device) =
            crate::identity::load_device(&home.join(crate::identity::DEVICE_IDENTITY_FILE))?
        {
            let signer = Ed25519Signer::from_pem(&device.private_key_pem)?;
            if signer.public_key() == owner {
                return Ok(signer);
            }
        }
        Err(Error::Invalid(
            "unclaimed Thread requires its retained original owner key".into(),
        ))
    }

    fn new_local_thread_signer(&self) -> Result<Ed25519Signer> {
        // An unclaimed Thread must outlive account enrollment and device-key
        // rotation. Its repository-owned key remains until an explicit claim.
        let local = crate::identity::load_or_mint_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )?;
        Ok(Ed25519Signer::from_pem(&local.private_key_pem)?)
    }
    /// Lookup never creates a Thread or invents a publisher signature.
    pub fn native_thread(&self, name: &str) -> Result<ThreadReplica> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        let path = self.heddle_dir().join(crate::local_metadata::DATABASE_NAME);
        if !path.exists() {
            return Err(Error::Invalid(format!(
                "Thread {name:?} has no native identity"
            )));
        }
        // WAL databases cannot be opened SQLITE_OPEN_READ_ONLY; lookup still
        // refuses to create a missing database (the exists() check above).
        let connection = super::install_artifacts::InstallationConnection::open(
            self.heddle_dir(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        let id: Option<Vec<u8>> = connection
            .query_row(
                "SELECT thread FROM local_thread_names WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let id =
            id.ok_or_else(|| Error::Invalid(format!("Thread {name:?} has no native identity")))?;
        ThreadReplica::open(self.heddle_dir(), super::hash(&id)?)
    }

    /// Named local replicas. Lookup never creates a Thread or invents a signature.
    pub fn list_native_threads(&self) -> Result<Vec<(String, ThreadReplica)>> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        let path = self.heddle_dir().join(crate::local_metadata::DATABASE_NAME);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let connection = super::install_artifacts::InstallationConnection::open(
            self.heddle_dir(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        let table_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_thread_names')",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(Vec::new());
        }
        let mut statement =
            connection.prepare("SELECT name, thread FROM local_thread_names ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut named = Vec::new();
        for row in rows {
            let (name, id) = row?;
            named.push((
                name,
                ThreadReplica::open(self.heddle_dir(), super::hash(&id)?)?,
            ));
        }
        Ok(named)
    }

    /// Persist the creator's genesis before checkout work begins. Repeating exact
    /// creation reuses the original signature even after device key rotation.
    pub fn create_native_thread(
        &self,
        name: &str,
        base: StateId,
        parent: Option<&str>,
        intent: &str,
    ) -> Result<ThreadReplica> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let spool = self.native_spool_id_locked(None)?;
        crate::device_catalog::register(&crate::identity::heddle_home_dir(), self, spool)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        let parent_id = parent
            .map(|name| self.native_thread(name).map(|replica| replica.thread_id()))
            .transpose()?;
        let database = self.heddle_dir().join(crate::local_metadata::DATABASE_NAME);
        if database.exists() {
            let connection = rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?;
            let table_exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='local_thread_names')",
                [],
                |row| row.get(0),
            )?;
            if table_exists {
                let id: Option<Vec<u8>> = connection
                    .query_row(
                        "SELECT thread FROM local_thread_names WHERE name=?1",
                        [name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(id) = id {
                    let existing = ThreadReplica::open(self.heddle_dir(), super::hash(&id)?)?;
                    let genesis = existing.genesis()?;
                    if genesis.spool != spool.to_string()
                        || genesis.base != base
                        || genesis.parent != parent_id
                        || genesis.intent != intent
                    {
                        return Err(Error::Invalid(
                            "local Thread name is already bound to another genesis".into(),
                        ));
                    }
                    return Ok(existing);
                }
            }
        }
        if self.store().get_state(&base)?.is_none() {
            return Err(Error::Invalid("Thread base state is unavailable".into()));
        }
        let signer = self.new_local_thread_signer()?;
        let creator = signer
            .public_key()
            .try_into()
            .map_err(|_| Error::Invalid("invalid local signing key".into()))?;
        let genesis = ThreadGenesis {
            version: 1,
            spool: spool.to_string(),
            parent: parent_id,
            base,
            name: name.into(),
            intent: intent.into(),
            creator,
            owner: objects::object::thread_replication::GenesisOwner::LocalKey(creator),
            nonce: Vec::new(),
        };
        let signed = SignedGenesis::sign(&genesis, &signer)?;
        let replica = ThreadReplica::create(self.heddle_dir(), &signed)?;
        replica.validate_local_source_possession(self.store(), base)?;
        replica.connect()?.execute(
            "INSERT INTO local_thread_names(name,thread) VALUES(?1,?2)",
            params![name, replica.thread_id().as_bytes()],
        )?;
        Ok(replica)
    }
    /// Rename the local address without changing the original Thread genesis.
    pub fn rename_native_thread(&self, old: &str, new: &str) -> Result<()> {
        if new.is_empty() {
            return Err(Error::Invalid("Thread name is empty".into()));
        }
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let replica = self.native_thread(old)?;
        let connection = replica.connect()?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_thread_names WHERE name=?1)",
            [new],
            |row| row.get(0),
        )?;
        if exists {
            return Err(Error::Invalid(
                "destination Thread name already exists".into(),
            ));
        }
        connection.execute(
            "UPDATE local_thread_names SET name=?1 WHERE name=?2 AND thread=?3",
            params![new, old, replica.thread_id().as_bytes()],
        )?;
        Ok(())
    }

    /// Clone/pull binds a display name to the original signed Thread received
    /// from its creator. It cannot repurpose a different local Thread name.
    pub fn adopt_native_thread(&self, name: &str, signed: &SignedGenesis) -> Result<ThreadReplica> {
        let genesis = signed.verify()?;
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let spool =
            uuid::Uuid::parse_str(&genesis.spool).map_err(|e| Error::Invalid(e.to_string()))?;
        self.native_spool_id_locked(Some(spool))?;
        let replica = ThreadReplica::create(self.heddle_dir(), signed)?;
        let connection = replica.connect()?;
        let existing: Option<Vec<u8>> = connection
            .query_row(
                "SELECT thread FROM local_thread_names WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if existing
            .as_deref()
            .is_some_and(|id| id != replica.thread_id().as_bytes())
        {
            return Err(Error::Invalid(
                "local Thread name already has another identity".into(),
            ));
        }
        connection.execute(
            "INSERT OR IGNORE INTO local_thread_names(name,thread) VALUES(?1,?2)",
            params![name, replica.thread_id().as_bytes()],
        )?;
        Ok(replica)
    }
    /// Record a locally produced capture once. Causal edges follow that capture's
    /// actual parents; another checkout's frontier never becomes an implicit merge.
    pub fn record_native_capture(&self, name: &str, state_id: StateId) -> Result<ContentHash> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let replica = self.native_thread(name)?;
        if let Some(existing) = replica.source_operation_page(state_id, None, 1)?.first() {
            replica.validate_local_source_possession(self.store(), state_id)?;
            return Ok(*existing);
        }
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?;
        let base = replica.genesis()?.base;
        let mut parents = BTreeSet::new();
        for parent in &state.parents {
            if *parent == base {
                continue;
            }
            let operations = replica.source_operation_page(*parent, None, 1024)?;
            if operations.is_empty() {
                return Err(Error::Invalid(format!(
                    "capture parent {} has no admitted native operation",
                    parent.to_string_full()
                )));
            }
            parents.extend(operations);
        }
        let signer = self.native_thread_signer(&replica)?;
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents,
            publisher: signer
                .public_key()
                .try_into()
                .map_err(|_| Error::Invalid("invalid publisher key".into()))?,
            body: ThreadOperationBody::Capture(
                objects::object::thread_replication::AuthoredCapture {
                    result: replica.prepare_capture(self, &state)?,
                    author: replica.source_author_for(
                        &signer
                            .public_key()
                            .try_into()
                            .map_err(|_| Error::Invalid("source signer length".into()))?,
                    )?,
                },
            ),
        };
        let signed = SignedOperation::sign(&operation, &signer)?;
        match replica.receive_prepared_source(&signed, self.store(), |_| Ok(()))? {
            Admission::Accepted => Ok(operation.id()?),
            other => Err(Error::Invalid(format!(
                "local capture was not admitted: {other:?}"
            ))),
        }
    }

    /// Record a capture or local integration on a named native Thread.
    /// Missing native identity is created here — capture owns genesis when
    /// overlay init or pull left an attached thread unsigned. Damaged mapping
    /// still fails before publishing a checkout ref.
    /// Cross-thread merge snapshots record LocalIntegration, never a Capture
    /// whose parents include another Thread's revision.
    pub fn record_native_source(&self, name: &str, state_id: StateId) -> Result<()> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        if let Err(error) = self.native_thread(name) {
            if !is_missing_native_identity(&error) {
                return Err(error);
            }
            self.create_native_thread(name, state_id, None, "")?;
        }
        self.record_native_source_graph(name, state_id, |state| {
            classify_attached_source(self, name, state)
        })
    }

    /// Admit an imported branch's existing ancestry into its already created
    /// Thread. Git merges belong to that graph, even when a sibling Thread has
    /// already imported the same revisions; they are not new local landings.
    ///
    /// Each imported state keeps its own signed capture. A single import-floor
    /// receipt would leave ancestors without an admitted operation, so a later
    /// capture on one of those parents could not name a publisher signature.
    /// The signatures are produced on one connection: WAL, `synchronous=NORMAL`
    /// for the admit, and commits of [`IMPORT_ADMIT_BATCH`] states. Genesis is
    /// verified once. Interactive capture stays on `synchronous=FULL`.
    pub fn record_native_imported_source(&self, name: &str, state_id: StateId) -> Result<()> {
        self.record_native_imported_sources(&[(name, state_id)])
    }

    /// Admit every imported branch on one connection. Each branch still gets
    /// its own signed captures. WAL stays `synchronous=NORMAL` until every
    /// branch has committed, then one durable checkpoint shrinks the log.
    /// Checkpointing after each branch rewrote the whole metadata database
    /// once per ref.
    pub fn record_native_imported_sources(&self, tips: &[(&str, StateId)]) -> Result<()> {
        if tips.is_empty() {
            return Ok(());
        }
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        // One signer for the whole import. `create_native_thread` already
        // bound every fresh thread to this repository-local key. Reading it
        // again per branch opened a new SQLite connection, and each of those
        // connections auto-checkpointed the growing WAL.
        let signer = self.new_local_thread_signer()?;
        let publisher: [u8; 32] = signer
            .public_key()
            .try_into()
            .map_err(|_| Error::Invalid("invalid publisher key".into()))?;
        let heddle_dir = self.heddle_dir();
        let mut connection = super::install_artifacts::InstallationConnection::open(
            heddle_dir,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        // wal_autocheckpoint=0 keeps commits from rewriting the database every
        // thousand pages. The explicit checkpoint below is the one flush.
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=0; PRAGMA cache_size=-262144; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=268435456;",
        )?;
        let first = replica_from_row(&connection, heddle_dir, tips[0].0)?;
        let admit_start = Instant::now();
        let wrote = (|| -> Result<(usize, u128)> {
            // One transaction for every branch. Per-branch commits were an
            // fsync each, and the source triggers are replaced for this
            // transaction so a resumed database picks up deferred indexing.
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            super::source_index::install_deferred_admit_triggers(&tx)?;
            let mut wrote = 0usize;
            // Branch tips share commits. Keep the decoded states for the next
            // thread instead of reading the object store again.
            let mut states = BTreeMap::new();
            for (name, state_id) in tips {
                let replica = replica_from_row(&tx, heddle_dir, name)?;
                let genesis = signed_genesis_on(&tx, replica.thread_id())?.verify()?;
                let author = import_author_on(&replica, &tx, &publisher)?;
                wrote += self.admit_imported_captures(
                    &replica,
                    &tx,
                    &genesis,
                    &signer,
                    publisher,
                    &author,
                    *state_id,
                    &mut states,
                )?;
            }
            let commit_start = Instant::now();
            tx.commit()?;
            Ok((wrote, commit_start.elapsed().as_micros()))
        })();
        let admit_us = admit_start.elapsed().as_micros();
        let checkpoint_start = Instant::now();
        let restored =
            connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA wal_checkpoint(TRUNCATE);");
        let checkpoint_us = checkpoint_start.elapsed().as_micros();
        drop(connection);
        let (wrote, commit_us) = match wrote {
            Ok(value) => value,
            Err(error) => {
                if std::env::var_os("HEDDLE_PROFILE").is_some() {
                    eprintln!(
                        "heddle profile:\n  command: import local admit batch\n  threads: {}\n  admit_batch_ms: {:.3}\n  admit_checkpoint_ms: {:.3}",
                        tips.len(),
                        admit_us as f64 / 1000.0,
                        checkpoint_us as f64 / 1000.0,
                    );
                }
                let _ = restored;
                return Err(error);
            }
        };
        if std::env::var_os("HEDDLE_PROFILE").is_some() {
            eprintln!(
                "heddle profile:\n  command: import local admit batch\n  threads: {}\n  admit_batch_ms: {:.3}\n  admit_commit_ms: {:.3}\n  admit_checkpoint_ms: {:.3}",
                tips.len(),
                admit_us as f64 / 1000.0,
                commit_us as f64 / 1000.0,
                checkpoint_us as f64 / 1000.0,
            );
        }
        restored?;
        if wrote > 0 {
            first.notify_committed()?;
        }
        Ok(())
    }

    fn record_native_source_graph(
        &self,
        name: &str,
        state_id: StateId,
        classify: impl Fn(&State) -> Result<AttachedSourceKind>,
    ) -> Result<()> {
        enum Work {
            Visit(StateId),
            AdmitCaptureParents {
                state_id: StateId,
                state: Box<State>,
                replica: ThreadReplica,
                base: StateId,
                next_parent: usize,
            },
            VerifyCaptureParent {
                parent: StateId,
                replica: ThreadReplica,
            },
        }

        let mut work = vec![Work::Visit(state_id)];
        while let Some(item) = work.pop() {
            match item {
                Work::Visit(state_id) => {
                    let state = self
                        .store()
                        .get_state(&state_id)?
                        .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?;
                    // Captures must declare ancestry; an empty-parent snapshot is genesis
                    // material, not a source operation on an already created Thread.
                    if state.parents.is_empty() {
                        continue;
                    }
                    match classify(&state)? {
                        AttachedSourceKind::Capture => {
                            let replica = self.native_thread(name)?;
                            let base = replica.genesis()?.base;
                            work.push(Work::AdmitCaptureParents {
                                state_id,
                                state: Box::new(state),
                                replica,
                                base,
                                next_parent: 0,
                            });
                        }
                        AttachedSourceKind::LocalIntegration {
                            source_thread,
                            source_operation,
                            source_revision,
                        } => {
                            self.record_native_local_integration(
                                name,
                                state_id,
                                source_thread,
                                source_operation,
                                source_revision,
                            )?;
                        }
                    }
                }
                Work::AdmitCaptureParents {
                    state_id,
                    state,
                    replica,
                    base,
                    next_parent,
                } => {
                    let Some(parent) = state.parents.get(next_parent).copied() else {
                        self.record_native_capture(name, state_id)?;
                        continue;
                    };
                    if parent == base || !replica.source_operation_page(parent, None, 1)?.is_empty()
                    {
                        work.push(Work::AdmitCaptureParents {
                            state_id,
                            state,
                            replica,
                            base,
                            next_parent: next_parent + 1,
                        });
                        continue;
                    }

                    let verify_replica = replica.clone();
                    work.push(Work::AdmitCaptureParents {
                        state_id,
                        state,
                        replica,
                        base,
                        next_parent: next_parent + 1,
                    });
                    work.push(Work::VerifyCaptureParent {
                        parent,
                        replica: verify_replica,
                    });
                    work.push(Work::Visit(parent));
                }
                Work::VerifyCaptureParent { parent, replica } => {
                    if replica.source_operation_page(parent, None, 1)?.is_empty() {
                        return Err(Error::Invalid(format!(
                            "capture parent {} has no admitted native operation",
                            parent.to_string_full()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Fail closed before committing a snapshot when this checkout is attached
    /// to a native Thread whose owner key cannot sign a source operation. The
    /// refusal is the typed [`HeddleError::NativeSourceSignerUnavailable`] so
    /// callers and tests can tell "no signer" from every other capture failure.
    pub fn require_attached_native_source_signer(&self) -> objects::error::Result<()> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())
            .map_err(|error| HeddleError::Config(error.to_string()))?;
        let refs::Head::Attached { thread } = self.head_ref()? else {
            return Ok(());
        };
        let name = thread.to_string();
        let replica = match self.native_thread(&name) {
            Ok(replica) => replica,
            Err(error) if is_missing_native_identity(&error) => return Ok(()),
            Err(error) => {
                return Err(HeddleError::NativeSourceSignerUnavailable {
                    thread: name,
                    reason: error.to_string(),
                });
            }
        };
        self.native_thread_signer(&replica)
            .map(|_| ())
            .map_err(|error| HeddleError::NativeSourceSignerUnavailable {
                thread: name,
                reason: error.to_string(),
            })
    }

    /// Record a capture on the attached native Thread after a local snapshot.
    pub fn record_attached_native_source(&self, state_id: StateId) -> Result<()> {
        let _serialization = InstallationLock::acquire(self.heddle_dir())?;
        let name = match self
            .head_ref()
            .map_err(|error| Error::Invalid(error.to_string()))?
        {
            refs::Head::Attached { thread } => thread.to_string(),
            refs::Head::Detached { .. } => return Ok(()),
        };
        self.record_native_source(&name, state_id)
    }

    /// Record a locally produced cross-thread landing once. Causal parents are
    /// the target Thread's source frontier; the merge State's parents keep the
    /// explicit source revision. Same-thread Capture must not be used here.
    pub fn record_native_local_integration(
        &self,
        name: &str,
        state_id: StateId,
        source_thread: ContentHash,
        source_operation: ContentHash,
        source_revision: StateId,
    ) -> Result<ContentHash> {
        let serialization = InstallationLock::acquire(self.heddle_dir())?;
        let _guard = self.native_identity_lock(&serialization)?;
        let replica = self.native_thread(name)?;
        if source_thread == replica.thread_id() {
            return Err(Error::Invalid(
                "local integration requires a distinct source Thread".into(),
            ));
        }
        if let Some(existing) = replica.source_operation_page(state_id, None, 1)?.first() {
            replica.validate_local_source_possession(self.store(), state_id)?;
            return Ok(*existing);
        }
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| Error::Invalid("integrated state unavailable".into()))?;
        let genesis = replica.genesis()?;
        let view = replica.view()?;
        let expected_heads = if view.source_heads.is_empty() {
            BTreeSet::from([genesis.base])
        } else {
            view.source_heads
        };
        let local_parents: BTreeSet<StateId> = state
            .parents
            .iter()
            .copied()
            .filter(|id| *id != source_revision)
            .collect();
        if local_parents != expected_heads {
            return Err(Error::Invalid(
                "local integration target has unresolved source heads".into(),
            ));
        }
        let frontier = view
            .frontiers
            .get(&ThreadFacet::Source)
            .cloned()
            .unwrap_or_default();
        let mut result_visibility = self.resolve_capture_default_visibility();
        for parent in &state.parents {
            result_visibility = local_integration::intersect_visibility(
                &result_visibility,
                &self
                    .effective_visibility_tier(parent)
                    .map_err(|error| Error::Invalid(error.to_string()))?,
            )?;
        }
        let local_policy_version = local_integration_policy_version(&result_visibility)?;
        let signer = self.native_thread_signer(&replica)?;
        let publisher: [u8; 32] = signer
            .public_key()
            .try_into()
            .map_err(|_| Error::Invalid("invalid publisher key".into()))?;
        let receipt = LocalIntegration {
            author: replica.source_author_for(&publisher)?,
            version: 1,
            spool: genesis
                .spool
                .parse()
                .map_err(|error: uuid::Error| Error::Invalid(error.to_string()))?,
            device: publisher,
            source_thread,
            source_operation,
            source_revision,
            target_thread: replica.thread_id(),
            expected_target_frontier: frontier.clone(),
            result: replica.prepare_integration(self, &state, source_thread, source_operation)?,
            result_visibility,
            initiating_request_proof: local_integration_request_digest(
                source_thread,
                replica.thread_id(),
                state_id,
            ),
            local_policy_version,
            executed_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: frontier,
            publisher,
            body: ThreadOperationBody::LocalIntegration(receipt.encode()?),
        };
        let signed = SignedOperation::sign(&operation, &signer)?;
        match replica.receive_prepared_source(&signed, self.store(), |_| Ok(()))? {
            Admission::Accepted => Ok(operation.id()?),
            other => Err(Error::Invalid(format!(
                "local integration was not admitted: {other:?}"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admit_imported_captures(
        &self,
        replica: &ThreadReplica,
        tx: &rusqlite::Transaction<'_>,
        genesis: &ThreadGenesis,
        signer: &Ed25519Signer,
        publisher: [u8; 32],
        author: &SourceAuthor,
        tip: StateId,
        states: &mut BTreeMap<StateId, State>,
    ) -> Result<usize> {
        let order_start = Instant::now();
        let fresh = !thread_has_accepted_operation(tx, &replica.thread_id())?;
        let order = self.import_capture_order(replica, tx, genesis.base, tip, fresh, states)?;
        let order_us = order_start.elapsed().as_micros();
        // A Git import has no reference seeds, inherited closures, or visibility
        // sidecars. `prepare` then returns the State bytes alone. Building that
        // capture directly keeps the signed payload and skips the per-state
        // catalog queries and installation lock.
        let bare = self.import_admit_is_bare(tx, &replica.thread_id())?;
        let fast = bare && bare_import_can_skip_receive_scan(tx, replica, &publisher, genesis)?;
        let mut clock = AdmitClock::default();
        let mut wrote = 0usize;
        let mut index = 0usize;
        // Parent ids recorded earlier in this thread, including rows from the
        // previous batch. Avoids a source-revision lookup per parent.
        let mut admitted_ops: BTreeMap<StateId, Vec<ContentHash>> = BTreeMap::new();
        while index < order.len() {
            let end = (index + IMPORT_ADMIT_BATCH).min(order.len());
            if fast {
                self.admit_bare_captures_in(
                    replica,
                    tx,
                    genesis,
                    signer,
                    publisher,
                    author,
                    &order[index..end],
                    &mut admitted_ops,
                    &mut clock,
                )?;
                wrote += end - index;
            } else {
                for state in &order[index..end] {
                    self.admit_imported_capture_in(
                        replica, tx, genesis, signer, publisher, author, state, bare, &mut clock,
                    )?;
                    wrote += 1;
                }
            }
            index = end;
        }
        if std::env::var_os("HEDDLE_PROFILE").is_some() {
            eprintln!(
                "heddle profile:\n  command: import local admit\n  thread: {}\n  admit_order_ms: {:.3}\n  admit_capture_ms: {:.3}\n  admit_canonical_ms: {:.3}\n  admit_sign_ms: {:.3}\n  admit_receive_ms: {:.3}\n  admit_commit_ms: {:.3}\n  admit_states: {wrote}\n  bare_capture: {bare}\n  direct_admit: {fast}",
                replica.thread_id(),
                order_us as f64 / 1000.0,
                clock.capture_us as f64 / 1000.0,
                clock.canonical_us as f64 / 1000.0,
                clock.sign_us as f64 / 1000.0,
                clock.receive_us as f64 / 1000.0,
                clock.commit_us as f64 / 1000.0,
            );
        }
        Ok(wrote)
    }

    /// Parent-first states that still need a signed capture. Already accepted
    /// revisions and the synthetic genesis base are not revisited.
    fn import_capture_order(
        &self,
        replica: &ThreadReplica,
        connection: &rusqlite::Connection,
        base: StateId,
        tip: StateId,
        fresh: bool,
        cache: &mut BTreeMap<StateId, State>,
    ) -> Result<Vec<State>> {
        let thread = replica.thread_id();
        let mut pending = BTreeMap::new();
        let mut stack = vec![tip];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            // A new thread has no accepted captures, so the per-state existence
            // query would only return false. Resume still checks.
            if !fresh && accepted_import_revision(connection, &thread, id)? {
                continue;
            }
            let state = if let Some(state) = cache.get(&id) {
                state.clone()
            } else {
                let state = self
                    .store()
                    .get_state(&id)?
                    .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?;
                cache.insert(id, state.clone());
                state
            };
            if state.parents.is_empty() {
                continue;
            }
            for parent in &state.parents {
                if *parent != base {
                    stack.push(*parent);
                }
            }
            pending.insert(id, state);
        }
        let mut children: BTreeMap<StateId, Vec<StateId>> = BTreeMap::new();
        let mut indegree: BTreeMap<StateId, usize> = BTreeMap::new();
        for (id, state) in &pending {
            let mut degree = 0usize;
            for parent in &state.parents {
                if pending.contains_key(parent) {
                    degree += 1;
                    children.entry(*parent).or_default().push(*id);
                }
            }
            indegree.insert(*id, degree);
        }
        let mut ready: BTreeSet<StateId> = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::with_capacity(pending.len());
        while let Some(id) = ready.pop_first() {
            let Some(state) = pending.remove(&id) else {
                return Err(Error::Invalid("imported history has a cycle".into()));
            };
            if let Some(dependents) = children.get(&id) {
                for child in dependents {
                    let Some(degree) = indegree.get_mut(child) else {
                        return Err(Error::Invalid("imported history has a cycle".into()));
                    };
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        ready.insert(*child);
                    }
                }
            }
            order.push(state);
        }
        if !pending.is_empty() {
            return Err(Error::Invalid("imported history has a cycle".into()));
        }
        Ok(order)
    }

    #[allow(clippy::too_many_arguments)]
    fn admit_imported_capture_in(
        &self,
        replica: &ThreadReplica,
        tx: &rusqlite::Transaction<'_>,
        genesis: &ThreadGenesis,
        signer: &Ed25519Signer,
        publisher: [u8; 32],
        author: &SourceAuthor,
        state: &State,
        bare: bool,
        clock: &mut AdmitClock,
    ) -> Result<()> {
        let mut parents = BTreeSet::new();
        for parent in &state.parents {
            if *parent == genesis.base {
                continue;
            }
            let operations = admitted_source_operations(tx, &replica.thread_id(), *parent)?;
            if operations.is_empty() {
                return Err(Error::Invalid(format!(
                    "capture parent {} has no admitted native operation",
                    parent.to_string_full()
                )));
            }
            parents.extend(operations);
        }
        let capture_start = Instant::now();
        let result = if bare {
            state.encode_current_msgpack()?.into()
        } else {
            replica.prepare_capture_in(tx, genesis, self, state)?
        };
        clock.capture_us += capture_start.elapsed().as_micros();
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents,
            publisher,
            body: ThreadOperationBody::Capture(
                objects::object::thread_replication::AuthoredCapture {
                    result,
                    author: author.clone(),
                },
            ),
        };
        let sign_start = Instant::now();
        let signed = SignedOperation::sign(&operation, signer)?;
        clock.sign_us += sign_start.elapsed().as_micros();
        let receive_start = Instant::now();
        replica.require_trusted_integration(&operation)?;
        replica.require_local_integration_source_in(tx, &operation)?;
        replica.validate_reference_capture_in(tx, genesis, &operation, self.store())?;
        match replica.receive_in(tx, &signed, &operation, self.store(), false, None, false)? {
            Admission::Accepted => {}
            other => {
                return Err(Error::Invalid(format!(
                    "local capture was not admitted: {other:?}"
                )));
            }
        }
        replica.record_source_possession_in(tx, state.id())?;
        clock.receive_us += receive_start.elapsed().as_micros();
        Ok(())
    }

    /// True when an imported capture's signed bytes are only the State.
    /// Reference rows or visibility sidecars still go through `prepare_capture`.
    fn import_admit_is_bare(
        &self,
        connection: &rusqlite::Connection,
        thread: &ContentHash,
    ) -> Result<bool> {
        let seeds: i64 = connection.query_row(
            "SELECT count(*) FROM reference_seeds WHERE thread=?1",
            [thread.as_bytes()],
            |row| row.get(0),
        )?;
        if seeds != 0 {
            return Ok(false);
        }
        let pending: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM reference_projection_pending WHERE thread=?1)",
            [thread.as_bytes()],
            |row| row.get(0),
        )?;
        if pending {
            return Ok(false);
        }
        let captures: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM reference_captures WHERE thread=?1)",
            [thread.as_bytes()],
            |row| row.get(0),
        )?;
        if captures {
            return Ok(false);
        }
        if directory_has_import_visibility(&self.state_visibility_dir())? {
            return Ok(false);
        }
        if directory_has_import_visibility(&self.entry_visibility_dir())? {
            return Ok(false);
        }
        Ok(true)
    }

    /// Admit bare Git captures without decoding each signed operation again.
    ///
    /// Signing already produced canonical bytes. The operation id is the
    /// `OPERATION_FORMAT` hash of those bytes, which is what `ThreadOperation::id`
    /// returns after a fresh signature. Each parent link is an operation already
    /// admitted for that state parent. The row is inserted accepted, after its
    /// parent links. Source revisions, heads, and the search queue are written
    /// once from this batch, matching the insert trigger's net frontier.
    /// Generation still
    /// advances for the insert, the acceptance, and a new possession row,
    /// applied once at the end of the batch. Callers keep the ordinary
    /// `receive_in` path when the capture is not a bare local import.
    #[allow(clippy::too_many_arguments, clippy::unused_self)]
    fn admit_bare_captures_in(
        &self,
        replica: &ThreadReplica,
        tx: &rusqlite::Transaction<'_>,
        genesis: &ThreadGenesis,
        signer: &Ed25519Signer,
        publisher: [u8; 32],
        author: &SourceAuthor,
        states: &[State],
        admitted: &mut BTreeMap<StateId, Vec<ContentHash>>,
        clock: &mut AdmitClock,
    ) -> Result<()> {
        author
            .validate()
            .map_err(|error| Error::Invalid(error.to_string()))?;
        if signer.public_key() != publisher.as_slice() {
            return Err(Error::Invalid("publisher differs from signing key".into()));
        }
        if let SourceAuthor::Account { spool, .. } = author
            && spool.to_string() != genesis.spool
        {
            return Err(Error::Invalid(
                "original source author crosses Spool scope".into(),
            ));
        }
        let facet = super::facet_number(ThreadFacet::Source);
        let thread = replica.thread_id();
        // The source triggers would rewrite heads once per capture. This row
        // makes them skip the thread; `finish_deferred_source_index` writes
        // the same revisions, count, heads, and search queue once.
        super::source_index::begin_deferred_source_index(tx, thread.as_bytes())?;
        let mut parent_query = tx.prepare_cached(
            "SELECT id FROM operations WHERE thread=?1 AND source_revision=?2 AND status=1 AND facet=?3 ORDER BY id LIMIT 1024",
        )?;
        let mut prepared = Vec::with_capacity(states.len());
        for state in states {
            if state.parents.is_empty()
                || state.parents.len() != state.parents.iter().collect::<BTreeSet<_>>().len()
            {
                return Err(Error::Invalid(
                    "capture source ancestry differs from causal parents".into(),
                ));
            }
            let parents = imported_parent_operations(
                &mut parent_query,
                admitted,
                &thread,
                state,
                genesis.base,
                facet,
            )?;
            let capture_start = Instant::now();
            let result = state.encode_current_msgpack()?.into();
            clock.capture_us += capture_start.elapsed().as_micros();
            let operation = ThreadOperation {
                version: 1,
                thread,
                parents,
                publisher,
                body: ThreadOperationBody::Capture(
                    objects::object::thread_replication::AuthoredCapture {
                        result,
                        author: author.clone(),
                    },
                ),
            };
            replica.require_trusted_integration(&operation)?;
            // Captures have no collaboration record. `receive_in` indexes
            // here; the call stays so a capture body that later carries one
            // still does.
            super::collaboration::index_operation(tx, &operation)?;
            let canonical_start = Instant::now();
            let canonical = operation.encode_locally_prepared_capture()?;
            clock.canonical_us += canonical_start.elapsed().as_micros();
            let id = ContentHash::compute_typed(
                objects::object::thread_replication::OPERATION_FORMAT,
                &canonical,
            );
            admitted.entry(state.id()).or_default().push(id);
            prepared.push(PreparedCapture {
                id,
                canonical,
                state_id: state.id(),
                parents: operation.parents,
            });
        }
        drop(parent_query);
        let sign_start = Instant::now();
        let signatures = sign_import_canonicals(signer, &prepared)?;
        clock.sign_us += sign_start.elapsed().as_micros();
        let receive_start = Instant::now();
        let mut insert_op = tx.prepare_cached(
            "INSERT OR IGNORE INTO operations(id,thread,facet,canonical,signature,source_revision,status) VALUES(?1,?2,?3,?4,?5,?6,1)",
        )?;
        let mut insert_parent =
            tx.prepare_cached("INSERT INTO parents(child,parent) VALUES(?1,?2)")?;
        let mut possess = tx.prepare_cached(
            "INSERT OR IGNORE INTO thread_source_availability(thread,revision) VALUES(?1,?2)",
        )?;
        let mut bumps = 0i64;
        for (capture, signature) in prepared.iter().zip(signatures) {
            // Parent links first so a non-deferred trigger would see them.
            for parent in &capture.parents {
                insert_parent.execute(params![capture.id.as_bytes(), parent.as_bytes()])?;
            }
            let inserted = insert_op.execute(params![
                capture.id.as_bytes(),
                thread.as_bytes(),
                facet,
                &capture.canonical,
                signature,
                capture.state_id.as_bytes(),
            ])?;
            if inserted != 1 {
                return Err(Error::Invalid(
                    "imported capture already has an operation".into(),
                ));
            }
            let possessed =
                possess.execute(params![thread.as_bytes(), capture.state_id.as_bytes()])?;
            bumps += 2 + i64::from(possessed != 0);
        }
        drop(insert_op);
        drop(insert_parent);
        drop(possess);
        let deferred: Vec<super::source_index::DeferredSourceCapture<'_>> = prepared
            .iter()
            .map(|capture| super::source_index::DeferredSourceCapture {
                id: capture.id.as_bytes(),
                revision: capture.state_id.as_bytes(),
                parents: &capture.parents,
            })
            .collect();
        super::source_index::finish_deferred_source_index(tx, thread.as_bytes(), &deferred)?;
        if bumps > 0 {
            tx.execute(
                "UPDATE threads SET generation=generation+?2 WHERE id=?1",
                params![thread.as_bytes(), bumps],
            )?;
        }
        clock.receive_us += receive_start.elapsed().as_micros();
        Ok(())
    }
}

struct PreparedCapture {
    id: ContentHash,
    canonical: Vec<u8>,
    state_id: StateId,
    parents: BTreeSet<ContentHash>,
}

#[derive(Default)]
struct AdmitClock {
    capture_us: u128,
    canonical_us: u128,
    sign_us: u128,
    receive_us: u128,
    commit_us: u128,
}

/// Ed25519 over already-encoded captures. Parent ids depend on earlier
/// canonical bytes, so encoding stays in order and only the signatures run
/// together. Each worker gets the same seed; the signature is deterministic.
fn sign_import_canonicals(
    signer: &Ed25519Signer,
    prepared: &[PreparedCapture],
) -> Result<Vec<Vec<u8>>> {
    if prepared.is_empty() {
        return Ok(Vec::new());
    }
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, prepared.len())
        .min(8);
    if workers == 1 || prepared.len() < 2 {
        return prepared
            .iter()
            .map(|capture| sign_one_canonical(signer, &capture.canonical))
            .collect();
    }
    let seed = signer.to_seed();
    let chunk = prepared.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for group in prepared.chunks(chunk) {
            handles.push(scope.spawn(move || {
                let signer = Ed25519Signer::from_seed(&seed)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                group
                    .iter()
                    .map(|capture| sign_one_canonical(&signer, &capture.canonical))
                    .collect::<Result<Vec<_>>>()
            }));
        }
        let mut signatures = Vec::with_capacity(prepared.len());
        for handle in handles {
            let signed = handle
                .join()
                .map_err(|_| Error::Invalid("import signature worker failed".into()))??;
            signatures.extend(signed);
        }
        Ok(signatures)
    })
}

fn sign_one_canonical(signer: &Ed25519Signer, canonical: &[u8]) -> Result<Vec<u8>> {
    SignedOperation::sign_canonical(canonical, signer)
        .map_err(|error| Error::Invalid(error.to_string()))
}

fn directory_has_import_visibility(path: &std::path::Path) -> Result<bool> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::Invalid(error.to_string())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| Error::Invalid(error.to_string()))?;
        if entry.file_name() == "audience_label" {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

fn replica_from_row(
    connection: &rusqlite::Connection,
    heddle_dir: &std::path::Path,
    name: &str,
) -> Result<ThreadReplica> {
    let id: Option<Vec<u8>> = connection
        .query_row(
            "SELECT thread FROM local_thread_names WHERE name=?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    let id = id.ok_or_else(|| Error::Invalid(format!("Thread {name:?} has no native identity")))?;
    Ok(ThreadReplica {
        path: heddle_dir.join(crate::local_metadata::DATABASE_NAME),
        thread: super::hash(&id)?,
    })
}

fn signed_genesis_on(
    connection: &rusqlite::Connection,
    thread: ContentHash,
) -> Result<SignedGenesis> {
    let signed = connection.query_row(
        "SELECT genesis,genesis_signature FROM threads WHERE id=?1",
        [thread.as_bytes()],
        |row| {
            Ok(SignedGenesis {
                canonical: row.get(0)?,
                signature: row.get(1)?,
            })
        },
    )?;
    if signed.verify()?.id()? != thread {
        return Err(Error::Invalid(
            "stored creator proof differs from Thread identity".into(),
        ));
    }
    Ok(signed)
}

fn import_author_on(
    replica: &ThreadReplica,
    connection: &rusqlite::Connection,
    publisher: &[u8; 32],
) -> Result<SourceAuthor> {
    use objects::object::thread_replication::GenesisOwner;
    match replica.effective_owner_in(connection)? {
        GenesisOwner::LocalKey(owner) if &owner == publisher => Ok(SourceAuthor::LocalKey),
        GenesisOwner::LocalKey(_) => Err(Error::Invalid(
            "unclaimed Thread requires original local source signer".into(),
        )),
        GenesisOwner::Account(_) => {
            let spool = replica
                .genesis_in(connection)?
                .spool
                .parse()
                .map_err(|error: uuid::Error| Error::Invalid(error.to_string()))?;
            let author = crate::identity::source_author::load(
                &crate::identity::heddle_home_dir(),
                publisher,
                spool,
            )
            .map_err(|error| Error::Invalid(error.to_string()))?;
            if matches!(author, SourceAuthor::LocalKey) {
                return Err(Error::Invalid(
                    "account Thread requires original device account proof".into(),
                ));
            }
            Ok(author)
        }
    }
}

/// States admitted in one transaction. Parent operation ids stay in memory
/// across the batch, and the generation counter moves once at the end.
const IMPORT_ADMIT_BATCH: usize = 65_536;

/// The direct admit path matches `receive_in` only for a fresh local capture.
/// A hosted import key, an existing import admission, an ownership claim, or a
/// forked genesis still goes through the full parent decode.
fn bare_import_can_skip_receive_scan(
    connection: &rusqlite::Connection,
    replica: &ThreadReplica,
    publisher: &[u8; 32],
    genesis: &ThreadGenesis,
) -> Result<bool> {
    if genesis.parent.is_some() {
        return Ok(false);
    }
    let witnessed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_import_job_keys WHERE public_key=?1)",
        [publisher.as_slice()],
        |row| row.get(0),
    )?;
    if witnessed {
        return Ok(false);
    }
    let admissions: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_import_admissions)",
        [],
        |row| row.get(0),
    )?;
    if admissions {
        return Ok(false);
    }
    let claims: i64 = connection.query_row(
        "SELECT count(*) FROM thread_owner_claims WHERE thread=?1",
        [replica.thread_id().as_bytes()],
        |row| row.get(0),
    )?;
    Ok(claims == 0)
}

fn imported_parent_operations(
    query: &mut rusqlite::CachedStatement<'_>,
    admitted: &mut BTreeMap<StateId, Vec<ContentHash>>,
    thread: &ContentHash,
    state: &State,
    base: StateId,
    facet: u32,
) -> Result<BTreeSet<ContentHash>> {
    let mut parents = BTreeSet::new();
    for parent in &state.parents {
        if *parent == base {
            continue;
        }
        if !admitted.contains_key(parent) {
            let rows = query.query_map(
                params![thread.as_bytes(), parent.as_bytes(), facet],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            let mut ops = Vec::new();
            for row in rows {
                ops.push(super::hash(&row?)?);
            }
            if ops.is_empty() {
                return Err(Error::Invalid(format!(
                    "capture parent {} has no admitted native operation",
                    parent.to_string_full()
                )));
            }
            if ops.len() == 1024 {
                return Err(Error::Invalid(format!(
                    "capture parent {} has too many admitted operations",
                    parent.to_string_full()
                )));
            }
            admitted.insert(*parent, ops);
        }
        let Some(ops) = admitted.get(parent) else {
            return Err(Error::Invalid(
                "capture parent has no admitted native operation".into(),
            ));
        };
        if ops.is_empty() {
            return Err(Error::Invalid(format!(
                "capture parent {} has no admitted native operation",
                parent.to_string_full()
            )));
        }
        parents.extend(ops.iter().copied());
    }
    Ok(parents)
}

fn thread_has_accepted_operation(
    connection: &rusqlite::Connection,
    thread: &ContentHash,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND status=1)",
        [thread.as_bytes()],
        |row| row.get(0),
    )?)
}

fn accepted_import_revision(
    connection: &rusqlite::Connection,
    thread: &ContentHash,
    revision: StateId,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND source_revision=?2 AND status=1)",
        params![thread.as_bytes(), revision.as_bytes()],
        |row| row.get(0),
    )?)
}

fn admitted_source_operations(
    connection: &rusqlite::Connection,
    thread: &ContentHash,
    revision: StateId,
) -> Result<Vec<ContentHash>> {
    let mut query = connection.prepare(
        "SELECT id FROM operations WHERE thread=?1 AND source_revision=?2 AND status=1 ORDER BY id LIMIT 1024",
    )?;
    let rows = query.query_map(params![thread.as_bytes(), revision.as_bytes()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    rows.map(|row| super::hash(&row?)).collect()
}

enum AttachedSourceKind {
    Capture,
    LocalIntegration {
        source_thread: ContentHash,
        source_operation: ContentHash,
        source_revision: StateId,
    },
}

pub(super) fn is_missing_native_identity(error: &Error) -> bool {
    matches!(error, Error::Invalid(message) if message.contains("has no native identity"))
}

fn classify_attached_source(
    repo: &Repository,
    name: &str,
    state: &State,
) -> Result<AttachedSourceKind> {
    let replica = repo.native_thread(name)?;
    let genesis = replica.genesis()?;
    let connection = replica.connect()?;
    let mut foreign: Option<(ContentHash, ContentHash, StateId)> = None;
    let mut local_parent = false;
    for parent in &state.parents {
        if *parent == genesis.base {
            local_parent = true;
            continue;
        }
        let local = replica.source_operation_page(*parent, None, 1)?;
        if !local.is_empty() {
            local_parent = true;
            continue;
        }
        let mut query = connection.prepare(
            "SELECT thread, MIN(id) FROM operations WHERE source_revision=?1 AND status=1 GROUP BY thread LIMIT 3",
        )?;
        let found = query
            .query_map([parent.as_bytes().as_slice()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut by_thread = BTreeMap::new();
        for (thread, operation) in found {
            let thread = super::hash(&thread)?;
            if thread == replica.thread_id() {
                continue;
            }
            by_thread.insert(thread, super::hash(&operation)?);
        }
        match by_thread.into_iter().next() {
            None => {
                // Pull/FF can land a foreign tip before this Thread has an
                // admitted op for it. `admit_local_capture_parents` records
                // that ancestry; classify must not refuse first.
                if repo.store().get_state(parent)?.is_none() {
                    return Err(Error::Invalid(format!(
                        "capture parent {} has no admitted native operation",
                        parent.to_string_full()
                    )));
                }
                local_parent = true;
            }
            Some((source_thread, source_operation)) => {
                // Multiple Threads can carry the same revision after a sibling
                // refresh. The revision is the integration input; any accepted
                // operation for it proves possession. Pick one deterministically.
                let candidate = (source_thread, source_operation, *parent);
                if let Some(existing) = &foreign {
                    if existing.0 != candidate.0 || existing.2 != candidate.2 {
                        return Err(Error::Invalid(
                            "local integration cannot combine multiple source Threads".into(),
                        ));
                    }
                } else {
                    foreign = Some(candidate);
                }
            }
        }
    }
    match foreign {
        None => Ok(AttachedSourceKind::Capture),
        Some((source_thread, source_operation, source_revision))
            if local_parent && state.parents.len() >= 2 =>
        {
            Ok(AttachedSourceKind::LocalIntegration {
                source_thread,
                source_operation,
                source_revision,
            })
        }
        Some(_) => Ok(AttachedSourceKind::Capture),
    }
}

/// The CLI has no device-RPC request to cite. On the device path
/// `initiating_request_proof` is the hash of the verified request proof; here
/// it carries a deterministic digest of the public request tuple instead, so
/// the receipt is bound to what was asked and a retry reproduces the same
/// value. It is not evidence of authority: that comes only from the operation
/// signature checked at admission; heddle admission never reads this field.
fn local_integration_request_digest(
    source_thread: ContentHash,
    target_thread: ContentHash,
    state_id: StateId,
) -> ContentHash {
    ContentHash::compute_typed(
        "heddle-cli-local-integration-request-v1",
        &[
            source_thread.as_bytes().as_slice(),
            target_thread.as_bytes().as_slice(),
            state_id.as_bytes().as_slice(),
        ]
        .concat(),
    )
}

fn local_integration_policy_version(visibility: &VisibilityTier) -> Result<ContentHash> {
    let encoded =
        serde_json::to_vec(visibility).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(ContentHash::compute_typed(
        "heddle-cli-local-integration-policy-v1",
        &[
            b"same-spool;root-derived-source-and-target-authority;explicit-source;all-target-parents;three-way-or-resolved;target-frontier-cas;preserve-audience;no-hosted-approval".as_slice(),
            encoded.as_slice(),
        ]
        .concat(),
    ))
}
