//! Selected hosted installation keeps repository artifacts inside the same
//! trust transaction as native admission. Staging stores supply no authority.
use std::path::Path;

use heddle_object_model::object::StateId;
use objects::{lock::RepositoryLockExt, store::ObjectStore};
use prost::Message;
use repo::{
    Repository,
    thread_replication::{
        ThreadReplica,
        delegated_import::AcceptedAuthority,
        hosted_trust::{Clock, HostedTrust},
        install_artifacts::InstallArtifacts,
    },
};

use super::{Error, StagedSource};

pub struct HostedPublication<'a> {
    pub replica: &'a ThreadReplica,
    pub prepared: repo::thread_replication::source_publication::PreparedPublication<'a>,
    pub command: repo::thread_replication::source_publication::Command<'a>,
}

impl StagedSource {
    /// `authority` and `trust` are independently selected by the receiver.
    /// Every selected original is admitted again under the durable trust lock;
    /// the complete public history never selects unrelated native branches.
    pub fn install_hosted(
        self,
        repository: &Repository,
        trust: &HostedTrust<impl Clock>,
        authority: &impl AcceptedAuthority,
        now_seconds: i64,
    ) -> Result<StateId, Error> {
        self.install_hosted_commit(repository, trust, authority, now_seconds, None, |_| {
            Ok(Vec::new())
        })
        .map(|(state, _)| state)
    }
    pub fn publish_hosted(
        self,
        repository: &Repository,
        trust: &HostedTrust<impl Clock>,
        authority: &impl AcceptedAuthority,
        now_seconds: i64,
        publication: HostedPublication<'_>,
        response: impl FnOnce(
            &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
        ) -> repo::thread_replication::Result<Vec<u8>>,
    ) -> Result<Vec<u8>, Error> {
        self.install_hosted_commit(
            repository,
            trust,
            authority,
            now_seconds,
            Some(publication),
            response,
        )
        .map(|(_, receipt)| receipt)
    }
    #[allow(clippy::too_many_arguments)]
    fn install_hosted_commit(
        self,
        repository: &Repository,
        trust: &HostedTrust<impl Clock>,
        authority: &impl AcceptedAuthority,
        now_seconds: i64,
        publication: Option<HostedPublication<'_>>,
        response: impl FnOnce(
            &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
        ) -> repo::thread_replication::Result<Vec<u8>>,
    ) -> Result<(StateId, Vec<u8>), Error> {
        crate::hybrid::transfer_ready(&self.ready).map_err(Error::Invalid)?;
        let imported = self.import_authority();
        let native = self.native_authority();
        let (bundle, bundle_owner) = match (imported, native) {
            (Some(b), None) => (b.encode_to_vec(), b.owner_genesis.as_ref()),
            (None, Some(b)) => (b.encode_to_vec(), b.owner_genesis.as_ref()),
            _ => return Err(Error::HostedTrustRequired),
        };
        let spool = self
            .ready
            .thread
            .as_ref()
            .and_then(|t| t.spool.as_ref())
            .ok_or(Error::Invalid("Spool absent"))?
            .id
            .parse::<uuid::Uuid>()
            .map_err(preparation)?;
        let owner_genesis = self
            .ready
            .owner_genesis
            .as_ref()
            .ok_or(Error::Invalid("owner genesis absent"))?;
        let owner = self
            .ready
            .ownership
            .as_ref()
            .ok_or(Error::Invalid("owner history absent"))?;
        let selected =
            repo::verify_spool_owner_observation(owner_genesis, owner, spool, now_seconds)
                .map_err(preparation)?;
        if bundle_owner != Some(selected.owner_genesis().signed()) {
            return Err(api::hybrid_codec::Reject::Root.into());
        }
        let _write_lock = repository.locker().write().map_err(preparation)?;
        let next_pin = repository
            .prepare_owner_observation_pin(
                owner_genesis,
                owner,
                spool,
                &selected.wire().canonical_spool_path_segments,
                now_seconds,
            )
            .map_err(preparation)?;
        let previous_spool = read_optional(&repository.heddle_dir().join("spool-id"))?;
        if previous_spool.as_ref().is_some_and(|bytes| {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|s| s.trim().parse::<uuid::Uuid>().ok())
                != Some(spool)
        }) {
            return Err(api::hybrid_codec::Reject::Root.into());
        }
        let pin_path = repository.heddle_dir().join("owner-authorization.bin");
        let previous_pin = read_optional(&pin_path)?;
        let staging = tempfile::tempdir_in(self.directory.path())?;
        let staged_repo = Repository::init(staging.path()).map_err(preparation)?;
        self.install_source_objects(&staged_repo)?;
        // Native checkout comparison needs the immutable empty base. Keep its
        // tree/state in staging so every imported artifact shares the journal.
        let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()
            .map_err(preparation)?;
        staged_repo
            .store()
            .put_snapshot_objects_packed(Vec::new(), &objects::object::Tree::new(), &seed)
            .map_err(preparation)?;
        let main = self
            .ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("Thread genesis absent"))?;
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
        let mut records = Vec::new();
        for wrapper in std::iter::once(main).chain(&self.dependencies) {
            records.push(
                wrapper
                    .genesis
                    .clone()
                    .ok_or(Error::Invalid("original genesis absent"))?,
            );
            records.extend(wrapper.ownership_claims.clone());
            records.extend(wrapper.ownership_resolutions.clone());
        }
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            records.push(crate::contract::SignedRecord {
                format: heddle_object_model::object::thread_replication::OPERATION_FORMAT.into(),
                canonical_record: signed.canonical.clone(),
                signatures: vec![crate::contract::RecordSignature {
                    public_key: operation.publisher.to_vec(),
                    signature: signed.signature.clone(),
                }],
            });
        }
        if let Some(bundle) = native {
            for wrapper in std::iter::once(main).chain(&self.dependencies) {
                require_native_genesis_match(bundle, wrapper)?;
            }
        }
        let state = self.state.id();
        let publish = |artifacts: &mut InstallArtifacts<'_>| {
            // An owner/spool update while staging requires fresh preparation.
            if read_optional(&pin_path).map_err(replica_error)? != previous_pin
                || read_optional(&repository.heddle_dir().join("spool-id"))
                    .map_err(replica_error)?
                    != previous_spool
            {
                return Err(repo::thread_replication::Error::Hybrid(
                    api::hybrid_codec::Reject::StaleContext,
                ));
            }
            publish_store(staged_repo.heddle_dir(), artifacts)?;
            artifacts.write_file(Path::new("owner-authorization.bin"), &next_pin)?;
            artifacts.write_file(Path::new("spool-id"), spool.to_string().as_bytes())?;
            Ok(())
        };
        let (replicas, receipt) = if let Some(publication) = publication {
            let receipt = if native.is_some() {
                ThreadReplica::publish_native_source(
                    publication.replica,
                    trust,
                    &bundle,
                    &records,
                    authority,
                    staged_repo.store(),
                    publication.prepared,
                    publication.command,
                    |context, artifacts| {
                        publish(artifacts)?;
                        response(context)
                    },
                )
            } else {
                ThreadReplica::publish_hybrid_source(
                    publication.replica,
                    trust,
                    &bundle,
                    &records,
                    authority,
                    staged_repo.store(),
                    publication.prepared,
                    publication.command,
                    |context, artifacts| {
                        publish(artifacts)?;
                        response(context)
                    },
                )
            }
            .map_err(preparation)?;
            (Vec::new(), receipt)
        } else {
            let replicas = if native.is_some() {
                ThreadReplica::install_hybrid_native(
                    repository.heddle_dir(),
                    trust,
                    &bundle,
                    &records,
                    authority,
                    staged_repo.store(),
                    publish,
                )
            } else {
                ThreadReplica::install_hybrid_import(
                    repository.heddle_dir(),
                    trust,
                    &bundle,
                    &records,
                    authority,
                    staged_repo.store(),
                    publish,
                )
            }
            .map_err(preparation)?;
            (replicas, Vec::new())
        };
        repository.store().reload_packs().map_err(preparation)?;
        if !replicas.is_empty() {
            let selected = replicas
                .iter()
                .find(|r| r.thread_id() == main_id)
                .ok_or(Error::Invalid("selected replica absent"))?;
            if self.is_complete() {
                selected
                    .record_source_possession(state)
                    .map_err(preparation)?;
            }
        }
        if !replicas.is_empty() {
            // Account/device lookup uses the committed Spool identity. Registration
            // is local discovery after authority commit, while the repository lock
            // still prevents another writer from replacing the installed identity.
            repo::device_catalog::register(&repo::identity::heddle_home_dir(), repository, spool)
                .map_err(preparation)?;
        }
        Ok((state, receipt))
    }
}

