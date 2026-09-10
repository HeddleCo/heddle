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

/// Current endpoint possession under independently retained account authority.
/// Retain the original credential privately; it never joins source proof packs.
pub struct OwnedDeviceBinding<'a> {
    pub attachment: &'a crate::contract::RootAttachment,
    pub credential: &'a [u8],
}

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
    /// The destination must be an unseeded `Repository::init` skeleton or
    /// already belong to this exact Spool; importing never replaces local work.
    pub fn install_owned_device(
        self,
        repository: &Repository,
        authority: &repo::device_authority::DeviceAuthority,
        binding: OwnedDeviceBinding<'_>,
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
        let owner = repo::verify_account_owner_observation(&authority.owner, now_unix_seconds).map_err(preparation)?;
        let account = owner.signed_root().root.as_ref().ok_or(Error::Invalid("account owner root absent"))?;
        let account_id = uuid::Uuid::from_slice(&account.account_uuid).map_err(preparation)?.to_string();
        authority.verify_mint_root(&binding.attachment.root_public_key, now_unix_seconds).map_err(preparation)?;
        authority.verify_publisher(&binding.attachment.subject_public_key).map_err(preparation)?;
        authority.verify_publisher(&endpoint.public_key).map_err(preparation)?;
        let roots = biscuit_verifier::parse_ed25519_public_keys_hex(&hex::encode(&binding.attachment.root_public_key), 1).map_err(preparation)?;
        let verified = crate::root_attachment::verify(binding.attachment, binding.credential, &roots, &account_id, endpoint,
            chrono::DateTime::from_timestamp(now_unix_seconds, 0).ok_or(Error::Invalid("invalid endpoint verification time"))?)?;
        if verified.credential_revocation_ids().iter().any(|id| authority.revoked_ids.contains(id)) {
            return Err(Error::Invalid("endpoint binding credential is explicitly revoked"));
        }
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
        let mut claims = std::collections::BTreeMap::<ContentHash, Vec<PendingClaim>>::new();
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
            let replica = match &genesis.owner {
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
                        let [_signature] = receipt.signatures.as_slice() else {
                            return Err(Error::Invalid(
                                "one hosted genesis admission signature required",
                            ));
                        };
                        let admission = crate::boundary_acceptance::genesis_admission(wrapper)?.ok_or(Error::Invalid("genesis admission absent"))?;
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
            let mut pending = Vec::new();
            let retained = replica.ownership_claims().map_err(preparation)?;
            for claim in crate::replication::ownership::verify_claims(wrapper, &genesis)? {
                let value = claim.original.verify().map_err(preparation)?;
                if let Some(receipt) = &claim.authority_admission {
                    let trust = replica.authority_admission_trust(receipt).map_err(preparation)?;
                    receipt.verify_claim(&claim.original, &genesis, &trust).map_err(preparation)?;
                } else if !retained.contains(&claim.original) {
                    let authority = authority.ok_or(Error::Invalid("new claim requires current original acceptance or independently pinned admission"))?;
                    repo::thread_replication::ownership_claim::verify_claim_authority(&claim.original, &genesis, authority, spool_path, now).map_err(preparation)?;
                }
                pending.push(PendingClaim { original: claim, remaining: value.source_frontier });
            }
            claims.insert(replica.thread_id(), pending);
            replicas.insert(replica.thread_id(), replica);
        }
        // Verify portable original testimony before installing immutable bytes.
        // Fresh capabilities are evaluated in dependency order below, after
        // explicit claims, while availability remains unpublished on failure.
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            let replica = replicas.get(&operation.thread).ok_or(Error::Invalid("source dependency replica absent"))?;
            if let Some(receipt) = self.authority_admissions.get(&operation.id().map_err(preparation)?) {
                replica.require_authority_admission(signed, receipt).map_err(preparation)?;
            }
        }
        repository
            .store()
            .install_pack_streaming(
                &self.directory.path().join("source.pack"),
                &self.directory.path().join("source.idx"),
            )
            .map_err(preparation)?;
        for (thread, pending) in &mut claims {
            install_ready_claims(replicas.get(thread).ok_or(Error::Invalid("claim replica absent"))?, pending, authority, spool_path, now)?;
        }
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            let replica = replicas
                .get(&operation.thread)
                .ok_or(Error::Invalid("source dependency replica absent"))?;
            let id = operation.id().map_err(preparation)?;
            if !self.authority_admissions.contains_key(&id) {
                let prior = replica.operation_with_authority_admission(&id).map_err(preparation)?;
                if !prior.is_some_and(|prior| prior.original == *signed && prior.status == objects::object::thread_replication::Admission::Accepted) {
                    if let Some(author) = operation.source_author().map_err(preparation)? {
                        match author {
                            objects::object::thread_replication::SourceAuthor::LocalKey => replica.verify_local_source_owner(&operation).map_err(preparation)?,
                            objects::object::thread_replication::SourceAuthor::Account { .. } => {
                                replica.verify_source_authority(&operation, authority.ok_or(Error::Invalid("fresh source requires original authority or retained admission"))?, spool_path, now).map_err(preparation)?;
                            }
                        }
                    }
                }
            }
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
            if let Some(pending) = claims.get_mut(&operation.thread) {
                for claim in pending.iter_mut() { claim.remaining.remove(&id); }
                install_ready_claims(replica, pending, authority, spool_path, now)?;
            }
        }
        if claims.values().any(|claims| !claims.is_empty()) {
            return Err(Error::Invalid("ownership cutoff did not settle before source completion"));
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
struct PendingClaim {
    original: crate::replication::ownership::OriginalClaim,
    remaining: std::collections::BTreeSet<ContentHash>,
}
fn install_ready_claims(
    replica: &ThreadReplica, pending: &mut Vec<PendingClaim>,
    authority: Option<&repo::device_authority::DeviceAuthority>, spool_path: &str, now: i64,
) -> Result<(), Error> {
    let mut index = 0;
    while index < pending.len() {
        if !pending[index].remaining.is_empty() { index += 1; continue; }
        let claim = pending.remove(index).original;
        if let Some(receipt) = &claim.authority_admission {
            replica.claim_ownership_with_admission(&claim.original, receipt).map_err(preparation)?;
        } else if !replica.ownership_claims().map_err(preparation)?.contains(&claim.original) {
            replica.claim_ownership(&claim.original, authority.ok_or(Error::Invalid("new claim authority absent"))?, spool_path, now).map_err(preparation)?;
        }
        replica.effective_owner().map_err(preparation)?;
    }
    Ok(())
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
