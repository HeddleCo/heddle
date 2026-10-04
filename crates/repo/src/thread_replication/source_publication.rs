//! One validated source publication commits original operations, availability,
//! and its caller-scoped receipt together. Pack validation and staging happen
//! outside the destination transaction; witnessed artifacts publish through the
//! existing borrowed journal facade under that transaction.
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
    /// Witnessed originals, artifacts, possession and the command receipt share
    /// Part 1b's authoritative transaction. Every original is verified again;
    /// a retained admission never substitutes for the supplied fresh evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_hybrid_source(
        &self,
        trust: &super::hosted_trust::HostedTrust<impl super::hosted_trust::Clock>,
        bundle: &[u8],
        records: &[api::heddle::api::v1alpha2::SignedRecord],
        authority: &impl super::delegated_import::AcceptedAuthority,
        store: &impl ObjectStore,
        prepared: PreparedPublication<'_>,
        command: Command<'_>,
        publish: impl FnOnce(
            &super::hosted_trust::TrustTransaction<'_>,
            &mut super::install_artifacts::InstallArtifacts<'_>,
        ) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        self.publish_witnessed_source(
            trust, bundle, records, authority, store, prepared, command, publish, false,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn publish_native_source(
        &self,
        trust: &super::hosted_trust::HostedTrust<impl super::hosted_trust::Clock>,
        bundle: &[u8],
        records: &[api::heddle::api::v1alpha2::SignedRecord],
        authority: &impl super::delegated_import::AcceptedAuthority,
        store: &impl ObjectStore,
        prepared: PreparedPublication<'_>,
        command: Command<'_>,
        publish: impl FnOnce(
            &super::hosted_trust::TrustTransaction<'_>,
            &mut super::install_artifacts::InstallArtifacts<'_>,
        ) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        self.publish_witnessed_source(
            trust, bundle, records, authority, store, prepared, command, publish, true,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn publish_witnessed_source(
        &self,
        trust: &super::hosted_trust::HostedTrust<impl super::hosted_trust::Clock>,
        bundle: &[u8],
        records: &[api::heddle::api::v1alpha2::SignedRecord],
        authority: &impl super::delegated_import::AcceptedAuthority,
        store: &impl ObjectStore,
        prepared: PreparedPublication<'_>,
        command: Command<'_>,
        publish: impl FnOnce(
            &super::hosted_trust::TrustTransaction<'_>,
            &mut super::install_artifacts::InstallArtifacts<'_>,
        ) -> Result<Vec<u8>>,
        native: bool,
    ) -> Result<Vec<u8>> {
        if command.namespace.is_empty()
            || command.namespace.len() > 1024
            || command.namespace.contains('\0')
            || prepared.operations.is_empty()
            || prepared.operations.len() > 10_000
            || prepared.guards.is_empty()
            || prepared.guards.len() > 128
        {
            return Err(Error::Invalid(
                "bounded authenticated publication required".into(),
            ));
        }
        let result = std::cell::RefCell::new(None);
        let directory = self
            .path
            .parent()
            .ok_or_else(|| Error::Invalid("metadata has no parent".into()))?;
        let before = |context: &super::hosted_trust::TrustTransaction<'_>| {
            let prior = command_replay(context.sql(), &command)?;
            if prior.is_none() {
                check_guards(context.sql(), prepared.guards, &self.path)?;
            }
            Ok(())
        };
        let publish =
            |context: &super::hosted_trust::TrustTransaction<'_>,
             artifacts: &mut super::install_artifacts::InstallArtifacts<'_>| {
                // Check exact signed bytes and their fresh witnessed admission in
                // this transaction, after install_in verified the whole bundle.
                for signed in prepared.operations {
                    let op = signed.verify()?;
                    let id = op.id()?;
                    if op.source_state()?.is_none()
                        || !records.iter().any(|r| {
                            r.format == objects::object::thread_replication::OPERATION_FORMAT
                                && r.canonical_record == signed.canonical
                                && r.signatures.iter().any(|s| {
                                    s.public_key == op.publisher && s.signature == signed.signature
                                })
                        })
                    {
                        return Err(Error::Invalid(
                            "witnessed publication original absent".into(),
                        ));
                    }
                    let accepted: bool = context.sql().query_row("SELECT EXISTS(SELECT 1 FROM operations o LEFT JOIN hosted_import_admissions a ON a.operation=o.id WHERE o.id=?1 AND o.thread=?2 AND o.canonical=?3 AND o.signature=?4 AND o.status=1 AND (a.operation IS NOT NULL OR EXISTS(SELECT 1 FROM hosted_native_proofs p WHERE p.thread=o.thread)))", params![id.as_bytes(),op.thread.as_bytes(),signed.canonical,signed.signature], |r| r.get(0))?;
                    if !accepted {
                        return Err(Error::Invalid(
                            "witnessed publication source did not settle".into(),
                        ));
                    }
                }
                let bytes = publish(context, artifacts)?;
                self.record_source_possession_in(context.sql(), prepared.revision)?;
                let bytes = match command_replay(context.sql(), &command)? {
                    Some(prior) => prior,
                    None => {
                        record_command(context.sql(), &command, &bytes)?;
                        bytes
                    }
                };
                *result.borrow_mut() = Some(bytes);
                Ok(())
            };
        if native {
            Self::install_hybrid_native_with(
                directory, trust, bundle, records, authority, store, before, publish,
            )
        } else {
            Self::install_hybrid_import_with(
                directory, trust, bundle, records, authority, store, before, publish,
            )
        }?;
        result
            .into_inner()
            .ok_or_else(|| Error::Invalid("publication receipt absent".into()))
    }

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
        if let Some(response) = command_replay(&tx, &command)? {
            return Ok(response);
        }
        check_guards(&tx, guards, &self.path)?;
        for (replica, signed, operation) in prepared {
            replica.require_local_integration_source_in(&tx, &operation)?;
            if replica.receive_in(
                &tx,
                signed,
                &operation,
                store,
                false,
                authority_admissions.get(&operation.id()?),
                false,
            )? != Admission::Accepted
            {
                return Err(Error::Invalid(
                    "validated publication source did not settle".into(),
                ));
            }
        }
        self.record_source_possession_in(&tx, revision)?;
        let bytes = response()?;
        record_command(&tx, &command, &bytes)?;
        tx.commit()?;
        drop(connection);
        self.notify_committed()?;
        Ok(bytes)
    }
}