fn require_native_genesis_match(
    bundle: &crate::contract::NativePublicProofBundleV1,
    wrapper: &crate::contract::ThreadGenesisRecord,
) -> Result<(), Error> {
    if !bundle.genesis_witnesses.iter().any(|p| {
        p.original_genesis == wrapper.genesis
            && p.creator_authority_envelope == wrapper.creator_authority
            && p.binding == wrapper.native_genesis_authority
    }) {
        return Err(api::hybrid_codec::Reject::GenesisBinding.into());
    }
    Ok(())
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    #[test]
    fn native_ready_binding_must_match_the_exact_witnessed_genesis() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native-host-witness-v1.json"
        ))
        .expect("vectors");
        let bundle: crate::contract::NativePublicProofBundleV1 = api::hybrid_codec::strict_decode(
            &hex::decode(
                fixture["wire_vectors"]["start_thread"]["wire_hex"]
                    .as_str()
                    .expect("wire"),
            )
            .expect("hex"),
            api::import_authority::MAX_BUNDLE_BYTES,
        )
        .expect("bundle");
        let p = &bundle.genesis_witnesses[0];
        let control = crate::contract::ThreadGenesisRecord {
            genesis: p.original_genesis.clone(),
            creator_authority: p.creator_authority_envelope.clone(),
            native_genesis_authority: p.binding.clone(),
            ..Default::default()
        };
        require_native_genesis_match(&bundle, &control).expect("exact Ready control");
        for field in 0..3 {
            let mut changed = control.clone();
            match field {
                0 => changed.genesis = None,
                1 => changed.creator_authority.push(0),
                _ => changed.native_genesis_authority = None,
            }
            assert!(
                matches!(
                    require_native_genesis_match(&bundle, &changed),
                    Err(Error::Hybrid(api::hybrid_codec::Reject::GenesisBinding))
                ),
                "Ready original, envelope and binding must match"
            );
        }
        require_native_genesis_match(&bundle, &control).expect("unchanged Ready control");
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn replica_error(error: impl std::fmt::Display) -> repo::thread_replication::Error {
    repo::thread_replication::Error::Invalid(error.to_string())
}

