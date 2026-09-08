//! Local Thread creation and capture use the same immutable records as remote peers.
use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer as _,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        ContentHash, StateId,
        thread_replication::{ThreadGenesis, ThreadOperation, ThreadOperationBody},
    },
    store::ObjectStore as _,
};
use rusqlite::{OptionalExtension as _, params};

use super::{Admission, Error, Result, ThreadReplica};
use crate::Repository;

impl Repository {
    /// Stable local/hosted spool identity, created before the first Thread.
    pub fn native_spool_id(&self) -> Result<uuid::Uuid> {
        let _guard = self.native_identity_lock()?;
        self.native_spool_id_locked(None)
    }
    /// Clone installs the source identity before creating local Thread records.
    pub fn install_native_spool_id(&self, id: uuid::Uuid) -> Result<()> {
        let _guard = self.native_identity_lock()?;
        self.native_spool_id_locked(Some(id))?;
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
    fn native_identity_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        objects::fs_atomic::create_dir_all_durable(self.heddle_dir())?;
        objects::lock::RepoLock::at(self.heddle_dir().join("locks/native-identity.lock"))
            .write()
            .map_err(|error| Error::Invalid(error.to_string()))
    }
    fn native_signer(&self) -> Result<Ed25519Signer> {
        let pem = match crate::identity::load_device(&crate::identity::device_identity_path())? {
            Some(device) => device.private_key_pem,
            None => {
                crate::identity::load_or_mint_local(
                    &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
                )?
                .private_key_pem
            }
        };
        Ok(Ed25519Signer::from_pem(&pem)?)
    }
    /// Lookup never creates a Thread or invents a publisher signature.
    pub fn native_thread(&self, name: &str) -> Result<ThreadReplica> {
        let connection = rusqlite::Connection::open_with_flags(
            self.heddle_dir().join("thread-replication.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
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
    /// Persist the creator's genesis before checkout work begins. Repeating exact
    /// creation reuses the original signature even after device key rotation.
    pub fn create_native_thread(
        &self,
        name: &str,
        base: StateId,
        parent: Option<&str>,
        intent: &str,
    ) -> Result<ThreadReplica> {
        let _guard = self.native_identity_lock()?;
        let spool = self.native_spool_id_locked(None)?;
        let parent_id = parent
            .map(|name| self.native_thread(name).map(|replica| replica.thread_id()))
            .transpose()?;
        let database = self.heddle_dir().join("thread-replication.sqlite3");
        if database.exists() {
            let connection = rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
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
        let signer = self.native_signer()?;
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
            nonce: Vec::new(),
        };
        let signed = SignedGenesis::sign(&genesis, &signer)?;
        let replica = ThreadReplica::create(self.heddle_dir(), &signed)?;
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
        let _guard = self.native_identity_lock()?;
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
        let _guard = self.native_identity_lock()?;
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
        let _guard = self.native_identity_lock()?;
        let replica = self.native_thread(name)?;
        if let Some(existing) = replica.source_operation_page(state_id, None, 1)?.first() {
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
        let signer = self.native_signer()?;
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents,
            publisher: signer
                .public_key()
                .try_into()
                .map_err(|_| Error::Invalid("invalid publisher key".into()))?,
            body: ThreadOperationBody::Capture(state.encode_current_msgpack()?),
        };
        let signed = SignedOperation::sign(&operation, &signer)?;
        match replica.receive(&signed, self.store(), |_| Ok(()))? {
            Admission::Accepted => Ok(operation.id()?),
            other => Err(Error::Invalid(format!(
                "local capture was not admitted: {other:?}"
            ))),
        }
    }
}