fn command_replay(
    tx: &rusqlite::Transaction<'_>,
    command: &Command<'_>,
) -> Result<Option<Vec<u8>>> {
    let prior: Option<(String, Vec<u8>, Vec<u8>, bool)> = tx.query_row("SELECT verb,request_hash,response,pending FROM operation_receipts WHERE namespace=?1 AND operation_id=?2", params![command.namespace, command.id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    if let Some((method, hash, response, pending)) = prior {
        if method != command.method || hash != command.request_hash || pending {
            return Err(Error::Invalid("publication command ID reused".into()));
        }
        return Ok(Some(response));
    }
    Ok(None)
}
fn check_guards(
    tx: &rusqlite::Transaction<'_>,
    guards: &[(ThreadReplica, i64)],
    path: &std::path::Path,
) -> Result<()> {
    for (replica, expected) in guards {
        if replica.path != path {
            return Err(Error::Invalid(
                "source publication crosses object stores".into(),
            ));
        }
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
    Ok(())
}
fn record_command(
    tx: &rusqlite::Transaction<'_>,
    command: &Command<'_>,
    bytes: &[u8],
) -> Result<()> {
    if bytes.len() > 1024 * 1024 {
        return Err(Error::Invalid("publication receipt bound".into()));
    }
    tx.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",params![command.namespace,command.id.to_string(),crate::operation_dedup::receipt_record_key(command.namespace,command.id).as_bytes(),command.method,command.request_hash.as_slice(),bytes,chrono::Utc::now().timestamp()])?;
    Ok(())
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
