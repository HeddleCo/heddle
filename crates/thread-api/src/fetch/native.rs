//! Install a verified hosted download through the existing local store and
//! replica admission paths. This disk operation never advances a checkout.
use crypto::thread_operation::SignedGenesis;
use heddle_object_model::object::{
    ContentHash, StateId,
    thread_replication::integration::{SPOOL_GENESIS_TRUST_FORMAT, TrustedHostedExecutor},
};
use objects::store::ObjectStore;
use prost::Message;
use repo::{Repository, thread_replication::ThreadReplica};

use super::{Error, StagedSource};
use crate::contract::EndpointKind;

impl StagedSource {
    /// Call on the application's disk worker. Owner history is independently
    /// verified and monotonically pinned; the authenticated hosted endpoint is
    /// pinned before accepting any original Integration operations.
    pub fn install(self, repository: &Repository, now_unix_seconds: i64) -> Result<StateId, Error> {
        let endpoint = self
            .ready
            .endpoint
            .as_ref()
            .ok_or(Error::Invalid("endpoint absent"))?;
        if endpoint.kind != EndpointKind::Weft as i32 {
            return Err(Error::Invalid(
                "hosted installation requires the selected Weft endpoint",
            ));
        }
        let executor = endpoint
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid("invalid hosted endpoint key"))?;
        let spool = uuid::Uuid::parse_str(
            &self
                .ready
                .thread
                .as_ref()
                .and_then(|t| t.spool.as_ref())
                .ok_or(Error::Invalid("Spool absent"))?
                .id,
        )
        .map_err(preparation)?;
        let genesis = self
            .ready
            .owner_genesis
            .as_ref()
            .ok_or(Error::Invalid("owner genesis absent"))?;
        let owner = self
            .ready
            .ownership
            .as_ref()
            .ok_or(Error::Invalid("owner history absent"))?;
        let verified =
            repo::verify_spool_owner_observation(genesis, owner, spool, now_unix_seconds)
                .map_err(preparation)?;
        let body = genesis
            .genesis
            .as_ref()
            .ok_or(Error::Invalid("owner genesis body absent"))?;
        let trust = TrustedHostedExecutor {
            spool,
            spool_genesis: ContentHash::compute_typed(
                SPOOL_GENESIS_TRUST_FORMAT,
                &body.encode_to_vec(),
            ),
            executor,
        };
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            require_source_operation(&operation).map_err(preparation)?;
            if operation
                .hosted_execution_binding()
                .map_err(preparation)?
                .is_some()
            {
                trust.authorize(&operation).map_err(preparation)?;
            }
        }
        repository
            .verify_and_pin_owner_observation(
                genesis,
                owner,
                spool,
                &verified.wire().canonical_spool_path_segments,
                now_unix_seconds,
            )
            .map_err(preparation)?;
        self.install_replicas(repository, Some(&trust), None, "", now_unix_seconds)?;
        Ok(self.state.id())
    }
    /// Device-only source uses independently admitted local account authority;
    /// incoming material cannot enroll its endpoint or replace a Spool owner.
    pub fn install_owned_device(
        self,
        repository: &Repository,
        authority: &repo::device_authority::DeviceAuthority,
        spool_path: &str,
        now_unix_seconds: i64,
    ) -> Result<StateId, Error> {
        let endpoint = self
            .ready
            .endpoint
            .as_ref()
            .ok_or(Error::Invalid("endpoint absent"))?;
        if endpoint.kind != EndpointKind::Device as i32 {
            return Err(Error::Invalid(
                "owned-device installation requires device endpoint",
            ));
        }
        authority
            .verify_mint_root(&endpoint.public_key, now_unix_seconds)
            .map_err(preparation)?;
        authority
            .verify_publisher(&endpoint.public_key)
            .map_err(preparation)?;
        let spool = self
            .ready
            .thread
            .as_ref()
            .and_then(|thread| thread.spool.as_ref())
            .ok_or(Error::Invalid("Spool absent"))?;
        repository
            .install_native_spool_id(spool.id.parse().map_err(preparation)?)
            .map_err(preparation)?;
        self.install_replicas(
            repository,
            None,
            Some(authority),
            spool_path,
            now_unix_seconds,
        )?;
        Ok(self.state.id())
    }
    fn install_replicas(
        &self,
        repository: &Repository,
        trust: Option<&TrustedHostedExecutor>,
        authority: Option<&repo::device_authority::DeviceAuthority>,
        spool_path: &str,
        now: i64,
    ) -> Result<(), Error> {
        let main = self
            .ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("Thread genesis absent"))?;
        let mut replicas = std::collections::BTreeMap::new();
        for wrapper in std::iter::once(main).chain(&self.dependencies) {
            let original = wrapper
                .genesis
                .as_ref()
                .ok_or(Error::Invalid("original signed genesis absent"))?;
            let [signature] = original.signatures.as_slice() else {
                return Err(Error::Invalid("one original creator signature required"));
            };
            let signed = SignedGenesis {
                canonical: original.canonical_record.clone(),
                signature: signature.signature.clone(),
            };
            let genesis = signed.verify().map_err(preparation)?;
            let replica = match genesis.owner {
                heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(_) => {
                    if !wrapper.creator_authority.is_empty() || wrapper.admission.is_some() {
                        return Err(Error::Invalid(
                            "local ownership cannot carry implicit account admission",
                        ));
                    }
                    ThreadReplica::create(repository.heddle_dir(), &signed).map_err(preparation)?
                }
                heddle_object_model::object::thread_replication::GenesisOwner::Account(_) => {
                    if let Some(receipt) = &wrapper.admission {
                        if receipt.format
                            != heddle_object_model::object::thread_genesis_admission::FORMAT
                        {
                            return Err(Error::Invalid("unknown genesis admission format"));
                        }
                        let [signature] = receipt.signatures.as_slice() else {
                            return Err(Error::Invalid(
                                "one hosted genesis admission signature required",
                            ));
                        };
                        let admission = crypto::thread_genesis_admission::SignedGenesisAdmission {
                            canonical: receipt.canonical_record.clone(),
                            signature: signature.signature.clone(),
                        };
                        let value = admission.verify_signature().map_err(preparation)?;
                        if signature.public_key != value.executor {
                            return Err(Error::Invalid("genesis admission key differs"));
                        }
                        match trust {
                            Some(trust) => ThreadReplica::create_from_genesis_admission(
                                repository.heddle_dir(),
                                &signed,
                                &wrapper.creator_authority,
                                &admission,
                                trust,
                            ),
                            None => ThreadReplica::create_from_pinned_genesis_admission(
                                repository.heddle_dir(),
                                &signed,
                                &wrapper.creator_authority,
                                &admission,
                            ),
                        }
                        .map_err(preparation)?
                    } else {
                        let authority = authority.ok_or(Error::Invalid(
                            "original account genesis admission required",
                        ))?;
                        ThreadReplica::create_authorized(
                            repository.heddle_dir(),
                            &signed,
                            &wrapper.creator_authority,
                            authority,
                            spool_path,
                            "/heddle.api.v2alpha1.ThreadService/StartThread",
                            now,
                        )
                        .map_err(preparation)?
                    }
                }
            };
            if let Some(trust) = trust {
                let executor = trust.executor;
                repository
                    .pin_thread_hosted_executor(&replica, executor)
                    .map_err(preparation)?;
            }
            replicas.insert(replica.thread_id(), replica);
        }
        // Structural source signatures are not membership proofs. Resolve all
        // fresh source authors before installing supplied bytes. Exact accepted
        // originals keep their durable admission without credential refresh.
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            let replica = replicas.get(&operation.thread).ok_or(Error::Invalid("source dependency replica absent"))?;
            if let Some(receipt) = self.authority_admissions.get(&operation.id().map_err(preparation)?) {
                replica.require_authority_admission(signed, receipt).map_err(preparation)?;
                continue;
            }
            let prior = replica.operation_with_authority_admission(&operation.id().map_err(preparation)?).map_err(preparation)?;
            if prior.is_some_and(|prior| prior.original == *signed && prior.status == objects::object::thread_replication::Admission::Accepted) {
                continue;
            }
            if operation.local_integration().map_err(preparation)?.is_some() && operation.publisher != replica.genesis().map_err(preparation)?.creator {
                return Err(Error::Invalid("fresh local integration requires independently admitted author authority"));
            }
            if let objects::object::thread_replication::ThreadOperationBody::Capture(capture) = &operation.body {
                let genesis = replica.genesis().map_err(preparation)?;
                match &capture.author {
                    objects::object::thread_replication::SourceAuthor::LocalKey => repo::thread_replication::source_authority::verify_local_source_owner(&operation, &genesis).map_err(preparation)?,
                    objects::object::thread_replication::SourceAuthor::Account { .. } => {
                        let authority = authority.ok_or(Error::Invalid("fresh account source requires original author authority or retained admission"))?;
                        repo::thread_replication::source_authority::verify_source_authority(&operation, &genesis, authority, spool_path, now).map_err(preparation)?;
                    }
                }
            }
        }
        repository
            .store()
            .install_pack_streaming(
                &self.directory.path().join("source.pack"),
                &self.directory.path().join("source.idx"),
            )
            .map_err(preparation)?;
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            let replica = replicas
                .get(&operation.thread)
                .ok_or(Error::Invalid("source dependency replica absent"))?;
            let admission = if let Some(receipt) = self.authority_admissions.get(&operation.id().map_err(preparation)?) {
                replica.receive_with_authority_admission(signed, receipt, repository.store(), require_source_operation)
            } else { replica.receive(signed, repository.store(), require_source_operation) }.map_err(preparation)?;
            if admission
                != objects::object::thread_replication::Admission::Accepted
            {
                return Err(Error::Invalid(
                    "source proof did not settle in dependency order",
                ));
            }
        }
        let main_id = crate::replication::opening::verify_genesis(
            main.genesis
                .as_ref()
                .ok_or(Error::Invalid("signed genesis absent"))?,
            self.ready
                .thread
                .as_ref()
                .ok_or(Error::Invalid("Thread absent"))?,
        )?
        .id()
        .map_err(preparation)?;
        if replicas
            .get(&main_id)
            .ok_or(Error::Invalid("selected replica absent"))?
            .accepted_source_revision(self.state.id())
            .map_err(preparation)?
            .is_none()
        {
            return Err(Error::Invalid("selected source proof did not settle"));
        }
        replicas.get(&main_id).ok_or(Error::Invalid("selected replica absent"))?
            .record_source_possession(self.state.id()).map_err(preparation)?;
        Ok(())
    }
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}

