//! Original-author authority receipts accompany immutable operations atomically.
//! Executor pins come from receiver-owned setup, never from incoming records.
use crypto::{
    thread_authority_admission::SignedAuthorityAdmission, thread_operation::SignedOperation,
};
use objects::{
    object::{ContentHash, thread_replication::integration::TrustedHostedExecutor},
    store::ObjectStore,
};
use rusqlite::{OptionalExtension, params};

use super::{Admission, Error, Result, ThreadReplica};

#[derive(Clone, Debug)]
pub struct StoredOperation {
    pub original: SignedOperation,
    pub status: Admission,
    pub authority_admission: Option<SignedAuthorityAdmission>,
}
impl ThreadReplica {
    /// Current delivery authorization remains the endpoint's separate gate.
    /// The receipt establishes original authority using an independently pinned
    /// executor. Its exact bytes persist in the operation's admission transaction.
    pub fn receive_with_authority_admission(
        &self,
        original: &SignedOperation,
        receipt: &SignedAuthorityAdmission,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&objects::object::thread_replication::ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        self.receive_inner(original, store, authorize, false, Some(receipt))
    }
    pub(super) fn require_authority_admission(
        &self,
        original: &SignedOperation,
        receipt: &SignedAuthorityAdmission,
    ) -> Result<()> {
        let statement = receipt.verify_signature()?;
        if self.genesis()?.spool != statement.spool.to_string() {
            return Err(Error::Invalid(
                "authority receipt crosses local Spool".into(),
            ));
        }
        let genesis: Option<Vec<u8>> = self
            .connect()?
            .query_row(
                "SELECT genesis FROM hosted_executor_pins WHERE spool=?1 AND executor=?2",
                params![statement.spool.to_string(), statement.executor],
                |row| row.get(0),
            )
            .optional()?;
        let genesis = genesis.ok_or_else(|| {
            Error::Invalid(
                "authority admission requires independently pinned executor trust".into(),
            )
        })?;
        let trust = TrustedHostedExecutor {
            spool: statement.spool,
            spool_genesis: super::hash(&genesis)?,
            executor: statement.executor,
        };
        receipt.verify(original, &trust)?;
        Ok(())
    }
    /// One indexed statement returns original bytes, status and retained proof.
    pub fn operation_with_authority_admission(
        &self,
        id: &ContentHash,
    ) -> Result<Option<StoredOperation>> {
        let row = self.connect()?.query_row(
            "SELECT canonical,signature,status,reason,authority_receipt_canonical,authority_receipt_signature FROM operations WHERE id=?1 AND thread=?2",
            params![id.as_bytes(), self.thread.as_bytes()],
            |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,i32>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<Vec<u8>>>(4)?,row.get::<_,Option<Vec<u8>>>(5)?)),
        ).optional()?;
        row.map(
            |(canonical, signature, status, reason, receipt_canonical, receipt_signature)| {
                let status = match status {
                    0 => Admission::Pending,
                    1 => Admission::Accepted,
                    2 => Admission::Rejected(reason.unwrap_or_default()),
                    _ => return Err(Error::Invalid("invalid operation admission status".into())),
                };
                let authority_admission = match (receipt_canonical, receipt_signature) {
                    (None, None) => None,
                    (Some(canonical), Some(signature)) => Some(SignedAuthorityAdmission {
                        canonical,
                        signature,
                    }),
                    _ => {
                        return Err(Error::Invalid(
                            "incomplete retained authority admission".into(),
                        ));
                    }
                };
                Ok(StoredOperation {
                    original: SignedOperation {
                        canonical,
                        signature,
                    },
                    status,
                    authority_admission,
                })
            },
        )
        .transpose()
    }
}