pub(crate) fn publish_store(
    staged: &Path,
    artifacts: &mut InstallArtifacts<'_>,
) -> repo::thread_replication::Result<()> {
    // Only immutable source storage crosses the boundary. Staging's refs,
    // identities, locks, configuration and local metadata never join the clone.
    for name in ["packs", "objects"] {
        publish_directory(&staged.join(name), Path::new(name), artifacts)?;
    }
    Ok(())
}
fn publish_directory(
    staged: &Path,
    destination: &Path,
    artifacts: &mut InstallArtifacts<'_>,
) -> repo::thread_replication::Result<()> {
    for entry in std::fs::read_dir(staged)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            // Pack install bookkeeping is local to the staging store.
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            publish_directory(
                &entry.path(),
                &destination.join(entry.file_name()),
                artifacts,
            )?;
        } else if entry.file_type()?.is_file() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            artifacts.install_file(&entry.path(), &destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use objects::{
        object::{State, Tree},
        store::{
            ObjectStore,
            pack::{ObjectType, PackBuilder, PackObjectId},
        },
    };
    use repo::thread_replication::hosted_trust::*;

    use super::*;
    use crate::{
        contract::*,
        hybrid::authority::{
            AcceptedHistory, SelectedAuthority,
            tests::{bundle, selected},
        },
    };

    struct ReceiverClock;
    impl Clock for ReceiverClock {
        fn now_millis(&self) -> repo::thread_replication::Result<i64> {
            Ok(1_350_000)
        }
        fn elapsed_millis(&self) -> repo::thread_replication::Result<u64> {
            Ok(0)
        }
    }
    fn record<T: Message + Default>(fixture: &serde_json::Value, name: &str) -> T {
        let vector = fixture["wire_vectors"]
            .get(name)
            .or_else(|| fixture["signed_vectors"].get(name))
            .expect("published vector");
        T::decode(
            hex::decode(vector["wire_hex"].as_str().expect("wire bytes"))
                .expect("hex")
                .as_slice(),
        )
        .expect("original record")
    }
    pub(crate) fn source(
        scratch: &Path,
        seed_only: bool,
    ) -> (
        StagedSource,
        RootSelection,
        heddleco_capability_verifier::VerifiedCloneKeyring,
    ) {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha27.json"))
                .expect("release fixture");
        let mut bundle = bundle();
        bundle.history_proofs = [
            "genesis_proof",
            "genesis_dev_proof",
            "publication_proof",
            "renewed_publication_proof",
        ]
        .map(|name| record(&fixture, name))
        .to_vec();
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = selected(&bundle, limits);
        let original: SignedRecord = record(&fixture, "converted_main");
        let signed = crate::replication::decode_record(original).expect("converted original");
        let operation = signed.verify().expect("original signature");
        let genesis_record = bundle
            .original_geneses
            .iter()
            .find(|record| {
                crypto::thread_operation::SignedGenesis {
                    canonical: record.canonical_record.clone(),
                    signature: record.signatures[0].signature.clone(),
                }
                .verify()
                .is_ok_and(|g| g.id().expect("genesis ID") == operation.thread)
            })
            .expect("selected genesis")
            .clone();
        let genesis = crypto::thread_operation::SignedGenesis {
            canonical: genesis_record.canonical_record.clone(),
            signature: genesis_record.signatures[0].signature.clone(),
        }
        .verify()
        .expect("genesis signature");
        let state: State = if seed_only {
            objects::object::thread_replication::hosted_import::synthetic_initial_base()
                .expect("empty seed")
        } else {
            operation.source_state().expect("source").expect("State")
        };
        let tree = Tree::new();
        assert_eq!(
            tree.hash(),
            state.tree,
            "published native conversion has the empty source tree"
        );
        let mut builder = PackBuilder::for_repack(Default::default(), 0);
        builder.add_id(
            PackObjectId::StateId(state.id()),
            ObjectType::State,
            state.encode_current_msgpack().expect("State"),
        );
        builder.add_id(
            PackObjectId::Hash(tree.hash()),
            ObjectType::Tree,
            tree.encode_canonical().expect("Tree"),
        );
        let (pack, index, _) = builder.build().expect("source pack");
        let directory = tempfile::tempdir_in(scratch).expect("staging");
        std::fs::write(directory.path().join("source.pack"), pack).expect("pack");
        std::fs::write(directory.path().join("source.idx"), index).expect("index");
        let spool = SpoolRef {
            id: genesis.spool.clone(),
        };
        let owner = pinned.owner_state();
        let ready = TransferReady {
            thread: Some(ThreadRef {
                spool: Some(spool.clone()),
                id: Some(ThreadId {
                    value: operation.thread.as_bytes().to_vec(),
                }),
            }),
            current: Some(RevisionRef {
                spool: Some(spool),
                revision: Some(revision_ref::Revision::State(
                    api::heddle::api::common::StateId {
                        value: state.id().as_bytes().to_vec(),
                    },
                )),
            }),
            thread_genesis: Some(ThreadGenesisRecord {
                creator_authority: bundle
                    .genesis_witnesses
                    .iter()
                    .find(|p| p.original_genesis.as_ref() == Some(&genesis_record))
                    .expect("selected creator authority")
                    .creator_authority_envelope
                    .clone(),
                genesis: Some(genesis_record),
                ..Default::default()
            }),
            owner_genesis: bundle.owner_genesis.clone(),
            ownership: Some(OwnerState {
                owner: Some(PrincipalRef {
                    id: uuid::Uuid::from_bytes(
                        owner
                            .signed_root()
                            .root
                            .as_ref()
                            .expect("root")
                            .account_uuid
                            .as_slice()
                            .try_into()
                            .expect("UUID"),
                    )
                    .to_string(),
                }),
                root: Some(owner.signed_root().clone()),
                accepted_transitions: pinned.wire().accepted_transitions.clone(),
                version: owner.state_hash().to_vec(),
                resource_keyring: Some(pinned.wire().clone()),
                ..Default::default()
            }),
            full_closure_available: true,
            import_authority: Some(bundle),
            protocol: Some(crate::hybrid::protocol()),
            ..Default::default()
        };
        let staged = super::super::staging::validate_with_receipts(
            directory,
            ready,
            if seed_only { vec![] } else { vec![signed] },
            vec![],
            vec![],
        )
        .expect("selected structural source");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(
                fixture["keys"]["root"]["public_key_hex"]
                    .as_str()
                    .expect("root"),
            )
            .expect("hex")
            .try_into()
            .expect("key"),
        };
        (staged, root, pinned)
    }
    fn artifacts(path: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn walk(root: &Path, path: &Path, values: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
            if !path.exists() {
                return;
            }
            for entry in std::fs::read_dir(path).expect("directory") {
                let entry = entry.expect("entry");
                if entry.file_type().expect("type").is_dir() {
                    walk(root, &entry.path(), values);
                } else {
                    values.insert(
                        entry
                            .path()
                            .strip_prefix(root)
                            .expect("relative")
                            .to_path_buf(),
                        std::fs::read(entry.path()).expect("bytes"),
                    );
                }
            }
        }
        let mut values = BTreeMap::new();
        for name in ["objects", "packs"] {
            walk(path, &path.join(name), &mut values);
        }
        for name in ["owner-authorization.bin", "spool-id"] {
            if let Ok(bytes) = std::fs::read(path.join(name)) {
                values.insert(name.into(), bytes);
            }
        }
        values
    }
    #[test]
    fn selected_hosted_capture_and_genesis_only_install_without_sibling_branch() {
        for seed_only in [false, true] {
            let scratch = tempfile::tempdir().expect("scratch");
            let directory = tempfile::tempdir().expect("receiver");
            let repo = Repository::init(directory.path()).expect("repository");
            let (staged, root, pinned) = source(scratch.path(), seed_only);
            let state = staged.state().id();
            let selected_thread = staged
                .ready()
                .thread
                .as_ref()
                .expect("Thread")
                .id
                .as_ref()
                .expect("ID")
                .value
                .clone();
            let bundle = staged.import_authority().expect("public history").clone();
            let history = AcceptedHistory::from_selected_spool(
                &bundle,
                &pinned,
                1350,
                heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
                    .expect("limits"),
            )
            .expect("selected history");
            select_root(repo.heddle_dir(), &root).expect("root pin");
            select_spool(
                repo.heddle_dir(),
                pinned.owner_genesis().spool_uuid(),
                *history.genesis(),
                *history.initial_owner(),
            )
            .expect("Spool selection");
            let trust = HostedTrust::open(repo.heddle_dir(), &root.authority, ReceiverClock)
                .expect("trust");
            let authority = SelectedAuthority::new(
                history,
                bundle.clone(),
                |_: &ImportPublicProofBundleV1,
                 _: i64,
                 _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| {
                    Ok(())
                },
            );
            assert_eq!(
                staged
                    .install_hosted(&repo, &trust, &authority, 1350)
                    .expect("selected hosted install"),
                state
            );
            assert!(repo.heddle_dir().join("spool-id").exists());
            assert!(repo.heddle_dir().join("owner-authorization.bin").exists());
            let (_, retained_owner) = repo
                .pinned_owner_observation(1350)
                .expect("independently retained owner observation");
            assert_eq!(
                retained_owner.owner_genesis().signed(),
                pinned.owner_genesis().signed()
            );
            assert!(
                repo.store()
                    .get_state(&state)
                    .expect("stored State")
                    .is_some()
            );
            for record in &bundle.original_geneses {
                let genesis = crypto::thread_operation::SignedGenesis {
                    canonical: record.canonical_record.clone(),
                    signature: record.signatures[0].signature.clone(),
                }
                .verify()
                .expect("original genesis");
                let id = genesis.id().expect("ID");
                if id.as_bytes().as_slice() == selected_thread {
                    assert_eq!(
                        ThreadReplica::open(repo.heddle_dir(), id)
                            .expect("selected replica")
                            .hybrid_import_bundle()
                            .expect("retained bundle"),
                        Some(bundle.clone())
                    );
                } else {
                    assert!(
                        ThreadReplica::open(repo.heddle_dir(), id).is_err(),
                        "public sibling proof must not install its native branch"
                    );
                }
            }
        }
    }
    #[test]
    fn late_disclosure_failure_rolls_back_pack_owner_pin_spool_and_trust() {
        let scratch = tempfile::tempdir().expect("scratch");
        let directory = tempfile::tempdir().expect("receiver");
        let repo = Repository::init(directory.path()).expect("repository");
        let before = artifacts(repo.heddle_dir());
        let (staged, root, pinned) = source(scratch.path(), false);
        let bundle = staged.import_authority().expect("history").clone();
        let history = AcceptedHistory::from_selected_spool(
            &bundle,
            &pinned,
            1350,
            heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
                .expect("limits"),
        )
        .expect("selected history");
        select_root(repo.heddle_dir(), &root).expect("root");
        select_spool(
            repo.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *history.genesis(),
            *history.initial_owner(),
        )
        .expect("Spool");
        let trust =
            HostedTrust::open(repo.heddle_dir(), &root.authority, ReceiverClock).expect("trust");
        let calls = AtomicUsize::new(0);
        let authority = SelectedAuthority::new(
            history,
            bundle,
            |_: &ImportPublicProofBundleV1,
             _: i64,
             _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| {
                if calls.fetch_add(1, Ordering::SeqCst) > 1 {
                    assert!(
                        repo.heddle_dir().join("owner-authorization.bin").exists(),
                        "failure must occur after staged artifacts were installed"
                    );
                    assert!(repo.heddle_dir().join("spool-id").exists());
                    return Err(repo::thread_replication::Error::Hybrid(
                        api::hybrid_codec::Reject::Expired,
                    ));
                }
                Ok(())
            },
        );
        assert!(
            staged
                .install_hosted(&repo, &trust, &authority, 1350)
                .is_err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "commit must recheck current disclosure after the file callback"
        );
        assert_eq!(
            artifacts(repo.heddle_dir()),
            before,
            "late rejection must restore exact destination artifacts"
        );
        assert!(
            trust
                .snapshot()
                .expect("rolled back trust")
                .previous
                .is_none()
        );
    }

    #[tokio::test]
    async fn hosted_native_relay_retains_bundle_and_rechecks_revoked_durable_context() {
        use std::sync::Arc;

        use crate::replication::{
            native::LocalReplica,
            store::{ReceivedOperation, ReplicaStore},
        };
        let scratch = tempfile::tempdir().expect("scratch");
        let directory = tempfile::tempdir().expect("receiver");
        let repo = Repository::init(directory.path()).expect("repository");
        let (staged, root, pinned) = source(scratch.path(), true);
        let bundle = staged.import_authority().expect("history").clone();
        let history = AcceptedHistory::from_selected_spool(
            &bundle,
            &pinned,
            1350,
            heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
                .expect("limits"),
        )
        .expect("selected history");
        select_root(repo.heddle_dir(), &root).expect("root");
        select_spool(
            repo.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *history.genesis(),
            *history.initial_owner(),
        )
        .expect("Spool");
        let trust = Arc::new(
            HostedTrust::open(repo.heddle_dir(), &root.authority, ReceiverClock).expect("trust"),
        );
        let authority = Arc::new(SelectedAuthority::new(
            history,
            bundle.clone(),
            |_: &ImportPublicProofBundleV1,
             _: i64,
             _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
        ));
        staged
            .install_hosted(&repo, &trust, authority.as_ref(), 1350)
            .expect("genesis-only receiver");
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha27.json"))
                .expect("vectors");
        let original = crate::replication::decode_record(record(&fixture, "converted_main"))
            .expect("original conversion");
        let operation = original.verify().expect("original signature");
        let id = operation.id().expect("ID");
        let replica =
            ThreadReplica::open(repo.heddle_dir(), operation.thread).expect("selected replica");
        let local = LocalReplica::new(replica, Arc::new(repo.store().clone()));
        let relay = local.clone().with_hosted_authority(
            repo.heddle_dir().to_path_buf(),
            trust.clone(),
            authority,
        );
        assert!(
            relay
                .receive(ReceivedOperation {
                    native_authority: None,
                    original: original.clone(),
                    authority_admission: None,
                    import_authority: None,
                })
                .await
                .is_err(),
            "receive cannot strip the required import history"
        );
        assert_eq!(
            relay
                .receive(ReceivedOperation {
                    native_authority: None,
                    original: original.clone(),
                    authority_admission: None,
                    import_authority: Some(Arc::new(bundle.clone()))
                })
                .await
                .expect("independently verified native receive"),
            heddle_object_model::object::thread_replication::Admission::Accepted
        );
        assert!(
            local.operation(id).await.is_err(),
            "an unconfigured relay must never strip retained HYBRID authority"
        );
        let (received, _) = relay
            .operation(id)
            .await
            .expect("fresh relay admission")
            .expect("original");
        assert_eq!(received.original, original);
        assert_eq!(received.import_authority.as_deref(), Some(&bundle));
        let revoked = record(&fixture, "revoked_set");
        trust
            .mutate(&revoked, |_| Ok(()))
            .expect("independently persist N+1 revocation");
        assert!(
            matches!(
                relay.operation(id).await,
                Err(crate::replication::native::Error::Store(
                    repo::thread_replication::Error::Hybrid(api::hybrid_codec::Reject::HighWater)
                ))
            ),
            "export cannot revive retained N after durable N+1 revocation"
        );
    }
}
