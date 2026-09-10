//! One validated source publication commits original operations, availability,
//! and its caller-scoped receipt together. Pack validation/install happens first
//! on a bounded disk worker; no transaction spans transport or object copying.
use crypto::thread_operation::SignedOperation;
use objects::{
    object::{OperationId, StateId, thread_replication::ThreadOperation},
    store::ObjectStore,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{Admission, Error, Result, ThreadReplica};

pub struct Command<'a> {
    pub namespace: &'a str,
    pub id: OperationId,
    pub method: &'a str,
    pub request_hash: [u8; 32],
}

/// The already-validated source-publication payload: the original operations,
/// their independently-verified authority admissions, the state revision, and
/// the per-dependency Thread guards. Bundled so the commit signature stays under
/// the argument threshold; behaviour (`store`/`authorize`/`response`) stays out.
pub struct PreparedPublication<'a> {
    pub operations: &'a [SignedOperation],
    pub authority_admissions: &'a std::collections::BTreeMap<
        objects::object::ContentHash,
        crypto::thread_authority_admission::SignedAuthorityAdmission,
    >,
    pub revision: StateId,
    pub guards: &'a [(ThreadReplica, i64)],
}

impl ThreadReplica {
    /// `operations` must already have passed the shared standalone pack/causal
    /// closure validator. `authorize` validates original authors independently
    /// of the current courier. Every dependency Thread was admitted earlier.
    pub fn publish_prepared_source(
        &self,
        prepared: PreparedPublication<'_>,
        store: &impl ObjectStore,
        command: Command<'_>,
        authorize: impl Fn(&ThreadReplica, &SignedOperation) -> Result<()>,
        response: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let PreparedPublication {
            operations,
            authority_admissions,
            revision,
            guards,
        } = prepared;
        if command.namespace.is_empty()
            || command.namespace.len() > 1024
            || command.namespace.contains('\0')
            || operations.is_empty()
            || operations.len() > 10_000
            || guards.is_empty()
            || guards.len() > 128
        {
            return Err(Error::Invalid(
                "bounded authenticated publication required".into(),
            ));
        }
        let mut prepared = Vec::<(&ThreadReplica, &SignedOperation, ThreadOperation)>::new();
        for signed in operations {
            let operation = signed.verify()?;
            if operation.source_state()?.is_none() {
                return Err(Error::Invalid(
                    "publication accepts source originals only".into(),
                ));
            }
            let replica = guards
                .iter()
                .find(|(replica, _)| replica.thread == operation.thread)
                .map(|(replica, _)| replica)
                .ok_or_else(|| {
                    Error::Invalid("independently authorized source dependency absent".into())
                })?;
            if replica.path != self.path {
                return Err(Error::Invalid(
                    "source publication crosses object stores".into(),
                ));
            }
            replica.require_trusted_integration(&operation)?;
            if let Some(receipt) = authority_admissions.get(&operation.id()?) {
                replica.require_authority_admission(signed, receipt)?;
            }
            authorize(replica, signed)?;
            replica.validate_reference_capture(&operation, store)?;
            prepared.push((replica, signed, operation));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior:Option<(String,Vec<u8>,Vec<u8>,bool)>=tx.query_row("SELECT verb,request_hash,response,pending FROM operation_receipts WHERE namespace=?1 AND operation_id=?2",params![command.namespace,command.id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        if let Some((method, hash, response, pending)) = prior {
            if method != command.method || hash != command.request_hash || pending {
                return Err(Error::Invalid("publication command ID reused".into()));
            }
            return Ok(response);
        }
        for (replica, expected) in guards {
            let actual: i64 = tx.query_row(
                "SELECT generation FROM threads WHERE id=?1",
                [replica.thread.as_bytes()],
                |row| row.get(0),
            )?;
            if actual != *expected {
                return Err(Error::Invalid(
                    "source authority or frontier changed during publication".into(),
                ));
            }
        }
        for (replica, signed, operation) in prepared {
            replica.require_local_integration_source_in(&tx, &operation)?;
            if replica.receive_in(
                &tx,
                signed,
                &operation,
                store,
                false,
                authority_admissions.get(&operation.id()?),
            )? != Admission::Accepted
            {
                return Err(Error::Invalid(
                    "validated publication source did not settle".into(),
                ));
            }
        }
        self.record_source_possession_in(&tx, revision)?;
        let bytes = response()?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Invalid("publication receipt bound".into()));
        }
        tx.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",params![command.namespace,command.id.to_string(),crate::operation_dedup::receipt_record_key(command.namespace,command.id).as_bytes(),command.method,command.request_hash.as_slice(),bytes,chrono::Utc::now().timestamp()])?;
        tx.commit()?;
        drop(connection);
        self.notify_committed()?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
    use objects::object::{
        Attribution, Principal, State, Tree,
        thread_replication::{AuthoredCapture, GenesisOwner, ThreadGenesis, ThreadOperationBody},
    };

    use super::*;
    #[test]
    fn publication_receipt_and_source_availability_commit_or_roll_back_together() {
        let root = tempfile::tempdir().expect("repository");
        let repository = crate::Repository::init_default(root.path()).expect("init");
        let signer = Ed25519Signer::from_seed(&[41; 32]).expect("signer");
        let key = signer.public_key().try_into().expect("key");
        let base = repository.head().expect("head").expect("base");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(43).to_string(),
            owner: GenesisOwner::LocalKey(key),
            creator: key,
            parent: None,
            base,
            name: "publication".into(),
            intent: "atomic source".into(),
            nonce: vec![],
        };
        let replica = ThreadReplica::create(
            repository.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("Thread");
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![base],
            Attribution::human(Principal::new("owner", "")),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: key,
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                replica
                    .prepare_capture(&repository, &state)
                    .expect("source references"),
            )),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("capture");
        let id = operation.id().expect("id");
        let command_id = uuid::Uuid::new_v4()
            .to_string()
            .parse::<OperationId>()
            .expect("command");
        let command = || Command {
            namespace: "owner/agent",
            id: command_id,
            method: "PublishContent",
            request_hash: [7; 32],
        };
        let guards = vec![(replica.clone(), replica.generation().expect("generation"))];
        let failed = replica.publish_prepared_source(
            PreparedPublication {
                operations: std::slice::from_ref(&signed),
                authority_admissions: &Default::default(),
                revision: state.id(),
                guards: &guards,
            },
            repository.store(),
            command(),
            |_, _| Ok(()),
            || Err(Error::Invalid("receipt construction failed".into())),
        );
        assert!(
            failed
                .expect_err("receipt failure")
                .to_string()
                .contains("receipt construction failed")
        );
        assert!(
            replica.operation(&id).expect("operation").is_none(),
            "source operation must roll back with receipt"
        );
        assert!(
            !replica
                .has_source_possession(state.id())
                .expect("availability"),
            "availability must roll back with receipt"
        );
        assert!(
            crate::device_operations::replay_response(
                repository.heddle_dir(),
                &crate::device_operations::Command {
                    namespace: "owner/agent",
                    id: command_id,
                    method: "PublishContent",
                    request_hash: [7; 32]
                }
            )
            .expect("receipt read")
            .is_none()
        );
        let response = replica
            .publish_prepared_source(
                PreparedPublication {
                    operations: std::slice::from_ref(&signed),
                    authority_admissions: &Default::default(),
                    revision: state.id(),
                    guards: &guards,
                },
                repository.store(),
                command(),
                |_, _| Ok(()),
                || Ok(vec![9]),
            )
            .expect("atomic source");
        assert_eq!(response, vec![9]);
        assert!(
            replica
                .has_source_possession(state.id())
                .expect("available")
        );
        assert_eq!(
            replica
                .operation(&id)
                .expect("operation")
                .expect("original")
                .1,
            Admission::Accepted
        );
        let generation = replica.generation().expect("generation");
        assert_eq!(
            replica
                .publish_prepared_source(
                    PreparedPublication {
                        operations: std::slice::from_ref(&signed),
                        authority_admissions: &Default::default(),
                        revision: state.id(),
                        guards: &guards,
                    },
                    repository.store(),
                    command(),
                    |_, _| Ok(()),
                    || panic!("exact retry must reuse response")
                )
                .expect("retry"),
            response
        );
        assert_eq!(
            replica.generation().expect("generation"),
            generation,
            "retry leaves projection and availability unchanged"
        );
        let other = Command {
            namespace: "other actor",
            id: command_id,
            method: "PublishContent",
            request_hash: [7; 32],
        };
        assert!(
            replica
                .publish_prepared_source(
                    PreparedPublication {
                        operations: std::slice::from_ref(&signed),
                        authority_admissions: &Default::default(),
                        revision: state.id(),
                        guards: &guards,
                    },
                    repository.store(),
                    other,
                    |_, _| Ok(()),
                    || Ok(vec![10])
                )
                .expect_err("no cross-actor replay")
                .to_string()
                .contains("frontier changed")
        );
    }
}
