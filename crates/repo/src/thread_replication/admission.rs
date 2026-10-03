//! Original-author authority receipts accompany immutable operations atomically.
//! Bare executor pins cannot authorize a receiving mutation. Part 2 supplies
//! independently selected fresh witness evidence through the HYBRID seam.
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
    /// A bare receipt now fails closed. Use `receive_witnessed` with an
    /// independently selected fresh set and the complete original authority.
    pub fn receive_with_authority_admission(
        &self,
        original: &SignedOperation,
        receipt: &SignedAuthorityAdmission,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&objects::object::thread_replication::ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        self.receive_inner(
            original,
            store,
            authorize,
            false,
            Some(receipt),
            false,
            None,
        )
    }
    pub fn require_authority_admission(
        &self,
        original: &SignedOperation,
        receipt: &SignedAuthorityAdmission,
    ) -> Result<()> {
        receipt.verify(original, &self.authority_admission_trust(receipt)?)?;
        Ok(())
    }
    /// Reject evergreen executor trust for any typed admission subject.
    /// Incoming testimony never inserts or replaces a trust record.
    pub fn authority_admission_trust(
        &self,
        receipt: &SignedAuthorityAdmission,
    ) -> Result<TrustedHostedExecutor> {
        if receipt.verify_signature()?.basis
            != objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority
        {
            return Err(Error::BoundaryAcceptancePendingApi318);
        }
        Err(Error::WitnessEvidenceRequired)
    }
    /// One indexed statement returns original bytes, status and retained proof.
    pub fn operation_with_authority_admission(
        &self,
        id: &ContentHash,
    ) -> Result<Option<StoredOperation>> {
        let row = self.connect()?.query_row(
            "SELECT o.canonical,o.signature,o.status,o.reason,o.authority_receipt_canonical,o.authority_receipt_signature,b.canonical,b.signature FROM operations o LEFT JOIN boundary_admission_evidence e ON e.receipt=o.authority_receipt_canonical LEFT JOIN boundary_acceptances b ON b.id=e.acceptance WHERE o.id=?1 AND o.thread=?2",
            params![id.as_bytes(), self.thread.as_bytes()],
            |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,i32>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<Vec<u8>>>(4)?,row.get::<_,Option<Vec<u8>>>(5)?,row.get::<_,Option<Vec<u8>>>(6)?,row.get::<_,Option<Vec<u8>>>(7)?)),
        ).optional()?;
        row.map(
            |(canonical, signature, status, reason, receipt_canonical, receipt_signature, evidence_canonical, evidence_signature)| {
                let status = match status {
                    0 => Admission::Pending,
                    1 => Admission::Accepted,
                    2 => Admission::Rejected(reason.unwrap_or_default()),
                    _ => return Err(Error::Invalid("invalid operation admission status".into())),
                };
                let evidence=match(evidence_canonical,evidence_signature) {(None,None)=>None,(Some(a),Some(b))=>Some((a,b)),_=>return Err(Error::Invalid("incomplete retained boundary evidence".into()))};
                let authority_admission = match (receipt_canonical, receipt_signature) {
                    (None, None) => None,
                    (Some(canonical), Some(signature)) => Some(SignedAuthorityAdmission {
                        boundary_acceptance: super::boundary_evidence::matched(evidence, &objects::object::thread_authority_admission::ThreadAuthorityAdmission::decode(&canonical)?.basis)?,
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
