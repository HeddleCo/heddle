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
    ) -> Result<Vec<SignedOperation>> {
        if max_records == 0
            || max_records > 10_000
            || max_bytes == 0
            || max_bytes > 16 * 1024 * 1024
        {
            return Err(Error::Invalid("source ancestry budget invalid".into()));
        }
        let connection = self.connect()?;
        let mut statement=connection.prepare("WITH RECURSIVE ancestry(id) AS (SELECT ?1 UNION SELECT p.parent FROM parents p JOIN ancestry a ON p.child=a.id LIMIT ?2) SELECT o.canonical,o.signature,o.status,o.thread,o.facet FROM ancestry a JOIN operations o ON o.id=a.id")?;
        let mut rows = statement.query(params![selected.as_bytes(), (max_records + 1) as i64])?;
        let mut output = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let canonical: Vec<u8> = row.get(0)?;
            let signature: Vec<u8> = row.get(1)?;
            let status: i64 = row.get(2)?;
            let thread: Vec<u8> = row.get(3)?;
            let facet: i64 = row.get(4)?;
            bytes = bytes
                .checked_add(canonical.len() + signature.len() + 128)
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
            output.push(SignedOperation {
                canonical,
                signature,
            });
        }
        if output.is_empty() {
            return Err(Error::Invalid(
                "selected source operation unavailable".into(),
            ));
        }
        Ok(output)
    }
}