// Stage already negotiates Source alone. Keep that trust boundary explicit at
// installation too: source verification is never original Metadata authority.
fn require_source_operation(
    operation: &heddle_object_model::object::thread_replication::ThreadOperation,
) -> repo::thread_replication::Result<()> {
    if operation.facet() != heddle_object_model::object::thread_replication::ThreadFacet::Source {
        return Err(repo::thread_replication::Error::Invalid(
            "source installation cannot admit non-source authority".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
    use heddle_object_model::object::{
        CollaborationActor,
        thread_replication::{
            ThreadGenesis, ThreadOperation, ThreadOperationBody,
            metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
        },
    };

    use super::*;
    #[test]
    fn source_install_gate_cannot_create_metadata_original_authority() {
        let directory = tempfile::tempdir().expect("repository");
        let repository = Repository::init_default(directory.path()).expect("repo");
        let signer = Ed25519Signer::from_seed(&[56; 32]).expect("signer");
        let spool = uuid::Uuid::from_u128(11);
        let genesis = ThreadGenesis {
            owner: objects::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("key"),
            ),
            version: 1,
            spool: spool.to_string(),
            parent: None,
            base: repository.head().expect("head").expect("base"),
            name: "source import".into(),
            intent: "original authority".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![5],
        };
        let replica = ThreadReplica::create(
            repository.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("original genesis"),
        )
        .expect("replica");
        let proof = b"unverified author evidence must not establish admission".to_vec();
        let control = ThreadControl {
            version: 1,
            spool,
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_u128(22),
                agent_id: None,
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &proof),
            authority_envelope: proof,
            client_operation_id: uuid::Uuid::now_v7(),
            occurred_at_ms: 0,
            control: Control::Name("unproved author".into()),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Metadata(control.encode().expect("valid canonical control")),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("valid original signature");
        signed.verify().expect("signature itself is valid");
        let failure = replica
            .receive(&signed, repository.store(), require_source_operation)
            .expect_err("source-only gate denies unproved Metadata");
        assert!(failure.to_string().contains("non-source authority"));
        assert!(
            replica
                .operation(&operation.id().expect("ID"))
                .expect("stored operation")
                .is_none(),
            "denial precedes immutable persistence"
        );
        assert!(
            !replica
                .original_authority_admitted(&signed)
                .expect("admission marker"),
            "source trust cannot manufacture author admission"
        );
    }
}
