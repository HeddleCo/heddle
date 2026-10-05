//! Hosted execution requires fresh receiver-selected witness evidence.
//! Bare executor keys and stored endpoint pins cannot authorize installation.
use objects::object::thread_replication::{ThreadOperation, integration::TrustedHostedExecutor};
use rusqlite::{OptionalExtension, params};

use super::{Error, Result, ThreadReplica};

/// Row shape for a local-integration source lookup: canonical bytes, signature,
/// status, and the source Thread's genesis bytes.
type LocalIntegrationSourceRow = (Vec<u8>, Vec<u8>, i32, Vec<u8>);

impl ThreadReplica {
    /// Called only by Repository's verified owner-genesis configuration seam.
    pub(crate) fn pin_hosted_executor(&self, _trust: &TrustedHostedExecutor) -> Result<()> {
        Err(Error::WitnessEvidenceRequired)
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
        let _ = receipt;
        Err(Error::WitnessEvidenceRequired)
    }
}

impl ThreadReplica {
    pub(super) fn require_local_integration_source(
        &self,
        operation: &ThreadOperation,
    ) -> Result<()> {
        self.require_local_integration_source_in(&*self.connect()?, operation)
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
