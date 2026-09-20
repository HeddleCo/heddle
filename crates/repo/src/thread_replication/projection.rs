//! Bounded native Thread reads from one SQLite snapshot. Views reuse indexed
//! accepted heads; they do not materialize the complete operation history.
use std::collections::BTreeMap;

use crypto::thread_operation::SignedOperation;
use objects::object::{
    ContentHash, StateId,
    thread_replication::{
        ThreadGenesis, ThreadOperationBody,
        metadata::{Property, ThreadControl},
    },
};
use rusqlite::params;

use super::{Error, Result, ThreadReplica, hash};

pub struct Projection {
    pub generation: i64,
    pub genesis: ThreadGenesis,
    pub source_heads: Vec<StateId>,
    pub capture_count: u64,
    pub fields: BTreeMap<Property, Vec<(ContentHash, SignedOperation)>>,
}
/// Opaque endpoint observation version; field CAS uses portable parent versions.
pub fn version(thread: ContentHash, generation: i64) -> ContentHash {
    ContentHash::compute_typed(
        "heddle-device-thread-view-v2",
        &[thread.as_bytes().as_slice(), &generation.to_be_bytes()].concat(),
    )
}
impl ThreadReplica {
    pub fn projection(&self) -> Result<Projection> {
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let (generation, genesis): (i64, Vec<u8>) = tx.query_row(
            "SELECT generation,genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let genesis = ThreadGenesis::decode(&genesis)?;
        let (capture_count, source_heads) = super::source_index::summary_in(&tx, self.thread)?;
        let mut fields = BTreeMap::new();
        // Singleton overview fields have a fixed query and row bound. Review
        // decisions have their own bounded section page and never grow this view.
        let mut loaded_bytes = 0usize;
        for property in [
            Property::Name,
            Property::Intent,
            Property::Lifecycle,
            Property::Sharing,
            Property::Audience,
            Property::Retention,
        ] {
            let key = super::metadata::key(&property);
            let mut statement = tx.prepare("SELECT o.id,o.canonical,o.signature FROM thread_control_heads h JOIN operations o ON o.thread=h.thread AND o.id=h.operation WHERE h.thread=?1 AND h.property=?2 AND o.status=1 AND o.authority_admitted=1 ORDER BY o.id LIMIT 129")?;
            let rows = statement.query_map(params![self.thread.as_bytes(), key], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            let mut candidates = Vec::new();
            for row in rows {
                let (id, canonical, signature) = row?;
                loaded_bytes = loaded_bytes
                    .checked_add(canonical.len())
                    .and_then(|bytes| bytes.checked_add(signature.len()))
                    .ok_or_else(|| Error::Invalid("Thread projection size overflow".into()))?;
                if candidates.len() == 128 || loaded_bytes > 4 * 1024 * 1024 {
                    return Err(Error::Invalid(
                        "Thread property frontier exceeds view budget".into(),
                    ));
                }
                let signed = SignedOperation {
                    canonical,
                    signature,
                };
                let operation = signed.verify()?;
                let ThreadOperationBody::Metadata(bytes) = &operation.body else {
                    return Err(Error::Invalid(
                        "Thread property index names another facet".into(),
                    ));
                };
                let control = ThreadControl::decode(bytes)?;
                if operation.thread != self.thread
                    || control.property() != property
                    || control.spool.to_string() != genesis.spool
                    || operation.id()?.as_bytes().as_slice() != id
                {
                    return Err(Error::Invalid(
                        "Thread property index integrity failure".into(),
                    ));
                }
                candidates.push((hash(&id)?, signed));
            }
            fields.insert(property, candidates);
        }
        tx.commit()?;
        Ok(Projection {
            generation,
            genesis,
            source_heads,
            capture_count,
            fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
    use rusqlite::StatementStatus;

    use super::*;
    #[test]
    fn source_page_work_is_bounded_by_page_after_ten_thousand_records() {
        let directory = tempfile::tempdir().expect("replica");
        let signer = Ed25519Signer::from_seed(&[32; 32]).expect("signer");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(1).to_string(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "paging".into(),
            intent: "bounded SQL work".into(),
            creator: signer.public_key().try_into().expect("key"),
            owner: objects::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("key"),
            ),
            nonce: vec![1],
        };
        let replica = ThreadReplica::create(
            directory.path(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("replica");
        let mut connection = replica.connect().expect("SQL");
        // These are SQL cardinality fixtures: operation payloads are deliberately
        // opaque because the measured production statement only pages records.
        let seed = |connection: &mut rusqlite::Connection, start: u64, end: u64| {
            let tx = connection.transaction().expect("transaction");
            for value in start..end {
                let mut id = [0u8; 32];
                id[24..].copy_from_slice(&value.to_be_bytes());
                tx.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),1,zeroblob(32))",params![id,replica.thread_id().as_bytes()]).expect("source cardinality");
                id[0] = 1;
                tx.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status) VALUES(?1,?2,2,x'00',zeroblob(64),1)",params![id,replica.thread_id().as_bytes()]).expect("other facet cardinality");
            }
            tx.commit().expect("commit");
        };
        let measured = |connection: &rusqlite::Connection, after: Option<Vec<u8>>| {
            let mut statement = connection
                .prepare(super::super::ACCEPTED_PAGE_SQL)
                .expect("actual production query");
            let rows = statement
                .query_map(
                    params![replica.thread_id().as_bytes(), 1, after, 16],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .expect("page")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("rows");
            assert_eq!(rows.len(), 16);
            let work = statement.get_status(StatementStatus::VmStep);
            assert!(work < 400, "a 16-row page performed {work} VM instructions");
            work
        };
        seed(&mut connection, 0, 32);
        let baseline = measured(&connection, None);
        seed(&mut connection, 32, 10_032);
        let first = measured(&connection, None);
        let mut cursor = vec![0; 32];
        cursor[24..].copy_from_slice(&9_999u64.to_be_bytes());
        let late = measured(&connection, Some(cursor));
        println!("source page VM steps: small={baseline}, large-first={first}, large-deep={late}");
        assert!(
            first <= baseline + 16 && late <= baseline + 32,
            "page work grew with history: {baseline}/{first}/{late}"
        );
    }
}
