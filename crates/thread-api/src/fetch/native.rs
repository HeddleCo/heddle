//! Install a verified hosted download through the existing local store and
//! replica admission paths. This disk operation never advances a checkout.
use crypto::thread_operation::SignedGenesis;
use heddle_object_model::object::{ContentHash, StateId};
use objects::store::ObjectStore;
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
    /// Verify a privately owned device through independent account authority.
    /// Originals must already belong to this exact Spool.
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
        self.require_device_originals()?;
        let owner = repo::verify_account_owner_observation(&authority.owner, now_unix_seconds)
            .map_err(preparation)?;
        let account = owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Error::Invalid("account owner root absent"))?;
        let account_id = uuid::Uuid::from_slice(&account.account_uuid)
            .map_err(preparation)?
            .to_string();
        authority
            .verify_mint_root(&binding.attachment.root_public_key, now_unix_seconds)
            .map_err(preparation)?;
        authority
            .verify_publisher(&binding.attachment.subject_public_key)
            .map_err(preparation)?;
        authority
            .verify_publisher(&endpoint.public_key)
            .map_err(preparation)?;
        let roots = biscuit_verifier::parse_ed25519_public_keys_hex(
            &hex::encode(&binding.attachment.root_public_key),
            1,
        )
        .map_err(preparation)?;
        let verified = crate::root_attachment::verify(
            binding.attachment,
            binding.credential,
            &roots,
            &account_id,
            endpoint,
            chrono::DateTime::from_timestamp(now_unix_seconds, 0)
                .ok_or(Error::Invalid("invalid endpoint verification time"))?,
        )?;
        if verified
            .credential_revocation_ids()
            .iter()
            .any(|id| authority.revoked_ids.contains(id))
        {
            return Err(Error::Invalid(
                "endpoint binding credential is explicitly revoked",
            ));
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
        self.install_replicas(repository, Some(authority), spool_path, now_unix_seconds)?;
        Ok(self.state.id())
    }
    fn require_device_originals(&self) -> Result<(), Error> {
        let main = self
            .ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("Thread genesis absent"))?;
        if self.ready.import_authority.is_some()
            || self.ready.native_authority.is_some()
            || !self.authority_admissions.is_empty()
            || std::iter::once(main)
                .chain(&self.dependencies)
                .any(|record| {
                    record.admission.is_some()
                        || record.native_genesis_authority.is_some()
                        || !record.ownership_claim_admissions.is_empty()
                        || !record.ownership_resolution_admissions.is_empty()
                })
        {
            return Err(Error::HostedTrustRequired);
        }
        Ok(())
    }
    fn install_replicas(
        &self,
        repository: &Repository,
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
        let mut resolutions = std::collections::BTreeMap::<
            ContentHash,
            crate::replication::ownership::OriginalResolution,
        >::new();
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
                    if wrapper.admission.is_some() {
                        return Err(Error::HostedTrustRequired);
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
                            "/heddle.api.v1alpha2.ThreadService/StartThread",
                            now,
                        )
                        .map_err(preparation)?
                    }
                }
            };
            let mut pending = Vec::new();
            let retained = replica.ownership_claims().map_err(preparation)?;
            for claim in crate::replication::ownership::verify_claims(wrapper, &genesis)? {
                let value = claim.original.verify().map_err(preparation)?;
                if claim.authority_admission.is_some() {
                    return Err(Error::HostedTrustRequired);
                } else if !retained.contains(&claim.original) {
                    let authority = authority.ok_or(Error::Invalid("new claim requires current original acceptance or independently pinned admission"))?;
                    repo::thread_replication::ownership_claim::verify_claim_authority(
                        &claim.original,
                        &genesis,
                        authority,
                        spool_path,
                        now,
                    )
                    .map_err(preparation)?;
                }
                pending.push(PendingClaim {
                    original: claim,
                    remaining: value.source_frontier,
                });
            }
            for resolution in crate::replication::ownership::verify_resolutions(wrapper, &genesis)?
            {
                if resolutions
                    .insert(replica.thread_id(), resolution)
                    .is_some()
                {
                    return Err(Error::Invalid("duplicate ownership resolution"));
                }
            }
            claims.insert(replica.thread_id(), pending);
            replicas.insert(replica.thread_id(), replica);
        }
        if !self.authority_admissions.is_empty() {
            return Err(Error::HostedTrustRequired);
        }
        self.install_source_objects(repository)?;
        for (thread, pending) in &mut claims {
            install_ready_claims(
                replicas
                    .get(thread)
                    .ok_or(Error::Invalid("claim replica absent"))?,
                pending,
                authority,
                spool_path,
                now,
            )?;
        }
        for (thread, resolution) in &resolutions {
            install_ready_resolution(
                replicas
                    .get(thread)
                    .ok_or(Error::Invalid("resolution replica absent"))?,
                resolution,
                authority,
                spool_path,
                now,
            )?;
        }
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            let replica = replicas
                .get(&operation.thread)
                .ok_or(Error::Invalid("source dependency replica absent"))?;
            let id = operation.id().map_err(preparation)?;
            let prior = replica
                .operation_with_authority_admission(&id)
                .map_err(preparation)?;
            if !prior.is_some_and(|prior| {
                prior.original == *signed
                    && prior.status == objects::object::thread_replication::Admission::Accepted
            }) && let Some(author) = operation.source_author().map_err(preparation)?
            {
                match author {
                    objects::object::thread_replication::SourceAuthor::LocalKey => replica
                        .verify_local_source_owner(&operation)
                        .map_err(preparation)?,
                    objects::object::thread_replication::SourceAuthor::Account { .. } => {
                        replica.verify_source_authority(&operation, authority.ok_or(Error::Invalid("fresh source requires original authority or retained admission"))?, spool_path, now).map_err(preparation)?;
                    }
                }
            }

            let admission = if !self.is_complete() {
                replica.receive_source_metadata(
                    signed,
                    repository.store(),
                    None,
                    require_source_operation,
                )
            } else {
                replica.receive(signed, repository.store(), require_source_operation)
            }
            .map_err(preparation)?;
            if admission != objects::object::thread_replication::Admission::Accepted {
                return Err(Error::Invalid(
                    "source proof did not settle in dependency order",
                ));
            }
            if let Some(pending) = claims.get_mut(&operation.thread) {
                for claim in pending.iter_mut() {
                    claim.remaining.remove(&id);
                }
                install_ready_claims(replica, pending, authority, spool_path, now)?;
            }
            if let Some(resolution) = resolutions.get(&operation.thread) {
                install_ready_resolution(replica, resolution, authority, spool_path, now)?;
            }
        }
        if claims.values().any(|claims| !claims.is_empty()) {
            return Err(Error::Invalid(
                "ownership cutoff did not settle before source completion",
            ));
        }
        for (thread, resolution) in &resolutions {
            let replica = replicas
                .get(thread)
                .ok_or(Error::Invalid("resolution replica absent"))?;
            install_ready_resolution(replica, resolution, authority, spool_path, now)?;
            if replica
                .ownership_resolution()
                .map_err(preparation)?
                .is_none()
            {
                return Err(Error::Invalid(
                    "ownership resolution frontier did not settle",
                ));
            }
        }
        for thread in claims
            .keys()
            .filter(|thread| !resolutions.contains_key(*thread))
        {
            replicas
                .get(thread)
                .ok_or(Error::Invalid("claim replica absent"))?
                .effective_owner()
                .map_err(preparation)?;
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
        let selected = replicas
            .get(&main_id)
            .ok_or(Error::Invalid("selected replica absent"))?;
        if self.operations.is_empty() {
            let genesis = selected.genesis().map_err(preparation)?;
            let canonical = self.state.encode_current_msgpack().map_err(preparation)?;
            objects::object::thread_replication::initial_base::initial_base_state(
                &genesis, &canonical,
            )
            .map_err(preparation)?;
        } else {
            let mut source = selected;
            let mut proved = false;
            let mut possession = Vec::new();
            for _ in 0..128 {
                if source
                    .accepted_source_revision(self.state.id())
                    .map_err(preparation)?
                    .is_some()
                {
                    possession.push(source);
                    proved = true;
                    break;
                }
                let genesis = source.genesis().map_err(preparation)?;
                if genesis.base != self.state.id() {
                    break;
                }
                possession.push(source);
                let Some(parent) = genesis.parent else { break };
                let Some(next) = replicas.get(&parent) else {
                    break;
                };
                if next.genesis().map_err(preparation)?.spool != genesis.spool {
                    break;
                }
                source = next;
            }
            if !proved {
                return Err(Error::Invalid("selected source proof did not settle"));
            }
            if self.is_complete() {
                for replica in possession {
                    replica
                        .record_source_possession(self.state.id())
                        .map_err(preparation)?;
                }
            }
        }
        if self.is_complete() {
            selected
                .record_source_possession(self.state.id())
                .map_err(preparation)?;
        }
        Ok(())
    }

    pub(super) fn install_source_objects(&self, repository: &Repository) -> Result<(), Error> {
        let pack = self.directory.path().join("source.pack");
        let index = self.directory.path().join("source.idx");
        // Verified converted Git ancestors are States only, already address
        // checked and closure checked against their signed import tip.
        if let Some([ancestry_pack, ancestry_index]) = self.ancestry_paths() {
            repository
                .store()
                .install_pack_streaming(&ancestry_pack, &ancestry_index)
                .map_err(preparation)?;
        }
        if self.is_complete() {
            return repository
                .store()
                .install_pack_streaming(&pack, &index)
                .map(|_| ())
                .map_err(preparation);
        }
        // HRT1 is a disclosure proof, not a full tree object. Split it out before
        // registering the visible immutable records in the shared object store.
        let visible_pack = self.directory.path().join("visible.pack");
        let visible_index = self.directory.path().join("visible.idx");
        let output = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&visible_pack)?;
        let mut builder = heddle_pack::store::pack::StreamingPackBuilder::new(
            output,
            visible_index.clone(),
            Default::default(),
            self.directory.path().join("visible-buckets"),
        )
        .map_err(preparation)?;
        let reader =
            heddle_pack::store::pack::PackReader::open(&pack, &index, self.directory.path())
                .map_err(preparation)?;
        reader
            .visit_objects(|id, kind, bytes| {
                if kind != heddle_pack::store::pack::ObjectType::Tree
                    || !objects::object::is_redacted_tree(bytes)
                {
                    builder.add_id(id, kind, bytes)?;
                }
                Ok(())
            })
            .map_err(preparation)?;
        let (output, _) = builder.finalize().map_err(preparation)?;
        drop(output);
        if let Some(closure) = &self.partial_trees {
            closure
                .visit_partial_trees(|partial| {
                    let bytes = objects::object::encode_redacted_projection(partial)?;
                    repository
                        .store()
                        .put_partial_tree(&partial.declared_root(), &bytes)
                        .map(|_| ())
                })
                .map_err(preparation)?;
        }
        repository
            .store()
            .install_pack_streaming(&visible_pack, &visible_index)
            .map(|_| ())
            .map_err(preparation)
    }
}
struct PendingClaim {
    original: crate::replication::ownership::OriginalClaim,
    remaining: std::collections::BTreeSet<ContentHash>,
}
fn install_ready_resolution(
    replica: &ThreadReplica,
    resolution: &crate::replication::ownership::OriginalResolution,
    authority: Option<&repo::device_authority::DeviceAuthority>,
    spool_path: &str,
    now: i64,
) -> Result<(), Error> {
    if let Some(existing) = replica.ownership_resolution().map_err(preparation)? {
        if existing == resolution.original {
            return Ok(());
        }
        return Err(Error::Invalid(
            "incoming ownership resolution conflicts with retained history",
        ));
    }
    let value = heddle_object_model::object::thread_replication::ownership_resolution::ThreadOwnershipResolution::decode(&resolution.original.canonical)
        .map_err(preparation)?;
    if replica.ownership_claims().map_err(preparation)?.len() != value.conflicting_claims.len() {
        return Ok(());
    }
    for head in &value.frontier {
        if replica
            .operation(head)
            .map_err(preparation)?
            .is_none_or(|(_, status)| {
                status != objects::object::thread_replication::Admission::Accepted
            })
        {
            return Ok(());
        }
    }
    if resolution.authority_admission.is_some() {
        return Err(Error::HostedTrustRequired);
    } else {
        replica
            .resolve_ownership(
                &resolution.original,
                authority.ok_or(Error::Invalid(
                    "new ownership resolution requires current recipient authority",
                ))?,
                spool_path,
                now,
            )
            .map_err(preparation)?;
    }
    Ok(())
}
fn install_ready_claims(
    replica: &ThreadReplica,
    pending: &mut Vec<PendingClaim>,
    authority: Option<&repo::device_authority::DeviceAuthority>,
    spool_path: &str,
    now: i64,
) -> Result<(), Error> {
    let mut index = 0;
    while index < pending.len() {
        if !pending[index].remaining.is_empty() {
            index += 1;
            continue;
        }
        let claim = pending.remove(index).original;
        if claim.authority_admission.is_some() {
            return Err(Error::HostedTrustRequired);
        } else if !replica
            .ownership_claims()
            .map_err(preparation)?
            .contains(&claim.original)
        {
            replica
                .claim_ownership_for_import(
                    &claim.original,
                    authority.ok_or(Error::Invalid("new claim authority absent"))?,
                    spool_path,
                    now,
                )
                .map_err(preparation)?;
        }
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
