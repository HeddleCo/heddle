//! Source metadata and locally available source are distinct. Only a trusted
//! local producer or a fully validated incoming pack may attest possession.
use std::collections::BTreeSet;

use crypto::thread_operation::SignedOperation;
use objects::{
    object::{ContentHash, StateId, TreeEntryTarget, thread_replication::ThreadOperation},
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
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let admission = self.receive_in(
            &tx,
            signed,
            &operation,
            store,
            operation.local_integration()?.is_some(),
            None,
        )?;
        if admission != Admission::Accepted {
            return Err(Error::Invalid("prepared source did not settle".into()));
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
        let mut trees = vec![state.tree];
        let mut seen = BTreeSet::<ContentHash>::new();
        let mut bytes = 0u64;
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
