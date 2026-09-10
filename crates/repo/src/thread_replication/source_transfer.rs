//! One bounded recursive indexed ancestry query plus one distinct evidence lookup.
//! This avoids opening a database connection for every causal operation.
use crypto::thread_operation::SignedOperation;
use objects::object::ContentHash;
use rusqlite::params;

use super::{Error, Result, ThreadReplica};
impl ThreadReplica {
    pub fn source_ancestry(
        &self,
        selected: ContentHash,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<Vec<super::admission::StoredOperation>> {
        if max_records == 0
            || max_records > 10_000
            || max_bytes == 0
            || max_bytes > 16 * 1024 * 1024
        {
            return Err(Error::Invalid("source ancestry budget invalid".into()));
        }
        let connection = self.connect()?;
        let mut statement=connection.prepare("WITH RECURSIVE ancestry(id) AS (SELECT ?1 UNION SELECT operation FROM thread_owner_claim_frontier WHERE thread=?3 UNION SELECT p.parent FROM parents p JOIN ancestry a ON p.child=a.id LIMIT ?2) SELECT o.canonical,o.signature,o.status,o.thread,o.facet,o.authority_receipt_canonical,o.authority_receipt_signature,o.id,e.acceptance FROM ancestry a JOIN operations o ON o.id=a.id LEFT JOIN boundary_admission_evidence e ON e.receipt=o.authority_receipt_canonical")?;
        let mut rows = statement.query(params![
            selected.as_bytes(),
            (max_records + 1) as i64,
            self.thread.as_bytes()
        ])?;
        let mut output = Vec::new();
        let mut evidence_ids = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        let mut selected_present = false;
        while let Some(row) = rows.next()? {
            let row_id: Vec<u8> = row.get(7)?;
            selected_present |= row_id == selected.as_bytes();
            let canonical: Vec<u8> = row.get(0)?;
            let signature: Vec<u8> = row.get(1)?;
            let status: i64 = row.get(2)?;
            let thread: Vec<u8> = row.get(3)?;
            let facet: i64 = row.get(4)?;
            let receipt_canonical: Option<Vec<u8>> = row.get(5)?;
            let receipt_signature: Option<Vec<u8>> = row.get(6)?;
            let evidence_id: Option<Vec<u8>> = row.get(8)?;
            let receipt_bytes = receipt_canonical.as_ref().map_or(0, Vec::len)
                + receipt_signature.as_ref().map_or(0, Vec::len);
            let authority_admission = match (receipt_canonical, receipt_signature) {
                (None, None) => None,
                (Some(canonical), Some(signature)) => Some(
                    crypto::thread_authority_admission::SignedAuthorityAdmission {
                        boundary_acceptance: None,
                        canonical,
                        signature,
                    },
                ),
                _ => return Err(Error::Invalid("partial source authority receipt".into())),
            };
            if let Some(receipt) = &authority_admission {
                let basis = receipt.verify_signature()?.basis;
                match (basis,evidence_id) {
                    (objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,None)=>{},
                    (objects::object::original_boundary_acceptance::AdmissionBasis::BoundaryAcceptance {acceptance},Some(id)) if acceptance.as_bytes().as_slice()==id => {evidence_ids.insert(acceptance);},
                    _=>return Err(Error::Invalid("source receipt lacks exact boundary evidence reference".into())),
                }
                if evidence_ids.len() > 128 {
                    return Err(Error::Invalid("boundary evidence count exceeded".into()));
                }
            } else if evidence_id.is_some() {
                return Err(Error::Invalid(
                    "unreferenced source boundary evidence".into(),
                ));
            }
            bytes = bytes
                .checked_add(canonical.len() + signature.len() + receipt_bytes + 128)
                .ok_or_else(|| Error::Invalid("source ancestry byte overflow".into()))?;
            if output.len() >= max_records || bytes > max_bytes {
                return Err(Error::Invalid(
                    "source ancestry exceeds transfer budget".into(),
                ));
            }
            if status != 1 || thread != self.thread.as_bytes() || facet != 1 {
                return Err(Error::Invalid(
                    "source ancestry contains inadmissible dependency".into(),
                ));
            }
            output.push(super::admission::StoredOperation {
                original: SignedOperation {
                    canonical,
                    signature,
                },
                status: super::Admission::Accepted,
                authority_admission,
            });
        }
        if !selected_present {
            return Err(Error::Invalid(
                "selected source operation unavailable".into(),
            ));
        }
        drop(rows);
        drop(statement);
        let evidence = super::boundary_evidence::load_many(
            &connection,
            &evidence_ids,
            max_bytes.saturating_sub(bytes),
        )?;
        for stored in &mut output {
            if let Some(receipt) = &mut stored.authority_admission
                && let objects::object::original_boundary_acceptance::AdmissionBasis::BoundaryAcceptance {acceptance}=receipt.verify_signature()?.basis
            {
                receipt.boundary_acceptance=Some(evidence.get(&acceptance).ok_or_else(||Error::Invalid("missing matched source evidence".into()))?.clone());
            }
        }
        Ok(output)
    }
}
