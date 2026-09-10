//! Local Thread creation and capture use the same immutable records as remote peers.
use std::collections::{BTreeMap, BTreeSet};

use crypto::{
    Ed25519Signer, Signer as _,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        ContentHash, State, StateId, VisibilityTier,
        thread_replication::{
            ThreadFacet, ThreadGenesis, ThreadOperation, ThreadOperationBody,
            local_integration::{self, LocalIntegration},
        },
    },
    store::ObjectStore as _,
};
use rusqlite::{OptionalExtension as _, params};

use super::{Admission, Error, Result, ThreadReplica};
use crate::Repository;

impl Repository {
    /// Read-only proof that this device still holds the unclaimed owner's key.
    /// Looking at a Thread must never mint a replacement identity.
    pub fn holds_native_owner_key(&self, key: &[u8; 32]) -> Result<bool> {
        if let Some(local) = crate::identity::load_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )? {
            if Ed25519Signer::from_pem(&local.private_key_pem)?.public_key() == key {
                return Ok(true);
            }
        }
        if let Some(device) =
            crate::identity::load_device(&crate::identity::device_identity_path())?
        {
            if Ed25519Signer::from_pem(&device.private_key_pem)?.public_key() == key {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// Stable local/hosted spool identity, created before the first Thread.
    pub fn native_spool_id(&self) -> Result<uuid::Uuid> {
        let _guard = self.native_identity_lock()?;
        let id = self.native_spool_id_locked(None)?;
        crate::device_catalog::register(&crate::identity::heddle_home_dir(), self, id)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        Ok(id)
    }
    /// Clone installs the source identity before creating local Thread records.
    pub fn install_native_spool_id(&self, id: uuid::Uuid) -> Result<()> {
        let _guard = self.native_identity_lock()?;
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
    fn native_identity_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        objects::fs_atomic::create_dir_all_durable(self.heddle_dir())?;
        objects::lock::RepoLock::at(self.heddle_dir().join("locks/native-identity.lock"))
            .write()
            .map_err(|error| Error::Invalid(error.to_string()))
    }
    /// Enrollment never changes the signer of an unclaimed local Thread.
    pub fn native_thread_signer(&self, replica: &ThreadReplica) -> Result<Ed25519Signer> {
        self.native_thread_signer_at(replica, &crate::identity::heddle_home_dir())
    }
    pub(crate) fn native_thread_signer_at(
        &self,
        replica: &ThreadReplica,
        home: &std::path::Path,
    ) -> Result<Ed25519Signer> {
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
        let path = self.heddle_dir().join(crate::local_metadata::DATABASE_NAME);
        if !path.exists() {
            return Err(Error::Invalid(format!(
                "Thread {name:?} has no native identity"
            )));
        }
        // WAL databases cannot be opened SQLITE_OPEN_READ_ONLY; lookup still
        // refuses to create a missing database (the exists() check above).
        let connection = rusqlite::Connection::open_with_flags(
            path,
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
    /// Missing native identity is not created here; capture/start own genesis.
    /// Cross-thread merge snapshots record LocalIntegration, never a Capture
    /// whose parents include another Thread's revision.
    pub fn record_native_source(&self, name: &str, state_id: StateId) -> Result<()> {
        if self.native_thread(name).is_err() {
            return Ok(());
        }
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?;
        // Captures must declare ancestry; an empty-parent snapshot is genesis
        // material, not a source operation on an already created Thread.
        if state.parents.is_empty() {
            return Ok(());
        }
        match classify_attached_source(self, name, &state)? {
            AttachedSourceKind::Capture => {
                self.record_native_capture(name, state_id)?;
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
        Ok(())
    }

    /// Fail closed before committing a snapshot when this checkout is attached
    /// to a native Thread whose owner key cannot sign a source operation.
    pub fn require_attached_native_source_signer(&self) -> Result<()> {
        let name = match self
            .head_ref()
            .map_err(|error| Error::Invalid(error.to_string()))?
        {
            refs::Head::Attached { thread } => thread.to_string(),
            refs::Head::Detached { .. } => return Ok(()),
        };
        let replica = match self.native_thread(&name) {
            Ok(replica) => replica,
            Err(_) => return Ok(()),
        };
        self.native_thread_signer(&replica)?;
        Ok(())
    }

    /// Record a capture on the attached native Thread after a local snapshot.
    pub fn record_attached_native_source(&self, state_id: StateId) -> Result<()> {
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
        let _guard = self.native_identity_lock()?;
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
            initiating_request_proof: ContentHash::compute_typed(
                "heddle-cli-local-integration-request-v1",
                &[
                    source_thread.as_bytes().as_slice(),
                    replica.thread_id().as_bytes().as_slice(),
                    state_id.as_bytes().as_slice(),
                ]
                .concat(),
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
}

enum AttachedSourceKind {
    Capture,
    LocalIntegration {
        source_thread: ContentHash,
        source_operation: ContentHash,
        source_revision: StateId,
    },
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
    for parent in &state.parents {
        if *parent == genesis.base {
            continue;
        }
        let local = replica.source_operation_page(*parent, None, 1)?;
        if !local.is_empty() {
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
        let mut unique = by_thread.into_iter();
        match (unique.next(), unique.next()) {
            (None, _) => {
                return Err(Error::Invalid(format!(
                    "capture parent {} has no admitted native operation",
                    parent.to_string_full()
                )));
            }
            (Some((source_thread, source_operation)), None) => {
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
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(
                    "local integration source revision is admitted on multiple Threads".into(),
                ));
            }
        }
    }
    match foreign {
        None => Ok(AttachedSourceKind::Capture),
        Some((source_thread, source_operation, source_revision)) => {
            Ok(AttachedSourceKind::LocalIntegration {
                source_thread,
                source_operation,
                source_revision,
            })
        }
    }
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
