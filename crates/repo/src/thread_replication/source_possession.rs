//! Source metadata and locally available source are distinct. Only a trusted
//! local producer or a fully validated incoming pack may attest possession.
use std::collections::BTreeSet;

use crypto::thread_operation::SignedOperation;
use objects::{
    object::{
        AttributionEvidenceV1, ContentHash, State, StateId, TreeEntryTarget,
        thread_replication::ThreadOperation,
    },
    store::ObjectStore,
};
use rusqlite::{Transaction, params};

use super::{Admission, Error, Result, ThreadReplica};

impl ThreadReplica {
    /// Availability is independent of current audience. Callers still authorize
    /// the Thread before disclosing metadata and the revision before raw reads.
    pub fn has_source_possession(&self, revision: StateId) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_source_availability WHERE thread=?1 AND revision=?2)",
            params![self.thread.as_bytes(), revision.as_bytes()], |row| row.get(0),
        )?)
    }

    /// The caller must have independently authorized and completely validated
    /// the supplied source/reference closure. Never call this because a signed
    /// State header names objects already present in a shared object store.
    pub fn record_source_possession(&self, revision: StateId) -> Result<()> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = self.record_source_possession_in(&tx, revision)?;
        tx.commit()?;
        if changed {
            self.notify_committed()?;
        }
        Ok(())
    }

    pub(super) fn record_source_possession_in(
        &self,
        tx: &Transaction<'_>,
        revision: StateId,
    ) -> Result<bool> {
        let admitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_source_revisions WHERE thread=?1 AND revision=?2 UNION SELECT 1 FROM thread_source_bases WHERE thread=?1 AND revision=?2)",
            params![self.thread.as_bytes(),revision.as_bytes()], |row| row.get(0))?;
        if !admitted {
            return Err(Error::Invalid(
                "source possession requires an admitted source or exact genesis base".into(),
            ));
        }
        let references_pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reference_projection_pending WHERE thread=?1 AND revision=?2)",
            params![self.thread.as_bytes(), revision.as_bytes()], |row| row.get(0))?;
        if references_pending {
            return Err(Error::ReferenceProjectionPending);
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO thread_source_availability(thread,revision) VALUES(?1,?2)",
            params![self.thread.as_bytes(), revision.as_bytes()],
        )?;
        if inserted != 0 {
            tx.execute(
                "UPDATE threads SET generation=generation+1 WHERE id=?1",
                [self.thread.as_bytes()],
            )?;
        }
        Ok(inserted != 0)
    }

    /// Local capture/checkout execution already produced the complete source
    /// objects. Publish its operation, reference root, possession, and wakeup
    /// generation together. Remote replication must use ordinary receive.
    pub fn receive_prepared_source(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        let operation = signed.verify()?;
        if operation.thread != self.thread {
            return Err(Error::Invalid("wrong Thread".into()));
        }
        let state = operation
            .source_state()?
            .ok_or_else(|| Error::Invalid("prepared source operation required".into()))?;
        self.require_trusted_integration(&operation)?;
        self.require_local_integration_source(&operation)?;
        authorize(&operation)?;
        self.validate_reference_capture(&operation, store)?;
        validate_attribution_evidence(store, &state)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let admission = self.receive_in(
            &tx,
            signed,
            &operation,
            store,
            operation.local_integration()?.is_some(),
            None,
            false,
        )?;
        if admission != Admission::Accepted {
            // Roll back the receive transaction. Callers report Pending/Rejected
            // instead of an opaque "did not settle".
            return Ok(admission);
        }
        self.record_source_possession_in(&tx, state.id())?;
        tx.commit()?;
        self.notify_committed()?;
        Ok(admission)
    }

    /// Bootstrap is a trusted local action, not admission of a remotely supplied
    /// genesis. Walk the exact tree closure before marking its initial source.
    pub(super) fn validate_local_source_possession(
        &self,
        store: &impl ObjectStore,
        revision: StateId,
    ) -> Result<()> {
        let connection = self.connect()?;
        let present: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM thread_source_availability WHERE thread=?1 AND revision=?2)", params![self.thread.as_bytes(),revision.as_bytes()], |row| row.get(0))?;
        drop(connection);
        if present {
            return Ok(());
        }
        let state = store
            .get_state(&revision)?
            .ok_or_else(|| Error::Invalid("initial source State missing".into()))?;
        let evidence_bytes = validate_attribution_evidence(store, &state)?;
        let mut trees = vec![state.tree];
        let mut seen = BTreeSet::<ContentHash>::new();
        let mut bytes = evidence_bytes;
        while let Some(hash) = trees.pop() {
            if !seen.insert(hash) {
                continue;
            }
            if seen.len() > 1_000_000 {
                return Err(Error::Invalid(
                    "initial source object budget exceeded".into(),
                ));
            }
            let tree = store
                .get_tree(&hash)?
                .ok_or_else(|| Error::Invalid("initial source tree missing".into()))?;
            for entry in tree.entries() {
                match entry.target() {
                    TreeEntryTarget::Tree { hash } => trees.push(*hash),
                    TreeEntryTarget::Blob { hash, .. } if seen.insert(*hash) => {
                        let blob = store
                            .get_blob(hash)?
                            .ok_or_else(|| Error::Invalid("initial source blob missing".into()))?;
                        bytes = bytes
                            .checked_add(blob.size() as u64)
                            .ok_or_else(|| Error::Invalid("initial source byte overflow".into()))?;
                        if bytes > 512 * 1024 * 1024 || seen.len() > 1_000_000 {
                            return Err(Error::Invalid(
                                "initial source closure budget exceeded".into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.record_source_possession(revision)
    }
}

/// Required State metadata belongs to source possession even when no file tree
/// points to it. Causal metadata admission remains independent of possession.
pub(super) fn validate_attribution_evidence(
    store: &impl ObjectStore,
    state: &State,
) -> Result<u64> {
    let Some(hash) = state.attribution_evidence else {
        return Ok(0);
    };
    let blob = store
        .get_blob(&hash)?
        .ok_or_else(|| Error::Invalid("source attribution evidence missing".into()))?;
    AttributionEvidenceV1::from_blob_with_hash(&blob, hash)
        .and_then(|evidence| evidence.validate_legacy_agent(state.attribution.agent.as_ref()))
        .map_err(|error| Error::Invalid(format!("invalid source attribution evidence: {error}")))?;
    Ok(blob.size() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objects::{
        object::{
            Attribution, AttributionBasis, AttributionClaim, AttributionSource, Blob, Principal,
            Tree,
        },
        store::InMemoryStore,
    };

    #[test]
    fn source_possession_requires_valid_state_bound_attribution() {
        let store = InMemoryStore::new();
        let evidence = AttributionEvidenceV1 {
            harness: Some(AttributionClaim::new(
                "codex",
                AttributionBasis::Observed,
                AttributionSource::Process,
            )),
            ..Default::default()
        }
        .to_blob()
        .expect("evidence");
        let legacy = State::new(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("Test", "test@example.test")),
        );
        assert_eq!(
            validate_attribution_evidence(&store, &legacy).expect("legacy"),
            0
        );
        let state = legacy.with_attribution_evidence(evidence.hash());
        assert!(validate_attribution_evidence(&store, &state).is_err());
        store.put_blob(&evidence).expect("blob");
        assert_eq!(
            validate_attribution_evidence(&store, &state).expect("bound evidence"),
            evidence.size() as u64
        );
        let bad = Blob::new(b"ordinary file bytes".to_vec());
        store.put_blob(&bad).expect("bad blob");
        assert!(
            validate_attribution_evidence(&store, &state.with_attribution_evidence(bad.hash()))
                .is_err()
        );
    }
    #[test]
    fn attribution_metadata_survives_without_claiming_full_source_possession() {
        use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
        use objects::object::thread_replication::{
            AuthoredCapture, GenesisOwner, ThreadGenesis, ThreadOperationBody,
        };
        let directory = tempfile::tempdir().expect("repository");
        let repository = crate::Repository::init_default(directory.path()).expect("init");
        let signer = Ed25519Signer::from_seed(&[47; 32]).expect("signer");
        let key = signer.public_key().try_into().expect("key");
        let base = repository.head().expect("head").expect("base");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(47).to_string(),
            owner: GenesisOwner::LocalKey(key),
            creator: key,
            parent: None,
            base,
            name: "attribution".into(),
            intent: "source metadata".into(),
            nonce: vec![],
        };
        let replica = ThreadReplica::create(
            repository.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("thread");
        let evidence = AttributionEvidenceV1 {
            harness: Some(AttributionClaim::new(
                "codex",
                AttributionBasis::Observed,
                AttributionSource::Process,
            )),
            ..Default::default()
        }
        .to_blob()
        .expect("evidence");
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![base],
            Attribution::human(Principal::new("Test", "test@example.test")),
        )
        .with_attribution_evidence(evidence.hash());
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: key,
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                state.encode_current_msgpack().expect("state").into(),
            )),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("signed metadata");
        assert_eq!(
            replica
                .receive_source_metadata(&signed, repository.store(), None, |_| Ok(()))
                .expect("metadata accepted"),
            Admission::Accepted
        );
        assert!(
            !replica
                .has_source_possession(state.id())
                .expect("no possession")
        );
        assert!(
            replica
                .receive_prepared_source(&signed, repository.store(), |_| Ok(()))
                .is_err(),
            "missing required evidence cannot claim full possession"
        );
        assert_eq!(
            replica
                .operation(&operation.id().expect("ID"))
                .expect("lookup")
                .expect("retained metadata")
                .1,
            Admission::Accepted
        );
        repository
            .store()
            .put_blob(&evidence)
            .expect("hydrate evidence");
        assert_eq!(
            replica
                .receive_prepared_source(&signed, repository.store(), |_| Ok(()))
                .expect("complete source"),
            Admission::Accepted
        );
        assert!(
            replica
                .has_source_possession(state.id())
                .expect("possession")
        );

        let mut pending = operation.clone();
        pending
            .parents
            .insert(ContentHash::compute(b"not yet received causal parent"));
        let pending = SignedOperation::sign(&pending, &signer).expect("pending metadata");
        assert_eq!(
            replica
                .receive_source_metadata(&pending, repository.store(), None, |_| Ok(()))
                .expect("pending source metadata"),
            Admission::Pending
        );
    }
}
