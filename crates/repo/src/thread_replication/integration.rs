//! Persisted receiver-owned hosted executor pins. Incoming records cannot add a
//! pin, and an immutable Spool genesis cannot be replaced under the same UUID.
use objects::object::{
    ContentHash,
    thread_replication::{ThreadOperation, integration::TrustedHostedExecutor},
};
use rusqlite::{OptionalExtension, params};

use super::{Error, Result, ThreadReplica};

/// Row shape for a local-integration source lookup: canonical bytes, signature,
/// status, and the source Thread's genesis bytes.
type LocalIntegrationSourceRow = (Vec<u8>, Vec<u8>, i32, Vec<u8>);

impl ThreadReplica {
    /// Called only by Repository's verified owner-genesis configuration seam.
    pub(crate) fn pin_hosted_executor(&self, trust: &TrustedHostedExecutor) -> Result<()> {
        if self.genesis()?.spool != trust.spool.to_string() {
            return Err(Error::Invalid(
                "executor pin belongs to another Thread Spool".into(),
            ));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: Option<Vec<u8>> = tx
            .query_row(
                "SELECT genesis FROM hosted_executor_pins WHERE spool=?1 LIMIT 1",
                [trust.spool.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if previous
            .as_deref()
            .is_some_and(|genesis| genesis != trust.spool_genesis.as_bytes())
        {
            return Err(Error::Invalid(
                "hosted executor pin cannot replace immutable Spool genesis".into(),
            ));
        }
        tx.execute(
            "INSERT OR IGNORE INTO hosted_executor_pins(spool,genesis,executor) VALUES(?1,?2,?3)",
            params![
                trust.spool.to_string(),
                trust.spool_genesis.as_bytes(),
                trust.executor
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Validate source executor pins before installing an incoming artifact.
    /// A capture or local integration has no hosted executor and is a no-op here;
    /// its original author must be checked separately.
    pub fn verify_source_executor(&self, operation: &ThreadOperation) -> Result<()> {
        self.require_trusted_integration(operation)
    }
    pub(super) fn require_trusted_integration(&self, operation: &ThreadOperation) -> Result<()> {
        let Some(receipt) = operation.hosted_execution_binding()? else {
            return Ok(());
        };
        let genesis: Option<Vec<u8>> = self
            .connect()?
            .query_row(
                "SELECT genesis FROM hosted_executor_pins WHERE spool=?1 AND executor=?2",
                params![receipt.spool.to_string(), receipt.executor],
                |row| row.get(0),
            )
            .optional()?;
        let genesis = genesis.ok_or_else(|| {
            Error::Invalid("hosted integration requires independently pinned executor trust".into())
        })?;
        let trust = TrustedHostedExecutor {
            spool: receipt.spool,
            spool_genesis: ContentHash::from_bytes(
                genesis
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invalid("invalid executor genesis pin".into()))?,
            ),
            executor: receipt.executor,
        };
        trust.authorize(operation)?;
        Ok(())
    }
}

impl ThreadReplica {
    pub(super) fn require_local_integration_source(
        &self,
        operation: &ThreadOperation,
    ) -> Result<()> {
        self.require_local_integration_source_in(&self.connect()?, operation)
    }
    pub(super) fn require_local_integration_source_in(
        &self,
        connection: &rusqlite::Connection,
        operation: &ThreadOperation,
    ) -> Result<()> {
        let Some(receipt) = operation.local_integration()? else {
            return Ok(());
        };
        let source: Option<LocalIntegrationSourceRow> = connection.query_row("SELECT o.canonical,o.signature,o.status,t.genesis FROM operations o JOIN threads t ON t.id=o.thread WHERE o.thread=?1 AND o.id=?2",params![receipt.source_thread.as_bytes(),receipt.source_operation.as_bytes()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        let (canonical, signature, status, genesis) = source.ok_or_else(|| {
            Error::Invalid("local integration requires original source Thread operation".into())
        })?;
        let source_genesis = objects::object::thread_replication::ThreadGenesis::decode(&genesis)?;
        let target: Vec<u8> = connection.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |row| row.get(0),
        )?;
        let target_genesis = objects::object::thread_replication::ThreadGenesis::decode(&target)?;
        if source_genesis.spool != receipt.spool.to_string()
            || target_genesis.spool != receipt.spool.to_string()
        {
            return Err(Error::Invalid("local integration crosses Spools".into()));
        }
        if status != 1 {
            return Err(Error::Invalid(
                "local integration source is not admitted".into(),
            ));
        }
        let original = crypto::thread_operation::SignedOperation {
            canonical,
            signature,
        };
        receipt.validate_source(&original.verify()?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crypto::{
        Ed25519Signer, Signer,
        thread_operation::{SignedGenesis, SignedOperation},
    };
    use objects::object::{
        Attribution, Principal, State, Tree,
        thread_replication::{
            Admission, GenesisOwner, ThreadGenesis, ThreadOperationBody,
            hosted_import::{HostedImport, ImportedCommit},
        },
    };

    use super::*;

    #[test]
    fn hosted_import_requires_persistent_independent_executor_pin_without_checkout_writes() {
        let directory = tempfile::TempDir::new().expect("repository directory");
        let repo = crate::Repository::init_default(directory.path()).expect("repository");
        let creator = Ed25519Signer::from_seed(&[41; 32]).expect("creator");
        let executor = Ed25519Signer::from_seed(&[42; 32]).expect("executor");
        let key = creator.public_key().try_into().expect("creator key");
        let base = repo.head().expect("HEAD").expect("initial state");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(43).to_string(),
            parent: None,
            base,
            name: "import".into(),
            intent: "retain Git attribution".into(),
            owner: GenesisOwner::LocalKey(key),
            creator: key,
            nonce: vec![],
        };
        let replica = ThreadReplica::create(
            repo.heddle_dir(),
            &SignedGenesis::sign(&genesis, &creator).expect("original genesis"),
        )
        .expect("replica");
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![base],
            Attribution::human(Principal::new("Git author", "git@example.test")),
        );
        let trust = TrustedHostedExecutor {
            spool: uuid::Uuid::from_u128(43),
            spool_genesis: ContentHash::from_bytes([44; 32]),
            executor: executor.public_key().try_into().expect("executor key"),
        };
        let receipt = HostedImport {
            version: 1,
            spool: trust.spool,
            spool_genesis: trust.spool_genesis,
            executor: trust.executor,
            target_thread: replica.thread_id(),
            expected_target_frontier: BTreeSet::new(),
            result: state.encode_current_msgpack().expect("state").into(),
            provider: "github".into(),
            provider_repository_id: "123".into(),
            source_commit: ImportedCommit::Sha1([45; 20]),
            initiating_request_proof: ContentHash::from_bytes([46; 32]),
            executed_at_ms: 100,
        };
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: BTreeSet::new(),
            publisher: trust.executor,
            body: ThreadOperationBody::HostedImport(receipt.encode().expect("receipt")),
        };
        let signed = SignedOperation::sign(&operation, &executor).expect("attestation");
        let error = replica
            .receive(&signed, repo.store(), |_| Ok(()))
            .expect_err("incoming receipt is not trust");
        assert!(
            error
                .to_string()
                .contains("independently pinned executor trust"),
            "{error}"
        );
        assert!(
            replica
                .operation(&operation.id().expect("ID"))
                .expect("lookup")
                .is_none()
        );
        replica
            .pin_hosted_executor(&trust)
            .expect("receiver configuration");
        assert_eq!(
            replica
                .receive(&signed, repo.store(), |_| Ok(()))
                .expect("trusted import"),
            Admission::Accepted
        );
        let reopened = ThreadReplica::open(repo.heddle_dir(), replica.thread_id()).expect("reopen");
        assert_eq!(
            reopened
                .receive(&signed, repo.store(), |_| Ok(()))
                .expect("replay"),
            Admission::Accepted
        );
        assert_eq!(
            reopened
                .accepted_source_revision(state.id())
                .expect("source membership")
                .expect("retained capture")
                .id(),
            state.id()
        );
        assert_eq!(repo.head().expect("unchanged checkout"), Some(base));
    }
}
