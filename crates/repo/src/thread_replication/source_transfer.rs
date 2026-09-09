//! One bounded recursive indexed query per requested immutable source ancestry.
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
        let mut statement=connection.prepare("WITH RECURSIVE ancestry(id) AS (SELECT ?1 UNION SELECT operation FROM thread_owner_claim_frontier WHERE thread=?3 UNION SELECT p.parent FROM parents p JOIN ancestry a ON p.child=a.id LIMIT ?2) SELECT o.canonical,o.signature,o.status,o.thread,o.facet,o.authority_receipt_canonical,o.authority_receipt_signature,o.id FROM ancestry a JOIN operations o ON o.id=a.id")?;
        let mut rows = statement.query(params![selected.as_bytes(), (max_records + 1) as i64, self.thread.as_bytes()])?;
        let mut output = Vec::new();
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
            let receipt_bytes = receipt_canonical.as_ref().map_or(0, Vec::len) + receipt_signature.as_ref().map_or(0, Vec::len);
            let authority_admission = match (receipt_canonical, receipt_signature) {
                (None, None) => None,
                (Some(canonical), Some(signature)) => Some(crypto::thread_authority_admission::SignedAuthorityAdmission { canonical, signature }),
                _ => return Err(Error::Invalid("partial source authority receipt".into())),
            };
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
            output.push(super::admission::StoredOperation { original: SignedOperation { canonical, signature }, status: super::Admission::Accepted, authority_admission });
        }
        if !selected_present {
            return Err(Error::Invalid(
                "selected source operation unavailable".into(),
            ));
        }
        Ok(output)
    }
}
