//! Durable source summary indexes, maintained exactly once at causal acceptance.
//! Reading a Thread overview never counts or walks its operation history.
use objects::object::{ContentHash, StateId};
use rusqlite::Transaction;

use super::{Error, Result, hash};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_source_counts(
 thread BLOB PRIMARY KEY, count INTEGER NOT NULL CHECK(count>=0));
CREATE TABLE IF NOT EXISTS thread_source_heads(
 thread BLOB NOT NULL, operation BLOB NOT NULL, revision BLOB NOT NULL,
 PRIMARY KEY(thread,operation));
CREATE TRIGGER IF NOT EXISTS thread_source_admitted
AFTER UPDATE OF status ON operations
WHEN OLD.status<>1 AND NEW.status=1 AND NEW.facet=1
BEGIN
 INSERT INTO thread_source_counts(thread,count) VALUES(NEW.thread,1)
 ON CONFLICT(thread) DO UPDATE SET count=count+1;
 DELETE FROM thread_source_heads WHERE thread=NEW.thread AND operation IN
 (SELECT parent FROM parents WHERE child=NEW.id);
 INSERT INTO thread_source_heads(thread,operation,revision)
 VALUES(NEW.thread,NEW.id,NEW.source_revision);
END;
CREATE TRIGGER IF NOT EXISTS thread_source_inserted
AFTER INSERT ON operations WHEN NEW.status=1 AND NEW.facet=1
BEGIN
 INSERT INTO thread_source_counts(thread,count) VALUES(NEW.thread,1)
 ON CONFLICT(thread) DO UPDATE SET count=count+1;
 DELETE FROM thread_source_heads WHERE thread=NEW.thread AND operation IN
 (SELECT parent FROM parents WHERE child=NEW.id);
 INSERT INTO thread_source_heads(thread,operation,revision)
 VALUES(NEW.thread,NEW.id,NEW.source_revision);
END;";

