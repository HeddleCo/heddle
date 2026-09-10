// SPDX-License-Identifier: Apache-2.0
//! Durable Thread replication over native captures and collaboration operations.
//! Endpoint adapters must authorize the exact Thread and disclosure facets before
//! calling receive/export. Signatures prove the publisher, not spool membership.
#![allow(
    clippy::collapsible_if,
    clippy::items_after_test_module,
    clippy::needless_borrow,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
pub mod admission;
#[cfg(test)]
mod admission_tests;
#[cfg(test)]
mod boundary_admission_tests;
mod boundary_evidence;
pub mod checkout;
mod checkout_resolution;
mod checkout_selection;
pub use checkout_resolution::source_conflict_version;
mod genesis_admission;
mod integration;
pub mod listing;
mod local;
pub mod metadata;
pub mod ownership_claim;
#[cfg(test)]
mod ownership_claim_tests;
mod peers;
mod policy_sync;
pub mod projection;
pub mod source_authority;
mod source_index;
mod source_possession;
pub mod source_publication;
mod source_transfer;

pub mod collaboration;
pub mod collaboration_search;
mod reference_capture;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use crypto::thread_operation::{SignedGenesis, SignedOperation};
use objects::{
    object::{
        CollaborationOperationEnvelope, ContentHash, MaterializedRepositoryCollaboration, State,
        StateId, materialize_repository_collaboration,
        thread_replication::{
            Admission, ThreadFacet, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    store::ObjectStore,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

const ACCEPTED_PAGE_SQL: &str = "SELECT id,canonical,signature FROM operations WHERE thread=?1 AND status=1 AND facet=?2 AND id>COALESCE(?3,x'') ORDER BY id LIMIT ?4";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Local metadata: {0}")]
    Metadata(#[from] crate::local_metadata::Error),
    #[error("Thread storage: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("Thread object: {0}")]
    Object(#[from] objects::error::HeddleError),
    #[error("Thread filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("Thread signature: {0}")]
    Signature(#[from] crypto::SignerError),
    #[error(transparent)]
    SignedOperation(#[from] crypto::thread_operation::Error),
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct ThreadView {
    pub generation: i64,
    pub frontiers: BTreeMap<ThreadFacet, BTreeSet<ContentHash>>,
    pub source_heads: BTreeSet<StateId>,
    pub collaboration: MaterializedRepositoryCollaboration,
    pub pending: BTreeSet<ContentHash>,
    pub rejected: BTreeMap<ContentHash, String>,
}

/// Cheap to clone across stream workers. Connections and transactions remain
/// local to each operation; SQLite serializes commits across processes.
#[derive(Clone)]
pub struct ThreadReplica {
    path: PathBuf,
    thread: ContentHash,
}
pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("
            CREATE TABLE IF NOT EXISTS local_thread_names(name TEXT PRIMARY KEY, thread BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS threads(id BLOB PRIMARY KEY, genesis BLOB NOT NULL, genesis_signature BLOB NOT NULL CHECK(length(genesis_signature)=64), creator_authority BLOB NOT NULL DEFAULT X'' CHECK(length(creator_authority)<=65536), genesis_admission BLOB, genesis_admission_signature BLOB, generation INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS operations(id BLOB PRIMARY KEY, thread BLOB NOT NULL, facet INTEGER NOT NULL, canonical BLOB NOT NULL, signature BLOB NOT NULL, status INTEGER NOT NULL DEFAULT 0, reason TEXT, source_revision BLOB, authority_admitted INTEGER NOT NULL DEFAULT 0 CHECK(authority_admitted IN(0,1)), authority_receipt_canonical BLOB CHECK(authority_receipt_canonical IS NULL OR length(authority_receipt_canonical)<=2048), authority_receipt_signature BLOB CHECK(authority_receipt_signature IS NULL OR length(authority_receipt_signature)=64), CHECK((authority_receipt_canonical IS NULL)=(authority_receipt_signature IS NULL)));
            CREATE INDEX IF NOT EXISTS operations_thread_status ON operations(thread,status);
            CREATE INDEX IF NOT EXISTS operations_thread_facet_status_id ON operations(thread,facet,status,id);
            CREATE INDEX IF NOT EXISTS operations_source_revision ON operations(thread,source_revision,status,id);
            CREATE TABLE IF NOT EXISTS parents(child BLOB NOT NULL, parent BLOB NOT NULL, PRIMARY KEY(child,parent));
            CREATE INDEX IF NOT EXISTS parents_parent ON parents(parent);
            CREATE TABLE IF NOT EXISTS request_nonces(identity TEXT NOT NULL, nonce BLOB NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(identity,nonce));
            CREATE TABLE IF NOT EXISTS peer_heads(thread BLOB NOT NULL, peer BLOB NOT NULL, operation BLOB NOT NULL, facet INTEGER NOT NULL, PRIMARY KEY(thread,peer,operation));
            CREATE TABLE IF NOT EXISTS peer_receipts(thread BLOB NOT NULL, peer BLOB NOT NULL, operation BLOB NOT NULL, status INTEGER NOT NULL, reason TEXT, PRIMARY KEY(thread,peer,operation));
            CREATE TABLE IF NOT EXISTS hosted_executor_pins(spool TEXT NOT NULL,genesis BLOB NOT NULL CHECK(length(genesis)=32),executor BLOB NOT NULL CHECK(length(executor)=32),PRIMARY KEY(spool,executor));
            CREATE TABLE IF NOT EXISTS native_policy_consent(thread BLOB PRIMARY KEY,account TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sharing(thread BLOB NOT NULL, destination BLOB NOT NULL, facets INTEGER NOT NULL, version BLOB NOT NULL, PRIMARY KEY(thread,destination));
            CREATE TABLE IF NOT EXISTS thread_control_heads(thread BLOB NOT NULL,property TEXT NOT NULL,operation BLOB NOT NULL,PRIMARY KEY(thread,property,operation));
            CREATE TABLE IF NOT EXISTS thread_control_commands(thread BLOB NOT NULL,publisher BLOB NOT NULL,command BLOB NOT NULL,operation BLOB NOT NULL,PRIMARY KEY(thread,publisher,command));")?;
    connection.execute_batch(ownership_claim::SCHEMA)?;
    connection.execute_batch(boundary_evidence::SCHEMA)?;
    connection.execute_batch(collaboration::SCHEMA)?;
    connection.execute_batch(collaboration_search::SCHEMA)?;
    connection.execute_batch(source_index::SCHEMA)?;
    connection.execute_batch(listing::SCHEMA)
}

impl ThreadReplica {
    /// Create or relay a Thread using the creator's original signed identity.
    /// Verify before touching disk; possession of the key is not needed to relay.
    pub fn create(heddle_dir: &Path, signed: &SignedGenesis) -> Result<Self> {
        let genesis = signed.verify()?;
        if !matches!(
            genesis.owner,
            objects::object::thread_replication::GenesisOwner::LocalKey(_)
        ) {
            return Err(Error::Invalid(
                "account-owned Thread requires original authority admission".into(),
            ));
        }
        Self::create_with_proof(heddle_dir, signed, &[], None)
    }

    /// Original account authority and signed genesis become durable together.
    /// The caller independently authorizes delivery and the selected Spool.
    pub fn create_authorized(
        heddle_dir: &Path,
        signed: &SignedGenesis,
        creator_authority: &[u8],
        authority: &crate::device_authority::DeviceAuthority,
        spool_path: &str,
        method: &str,
        now: i64,
    ) -> Result<Self> {
        let genesis = signed.verify()?;
        metadata::verify_genesis_authority(
            &genesis,
            creator_authority,
            authority,
            spool_path,
            method,
            now,
        )?;
        Self::create_with_proof(heddle_dir, signed, creator_authority, None)
    }

    fn create_with_proof(
        heddle_dir: &Path,
        signed: &SignedGenesis,
        creator_authority: &[u8],
        admission: Option<&crypto::thread_genesis_admission::SignedGenesisAdmission>,
    ) -> Result<Self> {
        let genesis = signed.verify()?;
        objects::fs_atomic::create_dir_all_durable(heddle_dir)?;
        let this = Self {
            path: heddle_dir.join(crate::local_metadata::DATABASE_NAME),
            thread: genesis.id()?,
        };
        let mut connection = crate::local_metadata::open(heddle_dir)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO threads(id,genesis,genesis_signature,creator_authority) VALUES(?1,?2,?3,?4)",
            params![this.thread.as_bytes(), &signed.canonical, &signed.signature, creator_authority],
        )?;
        let stored: (Vec<u8>, Vec<u8>, Vec<u8>) = transaction.query_row(
            "SELECT genesis,genesis_signature,creator_authority FROM threads WHERE id=?1",
            [this.thread.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if stored.0 != signed.canonical
            || stored.1 != signed.signature
            || stored.2 != creator_authority
        {
            return Err(Error::Invalid("Thread genesis collision".into()));
        }
        if let Some(admission) = admission {
            let existing: (Option<Vec<u8>>, Option<Vec<u8>>) = transaction.query_row(
                "SELECT genesis_admission,genesis_admission_signature FROM threads WHERE id=?1",
                [this.thread.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            match existing {
                (None, None) => {
                    transaction.execute("UPDATE threads SET genesis_admission=?2,genesis_admission_signature=?3 WHERE id=?1",params![this.thread.as_bytes(),admission.canonical,admission.signature])?;
                    boundary_evidence::persist(
                        &transaction,
                        &admission.canonical,
                        admission.boundary_acceptance.as_deref(),
                    )?;
                }
                (Some(canonical), Some(signature))
                    if canonical == admission.canonical && signature == admission.signature => {}
                _ => {
                    return Err(Error::Invalid(
                        "conflicting original genesis admission receipt".into(),
                    ));
                }
            }
        }
        transaction.execute(
            "INSERT OR IGNORE INTO thread_source_bases(thread,revision) VALUES(?1,?2)",
            params![this.thread.as_bytes(), genesis.base.as_bytes()],
        )?;
        listing::initialize(&transaction, &genesis)?;
        reference_capture::inherit_root(&transaction, &genesis)?;
        transaction.commit()?;
        this.notify_committed()?;
        Ok(this)
    }

    /// Lookup is side-effect free and needs only stable identity, not a key or
    /// caller-supplied genesis. Unknown IDs never create another replica.
    pub fn open(heddle_dir: &Path, thread: ContentHash) -> Result<Self> {
        let this = Self {
            path: heddle_dir.join(crate::local_metadata::DATABASE_NAME),
            thread,
        };
        this.signed_genesis()?;
        Ok(this)
    }

    pub fn signed_genesis(&self) -> Result<SignedGenesis> {
        let signed = self.connect()?.query_row(
            "SELECT genesis,genesis_signature FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |row| {
                Ok(SignedGenesis {
                    canonical: row.get(0)?,
                    signature: row.get(1)?,
                })
            },
        )?;
        if signed.verify()?.id()? != self.thread {
            return Err(Error::Invalid(
                "stored creator proof differs from Thread identity".into(),
            ));
        }
        Ok(signed)
    }

    pub fn genesis_record(&self) -> Result<api::heddle::api::v2alpha1::ThreadGenesisRecord> {
        let (canonical, signature, creator_authority, admission, admission_signature): (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>) =
            self.connect()?.query_row(
                "SELECT genesis,genesis_signature,creator_authority,genesis_admission,genesis_admission_signature FROM threads WHERE id=?1",
                [self.thread.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?,row.get(3)?,row.get(4)?)),
            )?;
        let original_genesis = SignedGenesis {
            canonical,
            signature,
        };
        let genesis = original_genesis.verify()?;
        if genesis.id()? != self.thread {
            return Err(Error::Invalid(
                "stored genesis differs from Thread identity".into(),
            ));
        }
        use api::heddle::api::v2alpha1 as wire;
        let mut boundary_acceptances = BTreeMap::new();
        let admission = match (admission, admission_signature) {
            (None, None) => None,
            (Some(canonical), Some(signature)) => {
                let mut signed = crypto::thread_genesis_admission::SignedGenesisAdmission {
                    boundary_acceptance: None,
                    canonical,
                    signature,
                };
                let value = signed.verify_signature()?;
                let trust =
                    objects::object::thread_replication::integration::TrustedHostedExecutor {
                        spool: value.spool,
                        spool_genesis: value.spool_genesis,
                        executor: value.executor,
                    };
                // Receipt trust was independently established at insertion. This
                // checks stored bindings; transport receivers must pin it anew.
                signed.boundary_acceptance =
                    boundary_evidence::load(&self.connect()?, &signed.canonical, &value.basis)?;
                signed.verify(&original_genesis, &creator_authority, &trust)?;
                boundary_evidence::add_wire(
                    &mut boundary_acceptances,
                    signed.boundary_acceptance.as_deref(),
                )?;
                Some(wire::SignedRecord {
                    format: objects::object::thread_genesis_admission::FORMAT.into(),
                    canonical_record: signed.canonical,
                    signatures: vec![wire::RecordSignature {
                        public_key: value.executor.to_vec(),
                        signature: signed.signature,
                    }],
                })
            }
            _ => return Err(Error::Invalid("incomplete stored genesis admission".into())),
        };
        let mut ownership_claims = Vec::new();
        let mut ownership_claim_admissions = Vec::new();
        for (claim, admission) in self.ownership_claims_with_admission()? {
            let value = claim.verify()?;
            value.validate_genesis(&genesis)?;
            ownership_claims.push(wire::SignedRecord {
                format: objects::object::thread_replication::ownership_claim::FORMAT.into(),
                canonical_record: claim.canonical.clone(),
                signatures: vec![
                    wire::RecordSignature {
                        public_key: value.prior_local_key.to_vec(),
                        signature: claim.local_signature.clone(),
                    },
                    wire::RecordSignature {
                        public_key: value.accepting_publisher.to_vec(),
                        signature: claim.acceptance_signature.clone(),
                    },
                ],
            });
            if let Some(receipt) = admission {
                let statement = receipt.verify_signature()?;
                receipt.verify_claim(
                    &claim,
                    &genesis,
                    &self.authority_admission_trust(&receipt)?,
                )?;
                boundary_evidence::add_wire(
                    &mut boundary_acceptances,
                    receipt.boundary_acceptance.as_deref(),
                )?;
                ownership_claim_admissions.push(wire::SignedRecord {
                    format: objects::object::thread_authority_admission::FORMAT.into(),
                    canonical_record: receipt.canonical,
                    signatures: vec![wire::RecordSignature {
                        public_key: statement.executor.to_vec(),
                        signature: receipt.signature,
                    }],
                });
            }
        }
        Ok(wire::ThreadGenesisRecord {
            boundary_acceptances: boundary_acceptances.into_values().collect(),
            ownership_claims,
            ownership_claim_admissions,
            genesis: Some(wire::SignedRecord {
                format: objects::object::thread_replication::GENESIS_FORMAT.into(),
                canonical_record: original_genesis.canonical,
                signatures: vec![wire::RecordSignature {
                    public_key: genesis.creator.to_vec(),
                    signature: original_genesis.signature,
                }],
            }),
            creator_authority,
            admission,
        })
    }

    fn connect(&self) -> Result<Connection> {
        self.connect_with_flags(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
    }
    fn connect_with_flags(&self, flags: rusqlite::OpenFlags) -> Result<Connection> {
        let connection = Connection::open_with_flags(&self.path, flags)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        Ok(connection)
    }
    pub(super) fn notify_committed(&self) -> Result<()> {
        objects::fs_atomic::write_file_atomic_secret(
            &self.path.with_extension("sqlite3.changed"),
            uuid::Uuid::now_v7().as_bytes(),
        )?;
        Ok(())
    }
    pub fn thread_id(&self) -> ContentHash {
        self.thread
    }
    pub fn genesis(&self) -> Result<ThreadGenesis> {
        let connection = self.connect()?;
        let bytes: Vec<u8> = connection.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        Ok(ThreadGenesis::decode(&bytes)?)
    }
    pub fn generation(&self) -> Result<i64> {
        Ok(self.connect()?.query_row(
            "SELECT generation FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?)
    }

    /// Indexed membership lookup, independent of repository object presence.
    /// Pending or rejected source operations cannot authorize content or checkout reads.
    pub fn accepted_source_revision(&self, revision: StateId) -> Result<Option<State>> {
        let bytes: Option<Vec<u8>> = self.connect()?.query_row(
            "SELECT canonical FROM operations WHERE thread=?1 AND source_revision=?2 AND status=1 ORDER BY id LIMIT 1",
            params![self.thread.as_bytes(), revision.as_bytes()], |row| row.get(0),
        ).optional()?;
        bytes
            .map(|bytes| {
                let operation = ThreadOperation::decode(&bytes)?;
                let state = operation.source_state()?.ok_or_else(|| {
                    Error::Invalid("source index names a non-source operation".into())
                })?;
                if operation.thread != self.thread || state.id() != revision {
                    return Err(Error::Invalid(
                        "source index differs from canonical identity".into(),
                    ));
                }
                Ok(state)
            })
            .transpose()
    }

    /// Matching causal parents without scanning or decoding unrelated history.
    pub fn source_operation_page(
        &self,
        revision: StateId,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<ContentHash>> {
        if !(1..=1024).contains(&limit) {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT id FROM operations WHERE thread=?1 AND source_revision=?2 AND status=1 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4")?;
        let rows = query.query_map(
            params![
                self.thread.as_bytes(),
                revision.as_bytes(),
                after.map(|id| id.as_bytes().to_vec()),
                limit as u32
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        rows.map(|row| hash(&row?)).collect()
    }

    /// Persist a verified operation and admit newly complete causal descendants.
    /// The caller's authorization is evaluated before any private bytes persist.
    /// Native capture objects become durable before the acceptance transaction.
    pub fn receive(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        self.receive_inner(signed, store, authorize, false, None)
    }
    /// Execute local landing only against the exact currently observed target
    /// frontier. Historical replication uses `receive` and preserves branches.
    pub fn receive_local_integration_cas(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        if signed.verify()?.local_integration()?.is_none() {
            return Err(Error::Invalid(
                "local integration CAS requires a local receipt".into(),
            ));
        }
        self.receive_inner(signed, store, authorize, true, None)
    }
    fn receive_inner(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
        compare_frontier: bool,
        authority_receipt: Option<&crypto::thread_authority_admission::SignedAuthorityAdmission>,
    ) -> Result<Admission> {
        let operation = signed.verify()?;
        if operation.thread != self.thread {
            return Err(Error::Invalid("wrong Thread".into()));
        }
        self.require_trusted_integration(&operation)?;
        self.require_local_integration_source(&operation)?;
        if let Some(receipt) = authority_receipt {
            self.require_authority_admission(signed, receipt)?;
        }
        authorize(&operation)?;
        self.validate_reference_capture(&operation, store)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let admission = self.receive_in(
            &tx,
            signed,
            &operation,
            store,
            compare_frontier,
            authority_receipt,
        )?;
        tx.commit()?;
        self.notify_committed()?;
        Ok(admission)
    }
    fn receive_in(
        &self,
        tx: &Transaction<'_>,
        signed: &SignedOperation,
        operation: &ThreadOperation,
        store: &impl ObjectStore,
        compare_frontier: bool,
        authority_receipt: Option<&crypto::thread_authority_admission::SignedAuthorityAdmission>,
    ) -> Result<Admission> {
        let id = operation.id()?;
        let already_accepted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND canonical=?2 AND signature=?3 AND status=1)",
            params![id.as_bytes(), &signed.canonical, &signed.signature], |row| row.get(0),
        )?;
        if !already_accepted {
            self.validate_source_owner_in(tx, operation)?;
        }

        if compare_frontier && let Some(receipt) = operation.local_integration()? {
            let existing: Option<i32> = tx
                .query_row(
                    "SELECT status FROM operations WHERE id=?1",
                    [id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            if existing != Some(1) {
                let mut query = tx.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=1 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT 129")?;
                let frontier = query
                    .query_map([self.thread.as_bytes().as_slice()], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })?
                    .map(|row| hash(&row?))
                    .collect::<Result<BTreeSet<_>>>()?;
                if frontier != receipt.expected_target_frontier {
                    return Err(Error::Invalid(
                        "local integration target frontier changed".into(),
                    ));
                }
            }
        }
        self.check_control_command(&tx, &operation, id, compare_frontier)?;
        let source_revision = match &operation.body {
            ThreadOperationBody::Capture(bytes) => {
                Some(State::decode_current_msgpack(&bytes.result.state)?.id())
            }
            ThreadOperationBody::Integration(_)
            | ThreadOperationBody::HostedImport(_)
            | ThreadOperationBody::LocalIntegration(_) => {
                operation.source_state()?.map(|state| state.id())
            }
            ThreadOperationBody::Discussion(_)
            | ThreadOperationBody::Context(_)
            | ThreadOperationBody::Metadata(_) => None,
        };
        let inserted = tx.execute("INSERT OR IGNORE INTO operations(id,thread,facet,canonical,signature,source_revision) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id.as_bytes(), self.thread.as_bytes(), facet_number(operation.facet()), signed.canonical, signed.signature, source_revision.map(|id| id.as_bytes().to_vec())])?;
        // The host's original-author gate ran before any immutable bytes were
        // installed. Persist its successful admission in this same transaction;
        // missing causal parents may arrive after the original credential expires.
        if objects::object::thread_authority_admission::OriginalAuthorityBinding::from_operation(
            &operation,
        )?
        .is_some()
        {
            tx.execute(
                "UPDATE operations SET authority_admitted=1 WHERE id=?1 AND authority_admitted=0",
                [id.as_bytes()],
            )?;
        }
        if let Some(receipt) = authority_receipt {
            let stored = tx.execute(
                "UPDATE operations SET authority_receipt_canonical=?2,authority_receipt_signature=?3 WHERE id=?1 AND authority_receipt_canonical IS NULL",
                params![id.as_bytes(), receipt.canonical, receipt.signature],
            )?;
            if stored > 0 {
                boundary_evidence::persist(
                    tx,
                    &receipt.canonical,
                    receipt.boundary_acceptance.as_deref(),
                )?;
            }
            if stored > 0 && inserted == 0 {
                tx.execute(
                    "UPDATE threads SET generation=generation+1 WHERE id=?1",
                    [self.thread.as_bytes()],
                )?;
            }
        }
        if inserted > 0 {
            for parent in &operation.parents {
                tx.execute(
                    "INSERT INTO parents(child,parent) VALUES(?1,?2)",
                    params![id.as_bytes(), parent.as_bytes()],
                )?;
            }
            tx.execute(
                "UPDATE threads SET generation=generation+1 WHERE id=?1",
                [self.thread.as_bytes()],
            )?;
        }
        collaboration::index_operation(tx, operation)?;
        self.admit_ready(tx, store)?;
        status(tx, &id)
    }

    fn admit_ready(&self, tx: &Transaction<'_>, store: &impl ObjectStore) -> Result<()> {
        let genesis_bytes: Vec<u8> = tx.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        let genesis = ThreadGenesis::decode(&genesis_bytes)?;
        loop {
            let ready: Vec<Vec<u8>> = tx.prepare("SELECT o.canonical FROM operations o WHERE o.thread=?1 AND o.status=0 AND (o.facet<>3 OR o.authority_admitted=1) AND (EXISTS (SELECT 1 FROM parents p JOIN operations a ON a.id=p.parent WHERE p.child=o.id AND (a.status=2 OR a.thread<>o.thread OR a.facet<>o.facet)) OR NOT EXISTS (SELECT 1 FROM parents p LEFT JOIN operations a ON a.id=p.parent WHERE p.child=o.id AND (a.id IS NULL OR a.status<>1))) ORDER BY o.id LIMIT 128")?
                .query_map([self.thread.as_bytes()], |r| r.get(0))?.collect::<std::result::Result<_,_>>()?;
            if ready.is_empty() {
                break;
            }
            for bytes in ready {
                let operation = ThreadOperation::decode(&bytes)?;
                let id = operation.id()?;
                let parent_failure: Option<String> = tx.query_row(
                    "SELECT CASE WHEN a.thread<>?2 OR a.facet<>?3 THEN 'causal parents cross Thread or disclosure facet' ELSE 'causal parent was rejected' END FROM parents p JOIN operations a ON a.id=p.parent WHERE p.child=?1 AND (a.status=2 OR a.thread<>?2 OR a.facet<>?3) ORDER BY a.id LIMIT 1",
                    params![id.as_bytes(), self.thread.as_bytes(), facet_number(operation.facet())],
                    |r| r.get(0),
                ).optional()?;
                if let Some(reason) = parent_failure {
                    tx.execute(
                        "UPDATE operations SET status=2,reason=?2 WHERE id=?1",
                        params![id.as_bytes(), reason],
                    )?;
                    tx.execute(
                        "UPDATE threads SET generation=generation+1 WHERE id=?1",
                        [self.thread.as_bytes()],
                    )?;
                    continue;
                }
                let mut parents = Vec::new();
                for parent in &operation.parents {
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT canonical FROM operations WHERE id=?1",
                        [parent.as_bytes()],
                        |r| r.get(0),
                    )?;
                    parents.push(ThreadOperation::decode(&bytes)?);
                }
                if let Err(error) = self.validate_source_owner_in(tx, &operation) {
                    tx.execute(
                        "UPDATE operations SET status=2,reason=?2 WHERE id=?1",
                        params![id.as_bytes(), error.to_string()],
                    )?;
                    tx.execute(
                        "UPDATE threads SET generation=generation+1 WHERE id=?1",
                        [self.thread.as_bytes()],
                    )?;
                    continue;
                }
                match operation.validate_parents(&genesis, &parents) {
                    Ok(()) => {
                        if let Some(state) = operation.source_state()? {
                            if let Some(receipt) = operation.local_integration()? {
                                use objects::object::{
                                    StateVisibility, StateVisibilityBlob, VisibilityTier,
                                    thread_replication::local_integration::intersect_visibility,
                                };
                                let mut required = VisibilityTier::Public;
                                for parent in &state.parents {
                                    if let Some(bytes) =
                                        store.get_state_visibility_bytes_for_state(parent)?
                                    {
                                        let blob = StateVisibilityBlob::decode(&bytes)
                                            .map_err(|e| Error::Invalid(e.to_string()))?;
                                        if let Some(record) = blob
                                            .latest()
                                            .map_err(|e| Error::Invalid(e.to_string()))?
                                        {
                                            required =
                                                intersect_visibility(&required, &record.tier)?;
                                        }
                                    }
                                }
                                if intersect_visibility(&required, &receipt.result_visibility)?
                                    != receipt.result_visibility
                                {
                                    return Err(Error::Invalid(
                                        "local integration weakens source audience".into(),
                                    ));
                                }
                                if receipt.result_visibility != VisibilityTier::Public
                                    && !store.has_state_visibility_for_state(&state.id())?
                                {
                                    let blob = StateVisibilityBlob::new(vec![StateVisibility {
                                        state: state.id(),
                                        tier: receipt.result_visibility,
                                        embargo_until: None,
                                        declarer: state.attribution.principal.clone(),
                                        declared_at: state.created_at,
                                        signature: None,
                                        supersedes: None,
                                    }]);
                                    store.put_state_visibility_bytes_for_state(
                                        &state.id(),
                                        &blob
                                            .encode()
                                            .map_err(|e| Error::Invalid(e.to_string()))?,
                                    )?;
                                }
                            }
                            store.put_state(&state)?;
                        }
                        self.accept_control_heads(tx, &operation, id)?;
                        self.index_reference_sources(tx, &operation, id)?;
                        tx.execute(
                            "UPDATE operations SET status=1 WHERE id=?1",
                            [id.as_bytes()],
                        )?;
                        self.publish_capture_references(tx, &operation, id, store)?;
                    }
                    Err(error) => {
                        tx.execute(
                            "UPDATE operations SET status=2,reason=?2 WHERE id=?1",
                            params![id.as_bytes(), error.to_string()],
                        )?;
                    }
                }
                tx.execute(
                    "UPDATE threads SET generation=generation+1 WHERE id=?1",
                    [self.thread.as_bytes()],
                )?;
            }
        }
        Ok(())
    }

    /// Claim a verified request nonce durably, across streams and restarts.
    pub fn claim_request_nonce(&self, identity: &str, nonce: &[u8], now_ms: i64) -> Result<bool> {
        if nonce.len() != 16 {
            return Err(Error::Invalid("invalid request nonce".into()));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM request_nonces WHERE expires<=?1", [now_ms])?;
        let count: i64 = tx.query_row("SELECT count(*) FROM request_nonces", [], |r| r.get(0))?;
        if count >= 16384 {
            return Err(Error::Invalid("request replay registry at capacity".into()));
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO request_nonces(identity,nonce,expires) VALUES(?1,?2,?3)",
            params![identity, nonce, now_ms.saturating_add(120000)],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Find a bounded page of unknown ancestors below a persisted pending
    /// operation. Known pending parents are traversed after reconnect; accepted
    /// history is already closed and never needs to be walked again.
    pub fn missing_ancestors(&self, root: ContentHash, limit: usize) -> Result<Vec<ContentHash>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare(
            "WITH RECURSIVE unresolved(id) AS (
                SELECT id FROM operations WHERE id=?1 AND thread=?2 AND status=0
                UNION
                SELECT p.parent FROM unresolved u JOIN operations o ON o.id=u.id AND o.status=0 AND o.thread=?2 JOIN parents p ON p.child=o.id
            ) SELECT u.id FROM unresolved u LEFT JOIN operations o ON o.id=u.id WHERE o.id IS NULL ORDER BY u.id LIMIT ?3"
        )?;
        query
            .query_map(
                params![root.as_bytes(), self.thread.as_bytes(), limit as u32],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .map(|row| hash(&row?))
            .collect()
    }

    pub fn frontier_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<ContentHash>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=?2 AND o.status=1 AND (?3 IS NULL OR o.id>?3) AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT ?4")?;
        query
            .query_map(
                params![
                    self.thread.as_bytes(),
                    facet_number(facet),
                    after.map(|id| id.as_bytes().to_vec()),
                    limit as u32
                ],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .map(|row| hash(&row?))
            .collect()
    }

    pub fn operation(&self, id: &ContentHash) -> Result<Option<(SignedOperation, Admission)>> {
        Ok(self
            .operation_with_authority_admission(id)?
            .map(|stored| (stored.original, stored.status)))
    }

    /// Bounded durable paging, also used after reconnect. The cursor is an
    /// operation ID within this Thread, not an observation cursor.
    pub fn accepted_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, SignedOperation)>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare(ACCEPTED_PAGE_SQL)?;
        let rows = query.query_map(
            params![
                self.thread.as_bytes(),
                facet_number(facet),
                after.map(|id| id.as_bytes().to_vec()),
                limit as u32
            ],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    SignedOperation {
                        canonical: r.get(1)?,
                        signature: r.get(2)?,
                    },
                ))
            },
        )?;
        rows.map(|row| {
            let (id, record) = row?;
            Ok((hash(&id)?, record))
        })
        .collect()
    }

    /// Snapshot and generation are read in one SQLite transaction. Stream
    /// adapters compare the generation after subscribing, closing the handoff gap.
    pub fn view(&self) -> Result<ThreadView> {
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let generation = tx.query_row(
            "SELECT generation FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        let mut accepted = BTreeMap::new();
        let mut pending = BTreeSet::new();
        let mut rejected = BTreeMap::new();
        let rows = tx
            .prepare(
                "SELECT id,canonical,status,reason FROM operations WHERE thread=?1 ORDER BY id",
            )?
            .query_map([self.thread.as_bytes()], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, bytes, status, reason) in rows {
            let id = hash(&id)?;
            match status {
                1 => {
                    accepted.insert(id, ThreadOperation::decode(&bytes)?);
                }
                2 => {
                    rejected.insert(id, reason.unwrap_or_default());
                }
                _ => {
                    pending.insert(id);
                }
            }
        }
        let mut frontiers: BTreeMap<ThreadFacet, BTreeSet<ContentHash>> = BTreeMap::new();
        let mut discussion = BTreeMap::new();
        for (id, operation) in &accepted {
            frontiers.entry(operation.facet()).or_default().insert(*id);
            if let ThreadOperationBody::Discussion(bytes) = &operation.body {
                let decoded = CollaborationOperationEnvelope::decode(bytes)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                discussion.insert(decoded.operation_id, decoded);
            }
        }
        for operation in accepted.values() {
            if let Some(heads) = frontiers.get_mut(&operation.facet()) {
                for parent in &operation.parents {
                    heads.remove(parent);
                }
            }
        }
        let mut source_heads = BTreeSet::new();
        if let Some(heads) = frontiers.get(&ThreadFacet::Source) {
            for head in heads {
                if let Some(operation) = accepted.get(head) {
                    if let Some(state) = operation.source_state()? {
                        source_heads.insert(state.id());
                    }
                }
            }
        }
        let collaboration = materialize_repository_collaboration(discussion.into_values())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        tx.commit()?;
        Ok(ThreadView {
            generation,
            frontiers,
            source_heads,
            collaboration,
            pending,
            rejected,
        })
    }

    /// The caller must hold Thread policy-write authority. Empty facets revoke
    /// future export; revocation never claims to erase already disclosed bytes.
    pub fn set_sharing(
        &self,
        destination: [u8; 32],
        facets: &BTreeSet<ThreadFacet>,
    ) -> Result<ContentHash> {
        let bits = facet_bits(facets);
        let mut bytes = self.thread.as_bytes().to_vec();
        bytes.extend(destination);
        bytes.extend(bits.to_be_bytes());
        let version = ContentHash::compute_typed("thread-sharing-v2", &bytes);
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed=tx.execute("INSERT INTO sharing(thread,destination,facets,version) VALUES(?1,?2,?3,?4) ON CONFLICT(thread,destination) DO UPDATE SET facets=excluded.facets,version=excluded.version WHERE sharing.version<>excluded.version",params![self.thread.as_bytes(),destination,bits,version.as_bytes()])?;
        if changed > 0 {
            tx.execute(
                "UPDATE threads SET generation=generation+1 WHERE id=?1",
                [self.thread.as_bytes()],
            )?;
        }
        tx.commit()?;
        self.notify_committed()?;
        Ok(version)
    }

    pub fn sharing(
        &self,
        destination: &[u8; 32],
    ) -> Result<(BTreeSet<ThreadFacet>, Option<ContentHash>)> {
        if let Some(policy) = self.policy_sync_sharing(destination)? {
            return Ok(policy);
        }
        let row: Option<(i64, Vec<u8>)> = self
            .connect()?
            .query_row(
                "SELECT facets,version FROM sharing WHERE thread=?1 AND destination=?2",
                params![self.thread.as_bytes(), destination],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((bits, version)) = row else {
            return Ok((BTreeSet::new(), None));
        };
        Ok((
            ThreadFacet::ALL
                .into_iter()
                .filter(|facet| bits & (1i64 << facet_number(*facet)) != 0)
                .collect(),
            Some(hash(&version)?),
        ))
    }
}

fn status(connection: &Connection, id: &ContentHash) -> Result<Admission> {
    let (status, reason): (i32, Option<String>) = connection.query_row(
        "SELECT status,reason FROM operations WHERE id=?1",
        [id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(match status {
        1 => Admission::Accepted,
        2 => Admission::Rejected(reason.unwrap_or_default()),
        _ => Admission::Pending,
    })
}
fn facet_number(facet: ThreadFacet) -> u32 {
    match facet {
        ThreadFacet::Source => 1,
        ThreadFacet::Discussion => 2,
        ThreadFacet::Metadata => 3,
    }
}
fn facet_bits(facets: &BTreeSet<ThreadFacet>) -> i64 {
    facets
        .iter()
        .fold(0, |bits, facet| bits | (1i64 << facet_number(*facet)))
}
fn hash(bytes: &[u8]) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(bytes.try_into().map_err(|_| {
        Error::Invalid("invalid operation ID length".into())
    })?))
}

#[cfg(test)]
mod tests;
