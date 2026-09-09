//! Persisted receiver-owned hosted executor pins. Incoming records cannot add a
//! pin, and an immutable Spool genesis cannot be replaced under the same UUID.
use objects::object::{
    ContentHash,
    thread_replication::{ThreadOperation, integration::TrustedHostedExecutor},
};
use rusqlite::{OptionalExtension, params};

use super::{Error, Result, ThreadReplica};

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
    pub(super) fn require_trusted_integration(&self, operation: &ThreadOperation) -> Result<()> {
        let Some(receipt) = operation.integration()? else {
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
        let Some(receipt) = operation.local_integration()? else {
            return Ok(());
        };
        let directory = self
            .path
            .parent()
            .ok_or_else(|| Error::Invalid("replica directory missing".into()))?;
        let source = ThreadReplica::open(directory, receipt.source_thread)?;
        if source.genesis()?.spool != receipt.spool.to_string()
            || self.genesis()?.spool != receipt.spool.to_string()
        {
            return Err(Error::Invalid("local integration crosses Spools".into()));
        }
        let (original, admission) =
            source
                .operation(&receipt.source_operation)?
                .ok_or_else(|| {
                    Error::Invalid(
                        "local integration requires original source Thread operation".into(),
                    )
                })?;
        if admission != super::Admission::Accepted {
            return Err(Error::Invalid(
                "local integration source is not admitted".into(),
            ));
        }
        receipt.validate_source(&original.verify()?)?;
        Ok(())
    }
}