pub(super) fn summary_in(tx: &Transaction<'_>, thread: ContentHash) -> Result<(u64, Vec<StateId>)> {
    let count: i64 = tx.query_row(
        "SELECT COALESCE((SELECT count FROM thread_source_counts WHERE thread=?1),0)",
        [thread.as_bytes()],
        |row| row.get(0),
    )?;
    let count =
        u64::try_from(count).map_err(|_| Error::Invalid("invalid accepted source count".into()))?;
    let mut statement = tx.prepare(
        "SELECT revision FROM thread_source_heads WHERE thread=?1 ORDER BY operation LIMIT 129",
    )?;
    let heads = statement
        .query_map([thread.as_bytes()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| Ok(StateId::from_bytes(*hash(&row?)?.as_bytes())))
        .collect::<Result<Vec<_>>>()?;
    if heads.len() > 128 {
        return Err(Error::Invalid(
            "Thread source frontier exceeds view budget".into(),
        ));
    }
    Ok((count, heads))
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
        thread_replication::{Admission, ThreadGenesis, ThreadOperation, ThreadOperationBody},
    };

    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        crate::Repository,
        super::super::ThreadReplica,
        ThreadGenesis,
        Ed25519Signer,
    ) {
        let dir = tempfile::tempdir().expect("native workspace");
        let repository = crate::Repository::init_default(dir.path()).expect("repository");
        let signer = Ed25519Signer::from_seed(&[201; 32]).expect("publisher");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::new_v4().to_string(),
            parent: None,
            base: repository.head().expect("HEAD").expect("initial source"),
            name: "indexed".into(),
            intent: "bounded summary".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![1; 16],
        };
        let replica = super::super::ThreadReplica::create(
            repository.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("replica");
        (dir, repository, replica, genesis, signer)
    }
    fn capture(
        genesis: &ThreadGenesis,
        signer: &Ed25519Signer,
        name: &str,
        parent: Option<(&SignedOperation, StateId)>,
    ) -> (SignedOperation, StateId) {
        let (parents, revisions) = match parent {
            Some((operation, revision)) => (
                BTreeSet::from([operation
                    .verify()
                    .expect("signed parent")
                    .id()
                    .expect("parent ID")]),
                vec![revision],
            ),
            None => (BTreeSet::new(), vec![genesis.base]),
        };
        let state = State::new_snapshot(
            Tree::new().hash(),
            revisions,
            Attribution::human(Principal::new(name, "agent@example.test")),
        );
        let operation = SignedOperation::sign(
            &ThreadOperation {
                version: 1,
                thread: genesis.id().expect("Thread"),
                parents,
                publisher: signer.public_key().try_into().expect("publisher"),
                body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("source")),
            },
            signer,
        )
        .expect("original signature");
        (operation, state.id())
    }
    #[test]
    fn source_index_tracks_causal_acceptance_once_and_preserves_rollback() {
        let (_dir, repository, replica, genesis, signer) = fixture();
        let (first, first_revision) = capture(&genesis, &signer, "first", None);
        let (second, second_revision) =
            capture(&genesis, &signer, "second", Some((&first, first_revision)));
        assert!(matches!(
            replica
                .receive(&second, repository.store(), |_| Ok(()))
                .expect("withheld child"),
            Admission::Pending
        ));
        assert_eq!(
            replica
                .projection()
                .expect("empty accepted summary")
                .capture_count,
            0
        );
        assert!(matches!(
            replica
                .receive(&first, repository.store(), |_| Ok(()))
                .expect("causal closure"),
            Admission::Accepted
        ));
        let projection = replica.projection().expect("indexed closure");
        assert_eq!(projection.capture_count, 2);
        assert_eq!(projection.source_heads, vec![second_revision]);
        replica
            .receive(&first, repository.store(), |_| Ok(()))
            .expect("exact replay");
        assert_eq!(
            replica
                .projection()
                .expect("deduplicated summary")
                .capture_count,
            2
        );
        let (parallel, parallel_revision) = capture(&genesis, &signer, "parallel", None);
        replica
            .receive(&parallel, repository.store(), |_| Ok(()))
            .expect("concurrent source");
        let projection = replica.projection().expect("concurrent summary");
        assert_eq!(projection.capture_count, 3);
        assert_eq!(
            projection.source_heads.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([second_revision, parallel_revision])
        );
        // A transaction failure cannot expose an index commit without its source.
        let (rollback, _) = capture(&genesis, &signer, "rollback", None);
        let operation = rollback.verify().expect("signature");
        let mut connection = replica.connect().expect("same durable database");
        let tx = connection.transaction().expect("transaction");
        tx.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,?3,?4,0,?5)",rusqlite::params![operation.id().expect("ID").as_bytes(),replica.thread_id().as_bytes(),rollback.canonical,rollback.signature,operation.source_state().expect("source").expect("capture").id().as_bytes()]).expect("pending fixture");
        tx.execute(
            "UPDATE operations SET status=1 WHERE id=?1",
            [operation.id().expect("ID").as_bytes()],
        )
        .expect("indexed transaction");
        assert_eq!(
            summary_in(&tx, replica.thread_id()).expect("own writes").0,
            4
        );
        tx.rollback()
            .expect("failed operation rolls back all summaries");
        assert_eq!(
            replica.projection().expect("durable summary").capture_count,
            3
        );
        assert!(
            replica
                .operation(&operation.id().expect("ID"))
                .expect("rollback lookup")
                .is_none()
        );
    }
    #[test]
    fn source_index_frontier_bound_rejects_large_concurrency_without_history_walk() {
        let (_dir, repository, replica, genesis, signer) = fixture();
        for i in 0..129 {
            let (record, _) = capture(&genesis, &signer, &format!("branch-{i}"), None);
            replica
                .receive(&record, repository.store(), |_| Ok(()))
                .expect("valid concurrent capture");
        }
        assert!(
            replica
                .projection()
                .err()
                .expect("bounded frontier")
                .to_string()
                .contains("source frontier exceeds")
        );
    }
}
