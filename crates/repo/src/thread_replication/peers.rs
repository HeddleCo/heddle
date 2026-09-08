// SPDX-License-Identifier: Apache-2.0
//! Peer claims are durable coordination state, never local admission authority.
use super::*;

impl ThreadReplica {
    pub fn remember_peer_heads(
        &self,
        peer: [u8; 32],
        heads: &[(ThreadFacet, ContentHash)],
    ) -> Result<()> {
        if heads.len() > 1024 {
            return Err(Error::Invalid("peer frontier page exceeds budget".into()));
        }
        if heads.is_empty() {
            return Ok(());
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (facet, id) in heads {
            tx.execute(
                "INSERT OR IGNORE INTO peer_heads(thread,peer,operation,facet) VALUES(?1,?2,?3,?4)",
                params![
                    self.thread.as_bytes(),
                    peer,
                    id.as_bytes(),
                    facet_number(*facet)
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Refilling the request window from durable peer heads avoids losing a
    /// wide graph when one missing-ancestor page is exhausted or a stream stops.
    pub fn needed_from_peer(
        &self,
        peer: [u8; 32],
        facets: &BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<ContentHash>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid(
                "dependency page size must be 1..1024".into(),
            ));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT h.operation,o.status FROM peer_heads h LEFT JOIN operations o ON o.id=h.operation AND o.thread=h.thread WHERE h.thread=?1 AND h.peer=?2 AND (o.status IS NULL OR o.status=0) AND ((h.facet=1 AND ?3) OR (h.facet=2 AND ?4)) ORDER BY h.operation LIMIT ?5")?;
        let heads = query
            .query_map(
                params![
                    self.thread.as_bytes(),
                    peer,
                    facets.contains(&ThreadFacet::Source),
                    facets.contains(&ThreadFacet::Discussion),
                    limit as u32
                ],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<i32>>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut needed = BTreeSet::new();
        for (bytes, status) in heads {
            let id = hash(&bytes)?;
            if status.is_none() {
                needed.insert(id);
            } else {
                needed.extend(self.missing_ancestors(id, limit - needed.len())?);
            }
            if needed.len() >= limit {
                break;
            }
        }
        Ok(needed.into_iter().collect())
    }

    /// A pending descendant may become accepted when a later parent arrives.
    /// Return those acknowledgments separately from the incoming parent's own
    /// receipt. A lost acknowledgment is repaired by the next Have exchange.
    pub fn settled_peer_heads(
        &self,
        peer: [u8; 32],
        facets: &BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, Admission)>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("receipt page size must be 1..1024".into()));
        }
        let mut connection = self.connect()?;
        let rows = connection.prepare("SELECT h.operation,o.status,o.reason FROM peer_heads h JOIN operations o ON o.id=h.operation AND o.thread=h.thread WHERE h.thread=?1 AND h.peer=?2 AND o.status<>0 AND ((h.facet=1 AND ?3) OR (h.facet=2 AND ?4)) ORDER BY h.operation LIMIT ?5")?
            .query_map(params![self.thread.as_bytes(), peer, facets.contains(&ThreadFacet::Source), facets.contains(&ThreadFacet::Discussion), limit as u32], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i32>(1)?, r.get::<_, Option<String>>(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut result = Vec::new();
        for (id, status, reason) in rows {
            tx.execute(
                "DELETE FROM peer_heads WHERE thread=?1 AND peer=?2 AND operation=?3",
                params![self.thread.as_bytes(), peer, id],
            )?;
            result.push((
                hash(&id)?,
                if status == 1 {
                    Admission::Accepted
                } else {
                    Admission::Rejected(reason.unwrap_or_default())
                },
            ));
        }
        tx.commit()?;
        Ok(result)
    }

    /// Store an authenticated destination's assertion. A later pending or
    /// rejected receipt cannot erase an earlier durable acceptance assertion.
    pub fn record_peer_receipt(
        &self,
        peer: [u8; 32],
        id: ContentHash,
        admission: &Admission,
    ) -> Result<()> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let locally_accepted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND thread=?2 AND status=1)",
            params![id.as_bytes(), self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        if !locally_accepted {
            return Err(Error::Invalid(
                "peer receipt names an unaccepted local operation".into(),
            ));
        }
        let (status, reason) = match admission {
            Admission::Accepted => (1, None),
            Admission::Pending => (0, None),
            Admission::Rejected(reason) => (2, Some(reason.as_str())),
        };
        tx.execute("INSERT INTO peer_receipts(thread,peer,operation,status,reason) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(thread,peer,operation) DO UPDATE SET status=excluded.status,reason=excluded.reason WHERE peer_receipts.status<>1 AND (peer_receipts.status=0 OR excluded.status=1)", params![self.thread.as_bytes(), peer, id.as_bytes(), status, reason])?;
        tx.commit()?;
        Ok(())
    }

    pub fn peer_receipt(&self, peer: [u8; 32], id: ContentHash) -> Result<Option<Admission>> {
        let row = self.connect()?.query_row("SELECT status,reason FROM peer_receipts WHERE thread=?1 AND peer=?2 AND operation=?3", params![self.thread.as_bytes(), peer, id.as_bytes()], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, Option<String>>(1)?))).optional()?;
        Ok(row.map(|(status, reason)| match status {
            1 => Admission::Accepted,
            2 => Admission::Rejected(reason.unwrap_or_default()),
            _ => Admission::Pending,
        }))
    }
}
