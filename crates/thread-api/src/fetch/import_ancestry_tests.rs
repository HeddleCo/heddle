//! A fresh clone of a HYBRID-imported Thread must receive the converted Git
//! history, not only the import tip (HeddleCo/heddle#2004, api alpha.42).
//!
//! The fixture re-signs the alpha.33 import for a tip State with real Git-style
//! ancestors: the job key signs the converted operation and the import
//! certificate, and the active witness signs the publication statement. The
//! rest of the public proof (owner, delegation, genesis authority, policies,
//! witness set) is the frozen fixture.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use api::{hybrid_codec, import_authority as contract, witness_trust};
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use objects::object::{
    Attribution, Blob, ContentHash, Principal, State, StateId, Tree, TreeEntry,
    thread_replication::{AuthoredCapture, ThreadOperation, ThreadOperationBody},
};
use repo::{
    Repository,
    thread_replication::{
        hosted_trust::{HostedTrust, RootSelection, TrustTransaction, select_root, select_spool},
        import_floor::ImportFloorRole,
    },
};

use super::{
    super::{ancestry::AncestryInput, staging::validate_with_receipts_and_carriers},
    Error, StagedSource,
    tests::ReceiverClock,
};
use crate::{
    contract::*,
    hybrid::authority::{
        AcceptedHistory, SelectedAuthority,
        tests::{bundle, selected},
    },
};

const NOW_MS: i64 = 1_350_000;

fn fixture_json() -> serde_json::Value {
    serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha33.json"))
        .expect("release fixture")
}
fn signer(fixture: &serde_json::Value, role: &str) -> Ed25519Signer {
    Ed25519Signer::from_seed(
        &hex::decode(fixture["keys"][role]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("signer")
}
fn file_tree(name: &str, content: &[u8], salt: u8) -> (Tree, Blob) {
    let blob = Blob::new(content.to_vec());
    let tree = Tree::from_entries_salted_v4(
        vec![TreeEntry::file(name, blob.hash(), false).expect("entry")],
        vec![[salt; 32]],
    )
    .expect("salted tree");
    (tree, blob)
}

/// A converted linear Git history: `chain[0]` is the root commit and
/// `chain[ancestors]` the tip. The tip and `older` carry real file trees.
struct ImportedHistory {
    bundle: ImportPublicProofBundleV1,
    signed: SignedOperation,
    thread: ContentHash,
    chain: Vec<State>,
    trees: BTreeMap<StateId, (Tree, Blob)>,
    digest: Vec<u8>,
    pinned: heddleco_capability_verifier::VerifiedCloneKeyring,
    root: RootSelection,
}
impl ImportedHistory {
    fn tip(&self) -> &State {
        self.chain.last().expect("tip")
    }
    fn ancestors(&self) -> &[State] {
        &self.chain[..self.chain.len() - 1]
    }
}

fn imported_history(ancestors: usize, older: usize) -> ImportedHistory {
    let fixture = fixture_json();
    let job = signer(&fixture, "job");
    let witness = signer(&fixture, "next_witness");
    let mut bundle = bundle();
    // Every statement is re-signed by the active witness below, so no
    // retired-witness history proof applies.
    bundle.history_proofs.clear();
    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let pinned = selected(&bundle, limits);
    let main = bundle
        .operations
        .iter()
        .position(|o| {
            o.body
                .as_ref()
                .is_some_and(|b| b.ref_name == "refs/heads/main")
        })
        .expect("main branch result");
    let mut signed_import = bundle.operations[main].clone();
    let old_digest = contract::signed_operation_digest(&signed_import).expect("old digest");
    // Every publication statement names its (operation, cumulative manifest)
    // through the exact payload; the main slot changes in every manifest, so
    // every P3 is re-signed by the active witness against the new manifests.
    let mut publications = Vec::new();
    for (statement_index, statement) in bundle.statements.iter().enumerate() {
        let body = statement.body.as_ref().expect("body");
        if body.purpose != 3 {
            continue;
        }
        let pair = bundle
            .operations
            .iter()
            .enumerate()
            .find_map(|(operation_index, operation)| {
                bundle
                    .manifests
                    .iter()
                    .position(|manifest| {
                        hybrid_codec::canonical(
                            &contract::publication_payload(operation, manifest).expect("payload"),
                        )
                        .expect("canonical")
                            == body.canonical_payload
                    })
                    .map(|manifest_index| (operation_index, manifest_index))
            })
            .expect("published operation and manifest");
        publications.push((statement_index, pair.0, pair.1));
    }
    assert_eq!(publications.len(), 2, "main and dev publications");
    let thread = ContentHash::from_bytes(
        signed_import
            .body
            .as_ref()
            .expect("body")
            .target_thread_id
            .as_slice()
            .try_into()
            .expect("thread"),
    );
    // Converted history. Distinct intents keep every State's identity unique.
    let mut chain = Vec::new();
    let mut trees = BTreeMap::new();
    for index in 0..=ancestors {
        let parents = chain
            .last()
            .map(|s: &State| vec![s.id()])
            .unwrap_or_default();
        let tree_hash = if index == ancestors {
            let (tree, blob) = file_tree("README.md", b"converted tip\n", 0x51);
            let hash = tree.hash();
            (
                hash,
                trees.insert(StateId::from_bytes([0; 32]), (tree, blob)),
            )
                .0
        } else if index == older {
            let (tree, blob) = file_tree("OLD.txt", b"an older converted commit\n", 0x52);
            let hash = tree.hash();
            (
                hash,
                trees.insert(StateId::from_bytes([1; 32]), (tree, blob)),
            )
                .0
        } else {
            Tree::new().hash()
        };
        let state = State::new_snapshot(
            tree_hash,
            parents,
            Attribution::human(Principal::new("git author", "author@example.test")),
        )
        .with_intent(format!("converted commit {index}"));
        if let Some(placeholder) = trees.remove(&StateId::from_bytes([0; 32])) {
            trees.insert(state.id(), placeholder);
        }
        if let Some(placeholder) = trees.remove(&StateId::from_bytes([1; 32])) {
            trees.insert(state.id(), placeholder);
        }
        chain.push(state);
    }
    let tip = chain.last().expect("tip").clone();
    let capture = AuthoredCapture::local(tip.encode_current_msgpack().expect("State").into());
    let canonical_capture = rmp_serde::to_vec_named(&capture.result).expect("capture");
    let operation = ThreadOperation {
        version: 1,
        thread,
        parents: BTreeSet::new(),
        publisher: job.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(capture),
    };
    let signed = SignedOperation::sign(&operation, &job).expect("converted original");
    let operation_id = operation.id().expect("id");
    let body = signed_import.body.as_mut().expect("body");
    body.resulting_frontier_digest = contract::frontier_digest(&ImportFrontierV1 {
        format_version: 1,
        thread_id: thread.as_bytes().to_vec(),
        operation_ids: vec![operation_id.as_bytes().to_vec()],
    })
    .expect("frontier");
    body.resulting_content_digest = contract::content_digest(&ImportContentV1 {
        format_version: 1,
        canonical_capture: canonical_capture.clone(),
    })
    .expect("content");
    body.result_bytes = canonical_capture.len() as u64;
    let job_signature = job
        .sign(&hybrid_codec::signing_digest(contract::OPERATION_DOMAIN, body).expect("digest"))
        .expect("job signature");
    signed_import.job_signature = Some(AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(job.public_key()),
        signature: job_signature.clone(),
    });
    let resulting_frontier_digest = body.resulting_frontier_digest.clone();
    let result_bytes = body.result_bytes;
    let digest = contract::signed_operation_digest(&signed_import).expect("digest");
    bundle.operations[main] = signed_import.clone();
    for manifest in bundle
        .manifests
        .iter_mut()
        .chain(bundle.terminal_manifest.iter_mut())
    {
        for slot in &mut manifest.slots {
            if slot.signed_operation_digest == old_digest {
                slot.signed_operation_digest = digest.clone();
                slot.resulting_frontier_digest = resulting_frontier_digest.clone();
                slot.result_bytes = result_bytes;
            }
        }
    }
    for (statement_index, operation_index, manifest_index) in publications {
        let operation = bundle.operations[operation_index].clone();
        let manifest = bundle.manifests[manifest_index].clone();
        let statement_body = bundle.statements[statement_index]
            .body
            .as_mut()
            .expect("body");
        statement_body.canonical_payload = hybrid_codec::canonical(
            &contract::publication_payload(&operation, &manifest).expect("payload"),
        )
        .expect("canonical");
        if operation_index == main {
            statement_body.original_signatures_digest = hybrid_codec::hash(&[&job_signature]);
        }
    }
    // Each branch's genesis admission (P1) and publication (P3) must share one
    // witness and host transaction. Every statement keeps its witnessed time
    // inside the delegation's [1000 s, 1300 s) window; only the signer moves
    // to the active witness, whose statements need no history proof.
    for statement in &mut bundle.statements {
        let statement_body = statement.body.as_mut().expect("body");
        statement_body.executor_id = witness_trust::witness_id(witness.public_key());
        statement.signature = witness
            .sign(&witness_trust::statement_signing_digest(statement_body).expect("statement"))
            .expect("witness signature");
    }
    // The fixture's active witness only starts at 1300 s, after the delegation
    // window closes, and its retired predecessor needs history proofs this
    // test does not mint. Widen the active entry back over the window under
    // the same deployment root; the receiver pins that root independently.
    let set = bundle.witness_set.as_mut().expect("witness set");
    let set_body = set.body.as_mut().expect("body");
    for entry in &mut set_body.entries {
        if entry.state == 1 {
            entry.active_from_unix_millis = 1_000_000;
        }
    }
    let set_bytes = witness_trust::set_signing_bytes(set_body).expect("set bytes");
    set.body_digest = hybrid_codec::hash(&[&set_bytes]);
    set.root_signature = signer(&fixture, "root")
        .sign(&set_bytes)
        .expect("root signature");
    bundle
        .manifests
        .sort_by_key(|m| contract::manifest_digest(m).expect("manifest digest"));
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    api::import_authority::validate_public_bundle(&bundle).expect("complete public proof");
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
    ImportedHistory {
        bundle,
        signed,
        thread,
        chain,
        trees,
        digest,
        pinned,
        root,
    }
}

fn page_set(
    history: &ImportedHistory,
    states: &[State],
    coverage: import_ancestry_page::Coverage,
    page_size: usize,
) -> Vec<ImportAncestryPage> {
    let pages = states.chunks(page_size).collect::<Vec<_>>();
    pages
        .iter()
        .enumerate()
        .map(|(index, chunk)| ImportAncestryPage {
            floor_tiers: Some(ImportFloorTierSummary::default()),
            thread: Some(ThreadRef {
                spool: Some(SpoolRef {
                    id: spool_id(history),
                }),
                id: Some(ThreadId {
                    value: history.thread.as_bytes().to_vec(),
                }),
            }),
            tip: Some(api::heddle::api::common::StateId {
                value: history.tip().id().as_bytes().to_vec(),
            }),
            signed_operation_digest: history.digest.clone(),
            coverage: coverage as i32,
            page_index: index as u32,
            page_count: pages.len() as u32,
            member_count: states.len() as u32,
            states: chunk
                .iter()
                .map(|state| ImportAncestorState {
                    id: Some(api::heddle::api::common::StateId {
                        value: state.id().as_bytes().to_vec(),
                    }),
                    canonical_state: state.encode_current_msgpack().expect("State"),
                })
                .collect(),
        })
        .collect()
}
fn spool_id(history: &ImportedHistory) -> String {
    uuid::Uuid::from_bytes(history.pinned.owner_genesis().spool_uuid()).to_string()
}

struct Transfer {
    ready: TransferReady,
    directory: tempfile::TempDir,
    carriers: crypto::import_authority::VerifiedImportCarriers,
}
/// A Fetch of `selected` (the tip or an older converted commit): the source
/// pack holds exactly that State's tree closure, nothing older or newer.
fn transfer(history: &ImportedHistory, scratch: &Path, selected: &State) -> Transfer {
    use objects::store::pack::{ObjectType, PackBuilder, PackObjectId};
    let (tree, blob) = history.trees.get(&selected.id()).expect("selected tree");
    let mut builder = PackBuilder::for_repack(Default::default(), 0);
    builder.add_id(
        PackObjectId::StateId(selected.id()),
        ObjectType::State,
        selected.encode_current_msgpack().expect("State"),
    );
    builder.add_id(
        PackObjectId::Hash(tree.hash()),
        ObjectType::Tree,
        tree.encode_canonical().expect("tree"),
    );
    builder.add_id(
        PackObjectId::Hash(blob.hash()),
        ObjectType::Blob,
        blob.clone().into_content(),
    );
    let (pack, index, _) = builder.build().expect("source pack");
    let directory = tempfile::tempdir_in(scratch).expect("staging");
    std::fs::write(directory.path().join("source.pack"), pack).expect("pack");
    std::fs::write(directory.path().join("source.idx"), index).expect("index");
    let bundle = &history.bundle;
    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let accepted = AcceptedHistory::from_selected_spool(bundle, &history.pinned, 1350, limits)
        .expect("verified import owner history");
    let authority = SelectedAuthority::new(
        accepted,
        bundle.clone(),
        |_: &ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let pin = api::import_authority::ImportWitnessRootPin {
        authority: history.root.authority.clone(),
        root_id: history.root.root_id.clone(),
        public_key: history.root.public_key.to_vec(),
        epoch: 1,
    };
    let carriers = repo::thread_replication::delegated_import::authenticate_import_carriers(
        bundle,
        &authority,
        &pin,
        NOW_MS,
        &[],
        &[],
        |_| Ok(()),
    )
    .expect("independently authenticated import carriers");
    let genesis_record = bundle
        .original_geneses
        .iter()
        .find(|record| {
            crypto::thread_operation::SignedGenesis {
                canonical: record.canonical_record.clone(),
                signature: record.signatures[0].signature.clone(),
            }
            .verify()
            .is_ok_and(|g| g.id().expect("genesis ID") == history.thread)
        })
        .expect("selected genesis")
        .clone();
    let owner = history.pinned.owner_state();
    let spool = SpoolRef {
        id: spool_id(history),
    };
    let ready = TransferReady {
        thread: Some(ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId {
                value: history.thread.as_bytes().to_vec(),
            }),
        }),
        current: Some(RevisionRef {
            spool: Some(spool),
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::common::StateId {
                    value: selected.id().as_bytes().to_vec(),
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
            accepted_transitions: history.pinned.wire().accepted_transitions.clone(),
            version: owner.state_hash().to_vec(),
            resource_keyring: Some(history.pinned.wire().clone()),
            ..Default::default()
        }),
        full_closure_available: true,
        import_authority: Some(bundle.clone()),
        protocol: Some(crate::hybrid::protocol()),
        ..Default::default()
    };
    Transfer {
        ready,
        directory,
        carriers,
    }
}
fn stage(
    history: &ImportedHistory,
    transfer: Transfer,
    pages: Vec<ImportAncestryPage>,
    excluded_tips: BTreeSet<StateId>,
) -> Result<StagedSource, Error> {
    let mut ancestry = AncestryInput::new(excluded_tips);
    for page in pages {
        ancestry.push(
            page,
            transfer.ready.thread.as_ref().expect("thread"),
            transfer.directory.path(),
        )?;
    }
    ancestry.finish()?;
    validate_with_receipts_and_carriers(
        transfer.directory,
        transfer.ready,
        vec![history.signed.clone()],
        vec![],
        vec![],
        Some(transfer.carriers),
        ancestry,
    )
}
fn receiver(history: &ImportedHistory, directory: &Path) -> Repository {
    let repository = Repository::init(directory).expect("repository");
    select_root(repository.heddle_dir(), &history.root).expect("root pin");
    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let accepted =
        AcceptedHistory::from_selected_spool(&history.bundle, &history.pinned, 1350, limits)
            .expect("history");
    select_spool(
        repository.heddle_dir(),
        history.pinned.owner_genesis().spool_uuid(),
        *accepted.genesis(),
        *accepted.initial_owner(),
    )
    .expect("Spool selection");
    repository
}
fn install(history: &ImportedHistory, repository: &Repository, staged: StagedSource) -> StateId {
    try_install(history, repository, staged).expect("hosted install")
}
fn try_install(
    history: &ImportedHistory,
    repository: &Repository,
    staged: StagedSource,
) -> Result<StateId, Error> {
    let trust = HostedTrust::open(
        repository.heddle_dir(),
        &history.root.authority,
        ReceiverClock,
    )
    .expect("trust");
    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let accepted =
        AcceptedHistory::from_selected_spool(&history.bundle, &history.pinned, 1350, limits)
            .expect("history");
    let authority = SelectedAuthority::new(
        accepted,
        history.bundle.clone(),
        |_: &ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    staged.install_hosted(repository, &trust, &authority, 1350)
}
fn checkout_files(repository: &Repository, state: &State, dest: &Path) -> Vec<String> {
    let outcome = repository
        .checkout_state_gated(&state.id(), state, dest, &repo::AudienceTier::Internal)
        .expect("materialize");
    assert!(
        matches!(outcome, repo::CheckoutMaterialization::Materialized { .. }),
        "visible converted history materializes its real tree: {outcome:?}"
    );
    let mut names = std::fs::read_dir(dest)
        .expect("checkout")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn fresh_clone_installs_the_converted_history_and_an_older_commit_fetches_lazily() {
    const ANCESTORS: usize = 64;
    const OLDER: usize = 20;
    let history = imported_history(ANCESTORS, OLDER);
    let scratch = tempfile::tempdir().expect("scratch");
    let receiver_dir = tempfile::tempdir().expect("receiver");
    let repository = receiver(&history, receiver_dir.path());
    let tip = history.tip().clone();

    // 1. Fresh clone: the floor travels in three pages (odd page size so the
    //    last page is partial), every ancestor installs, the walk stops at the
    //    floor members, and the checkout holds the real file.
    let pages = page_set(
        &history,
        history.ancestors(),
        import_ancestry_page::Coverage::Floor,
        25,
    );
    assert_eq!(pages.len(), 3);
    let staged = stage(
        &history,
        transfer(&history, scratch.path(), &tip),
        pages,
        BTreeSet::new(),
    )
    .expect("floor verifies against the signed tip");
    let floors = staged.verified_import_floors().collect::<Vec<_>>();
    assert_eq!(floors.len(), 1);
    assert_eq!(floors[0].0, tip.id());
    assert_eq!(floors[0].1.len(), ANCESTORS);
    assert_eq!(install(&history, &repository, staged), tip.id());
    use objects::store::ObjectStore;
    for state in history.ancestors() {
        assert_eq!(
            repository
                .store()
                .get_state(&state.id())
                .expect("read")
                .as_ref()
                .map(State::id),
            Some(state.id()),
            "every converted ancestor is installed"
        );
    }
    assert_eq!(
        repository
            .import_floor_role(history.thread, &tip.id())
            .expect("role")
            .expect("recorded"),
        ImportFloorRole::Tip
    );
    assert_eq!(
        repository
            .import_floor_role(history.thread, &history.chain[OLDER].id())
            .expect("role"),
        Some(ImportFloorRole::Member { tip: tip.id() })
    );
    let replica =
        repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), history.thread)
            .expect("replica");
    assert_eq!(replica.import_floor_tips().expect("tips"), vec![tip.id()]);
    assert_eq!(
        repository
            .withholding_visibility_for_audience(&tip.id(), &repo::AudienceTier::Internal)
            .expect("walk"),
        None,
        "a complete converted history is not an unresolved private ancestor"
    );
    let checkout = tempfile::tempdir().expect("checkout");
    assert_eq!(
        checkout_files(&repository, &tip, checkout.path()),
        ["README.md"],
        "no HEDDLE-EMBARGO.txt placeholder"
    );
    assert_eq!(
        std::fs::read(checkout.path().join("README.md")).expect("file"),
        b"converted tip\n"
    );

    // 2. Lazy fetch of an older converted commit: the client excludes the tip
    //    it holds, the endpoint proves membership with the parent chain from
    //    the tip down to the commit, and the pack carries that commit's tree.
    let older = history.chain[OLDER].clone();
    let path = history.chain[OLDER..ANCESTORS]
        .iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(path.len(), ANCESTORS - OLDER);
    let staged = stage(
        &history,
        transfer(&history, scratch.path(), &older),
        page_set(&history, &path, import_ancestry_page::Coverage::Path, 4096),
        BTreeSet::from([tip.id()]),
    )
    .expect("an older commit stages through its parent chain proof");
    assert_eq!(staged.state().id(), older.id());
    assert_eq!(staged.verified_import_floors().count(), 0);
    assert_eq!(install(&history, &repository, staged), older.id());
    assert!(
        replica
            .has_source_possession(older.id())
            .expect("possession"),
        "a converted ancestor records possession like any source revision"
    );
    let checkout = tempfile::tempdir().expect("older checkout");
    assert_eq!(
        checkout_files(&repository, &older, checkout.path()),
        ["OLD.txt"]
    );

    // 3. A later pull of the same Thread omits the floor the client already
    //    recorded; staging accepts the declared exclusion and install keeps
    //    the earlier record.
    let staged = stage(
        &history,
        transfer(&history, scratch.path(), &tip),
        Vec::new(),
        BTreeSet::from([tip.id()]),
    )
    .expect("a recorded floor need not travel again");
    assert_eq!(install(&history, &repository, staged), tip.id());
    assert_eq!(replica.import_floor_tips().expect("tips"), vec![tip.id()]);
}

#[test]
fn native_continuation_on_an_imported_tip_publishes_without_ancestry() {
    let mut history = imported_history(8, 2);
    let author = signer(&fixture_json(), "job");
    let (tree, blob) = file_tree("README.md", b"native continuation\n", 0x53);
    let state = State::new_snapshot(
        tree.hash(),
        vec![history.tip().id()],
        Attribution::human(Principal::new("native author", "author@example.test")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: history.thread,
        parents: BTreeSet::from([history
            .signed
            .verify()
            .expect("import operation")
            .id()
            .expect("id")]),
        publisher: author.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(AuthoredCapture::local(
            state.encode_current_msgpack().expect("State").into(),
        )),
    };
    let signed = SignedOperation::sign(&operation, &author).expect("native original");
    history.trees.insert(state.id(), (tree, blob));
    let scratch = tempfile::tempdir().expect("scratch");
    let transfer = transfer(&history, scratch.path(), &state);
    let packs = [
        ("source.pack", pack_extent::Kind::NativePack),
        ("source.idx", pack_extent::Kind::NativeIndex),
    ]
    .into_iter()
    .map(|(name, kind)| {
        let data = std::fs::read(transfer.directory.path().join(name)).expect("artifact");
        let address = ObjectAddress {
            algorithm: "blake3".into(),
            digest: blake3::hash(&data).as_bytes().to_vec(),
        };
        PackExtent {
            pack: Some(address.clone()),
            kind: kind as i32,
            offset: 0,
            length: data.len() as u64,
            extent_digest: Some(address),
        }
    })
    .collect();
    let opening = PublishContentOpen {
        thread: transfer.ready.thread,
        revision: transfer.ready.current,
        packs,
        import_authority: Some(history.bundle.clone()),
        protocol: Some(crate::hybrid::protocol()),
        ..Default::default()
    };
    let operations = [history.signed, signed]
        .into_iter()
        .map(|signed| SignedRecord {
            format: objects::object::thread_replication::OPERATION_FORMAT.into(),
            signatures: vec![RecordSignature {
                public_key: signed.verify().expect("operation").publisher.to_vec(),
                signature: signed.signature,
            }],
            canonical_record: signed.canonical,
        })
        .collect();
    let originals = crate::publication::PublicationOriginals {
        geneses: vec![transfer.ready.thread_genesis.expect("genesis")],
        operations: vec![ReplicationOperations {
            operations,
            import_authority: opening.import_authority.clone(),
            ..Default::default()
        }],
    };
    assert!(!transfer.directory.path().join("ancestry.pack").exists());
    let validated = crate::publication::validate_source_artifacts_with_import_carriers(
        transfer.directory,
        &opening,
        originals,
        transfer.carriers,
    )
    .expect("publication retains the import proof without requiring its ancestry pages");
    assert_eq!(validated.state().id(), state.id());
    assert_eq!(validated.operations().len(), 2);
    assert_eq!(
        validated.import_authority(),
        opening.import_authority.as_ref()
    );
}

#[test]
fn a_converted_history_without_its_ancestry_does_not_stage() {
    // The pre-alpha.42 shape: only the tip travels. This is the exact failure
    // weft#2617 observed as an embargo placeholder; it must now fail closed
    // before anything installs instead of settling with a tip-only clone.
    let history = imported_history(8, 2);
    let scratch = tempfile::tempdir().expect("scratch");
    let error = stage(
        &history,
        transfer(&history, scratch.path(), history.tip()),
        Vec::new(),
        BTreeSet::new(),
    )
    .err()
    .expect("tip without converted ancestors");
    assert!(
        error
            .to_string()
            .contains("import ancestry absent for a converted Git history"),
        "{error}"
    );
}

#[test]
fn ancestry_pages_reject_tampered_missing_extra_and_misattributed_states() {
    const ANCESTORS: usize = 16;
    let history = imported_history(ANCESTORS, 3);
    let scratch = tempfile::tempdir().expect("scratch");
    let floor = |page_size: usize| {
        page_set(
            &history,
            history.ancestors(),
            import_ancestry_page::Coverage::Floor,
            page_size,
        )
    };
    let attempt = |pages: Vec<ImportAncestryPage>| {
        stage(
            &history,
            transfer(&history, scratch.path(), history.tip()),
            pages,
            BTreeSet::new(),
        )
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
    };
    // Control: the untouched pages verify.
    stage(
        &history,
        transfer(&history, scratch.path(), history.tip()),
        floor(5),
        BTreeSet::new(),
    )
    .expect("control floor verifies");

    // A tampered State: bytes changed under an unchanged address.
    let mut pages = floor(5);
    let victim = &mut pages[1].states[2];
    let mut tampered = State::decode_current_msgpack(&victim.canonical_state).expect("State");
    tampered.intent = Some("rewritten history".into());
    victim.canonical_state = tampered.encode_current_msgpack().expect("bytes");
    assert!(
        attempt(pages).contains("differs from its address"),
        "tampered State"
    );

    // A missing middle State: the chain from the tip cannot reach the root.
    let mut pages = floor(5);
    pages[1].states.remove(2);
    for page in &mut pages {
        page.member_count = (ANCESTORS - 1) as u32;
    }
    assert!(
        attempt(pages).contains("incomplete below the tip"),
        "missing middle"
    );

    // An extra State outside the floor: address-valid, but unreachable from
    // the signed tip.
    let mut pages = floor(5);
    let stranger = State::new_snapshot(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("stranger", "s@example.test")),
    )
    .with_intent("not converted from this repository");
    pages[0].states.push(ImportAncestorState {
        id: Some(api::heddle::api::common::StateId {
            value: stranger.id().as_bytes().to_vec(),
        }),
        canonical_state: stranger.encode_current_msgpack().expect("bytes"),
    });
    for page in &mut pages {
        page.member_count = (ANCESTORS + 1) as u32;
    }
    assert!(
        attempt(pages).contains("outside the signed floor"),
        "extra State"
    );

    // A page set claiming a different floor (another tip, or another signed
    // operation) cannot stand in for the carried import's floor: the real
    // floor is then absent, and the stray set is refused. One stray page
    // leaves its own set incomplete.
    let mut pages = floor(5);
    for page in &mut pages {
        page.tip = Some(api::heddle::api::common::StateId {
            value: history.chain[0].id().as_bytes().to_vec(),
        });
    }
    assert!(
        attempt(pages).contains("import ancestry absent for a converted Git history"),
        "different tip"
    );
    let mut pages = floor(5);
    for page in &mut pages {
        page.signed_operation_digest = vec![7; 32];
    }
    assert!(
        attempt(pages).contains("import ancestry absent for a converted Git history"),
        "different operation"
    );
    let mut pages = floor(5);
    pages[2].signed_operation_digest = vec![7; 32];
    assert!(
        attempt(pages).contains("page set incomplete"),
        "one stray page"
    );
    // The real floor plus a stray complete set for another operation.
    let mut pages = floor(5);
    let mut stray = floor(4096);
    stray[0].signed_operation_digest = vec![7; 32];
    pages.extend(stray);
    assert!(
        attempt(pages).contains("outside the selected ancestry"),
        "stray floor"
    );

    // Pages that disagree about their floor, and a page set with a gap.
    let mut pages = floor(5);
    pages[1].member_count += 1;
    assert!(
        attempt(pages).contains("disagree about their floor"),
        "header mismatch"
    );
    let mut pages = floor(5);
    pages.remove(1);
    assert!(
        attempt(pages).contains("page set incomplete"),
        "missing page"
    );

    // A path that does not reach the selected revision proves nothing.
    let older = history.chain[3].clone();
    let short_path = history.chain[ANCESTORS - 2..ANCESTORS]
        .iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    let error = stage(
        &history,
        transfer(&history, scratch.path(), &older),
        page_set(
            &history,
            &short_path,
            import_ancestry_page::Coverage::Path,
            4096,
        ),
        BTreeSet::from([history.tip().id()]),
    )
    .err()
    .expect("short path");
    // The selection is refused before any closure is accepted: the pages do
    // not carry it at all, so no owning import operation is even selected.
    assert!(
        error.to_string().contains("selected source proof absent"),
        "{error}"
    );
    // A path that carries the selected commit but not the chain to it.
    let mut broken_path = short_path.clone();
    broken_path.push(older.clone());
    let error = stage(
        &history,
        transfer(&history, scratch.path(), &older),
        page_set(
            &history,
            &broken_path,
            import_ancestry_page::Coverage::Path,
            4096,
        ),
        BTreeSet::from([history.tip().id()]),
    )
    .err()
    .expect("broken path");
    assert!(
        error.to_string().contains("outside the signed floor"),
        "{error}"
    );

    // The floor must carry the tip's ancestry, not the tip itself.
    let mut pages = floor(5);
    pages[0].states.insert(
        0,
        ImportAncestorState {
            id: Some(api::heddle::api::common::StateId {
                value: history.tip().id().as_bytes().to_vec(),
            }),
            canonical_state: history.tip().encode_current_msgpack().expect("bytes"),
        },
    );
    for page in &mut pages {
        page.member_count = (ANCESTORS + 1) as u32;
    }
    assert!(
        attempt(pages).contains("carries the tip, frontier or genesis base"),
        "tip among members"
    );
}

#[test]
fn imported_member_obeys_publication_private_tier_without_sidecars() {
    let history = imported_history(4, 2);
    let scratch = tempfile::tempdir().expect("scratch");
    let receiver_dir = tempfile::tempdir().expect("receiver");
    let repository = receiver(&history, receiver_dir.path());
    let mut pages = page_set(
        &history,
        history.ancestors(),
        import_ancestry_page::Coverage::Floor,
        2,
    );
    for page in &mut pages {
        page.floor_tiers = Some(ImportFloorTierSummary {
            rows: vec![import_floor_tier_summary::Row {
                tier: import_floor_tier_summary::row::Tier::Private as i32,
                label: "security".into(),
                tier_rows: 1,
            }],
        });
    }
    let staged = stage(
        &history,
        transfer(&history, scratch.path(), history.tip()),
        pages,
        BTreeSet::new(),
    )
    .expect("stage");
    install(&history, &repository, staged);
    let member = history.chain[2].id();
    assert!(
        repository
            .withholding_visibility_for_audience(&member, &repo::AudienceTier::Public)
            .expect("walk")
            .is_some()
    );
    assert!(
        repository
            .collect_content_disclosure(&member)
            .expect("proof")
            .expect("resolved")
            .for_audience(&repo::AudienceTier::Public)
            .is_none()
    );
}

#[test]
fn ancestry_staging_retains_bounded_allocations() {
    let history = imported_history(512, 2);
    #[cfg(target_os = "linux")]
    let baseline_peak =
        (std::env::var_os("HEDDLE_ANCESTRY_RSS_PROBE").is_some()).then(peak_resident_bytes);
    let mut input = AncestryInput::default();
    let directory = tempfile::tempdir().expect("directory");
    for mut page in page_set(
        &history,
        history.ancestors(),
        import_ancestry_page::Coverage::Floor,
        1,
    ) {
        // Large canonical States must leave memory after each page, regardless
        // of how little graph metadata they carry.
        for ancestor in &mut page.states {
            let mut state =
                State::decode_current_msgpack(&ancestor.canonical_state).expect("state");
            state.intent = Some("x".repeat(64 * 1024));
            ancestor.canonical_state = state.encode_current_msgpack().expect("encode");
            ancestor.id.as_mut().expect("id").value = state.id().as_bytes().to_vec();
        }
        let thread = page.thread.clone().expect("thread");
        input.push(page, &thread, directory.path()).expect("page");
        assert!(input.retained_allocations() < 1024 * 1024);
    }
    let retained = input.retained_allocations();
    println!("retained parent graph allocations: {retained} bytes");
    input.finish().expect("finish");
    #[cfg(target_os = "linux")]
    if let Some(baseline) = baseline_peak {
        let growth = peak_resident_bytes().saturating_sub(baseline);
        println!("peak resident memory growth for 512 large States: {growth} bytes");
        assert!(growth < 16 * 1024 * 1024, "peak RSS growth: {growth}");
    }
    let reader = objects::store::pack::PackReader::open(
        &directory.path().join("ancestry.pack"),
        &directory.path().join("ancestry.idx"),
    )
    .expect("staged pack");
    let mut persisted = 0;
    reader
        .visit_objects(|_, kind, bytes| {
            assert_eq!(kind, objects::store::pack::ObjectType::State);
            persisted += bytes.len();
            Ok(())
        })
        .expect("read staged States");
    assert!(
        persisted > 32 * 1024 * 1024,
        "canonical payloads must all reach disk"
    );
    assert!(
        retained < 1024 * 1024,
        "retained canonical allocations: {retained}"
    );
}

#[test]
fn shared_git_history_selects_a_tip_independently_of_page_order() {
    let history = imported_history(4, 2);
    let mut pages = page_set(
        &history,
        history.ancestors(),
        import_ancestry_page::Coverage::Floor,
        4,
    );
    let mut shared = pages[0].clone();
    shared.tip.as_mut().expect("tip").value = vec![93; 32];
    shared.signed_operation_digest = vec![94; 32];
    pages.push(shared);
    let mut selections = Vec::new();
    for reverse in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let mut input = AncestryInput::default();
        let mut ordered = pages.clone();
        if reverse {
            ordered.reverse();
        }
        for page in ordered {
            let thread = page.thread.clone().expect("thread");
            input.push(page, &thread, directory.path()).expect("page");
        }
        input.finish().expect("finish");
        selections.push(
            input
                .selected_page_tip(history.chain[2].id(), directory.path())
                .expect("selection")
                .expect("shared ancestor"),
        );
    }
    assert_eq!(selections[0], selections[1]);
    assert_eq!(
        selections[0].1,
        history.chain[2].encode_current_msgpack().expect("state")
    );
}

#[test]
fn ancestry_headers_require_identical_valid_whole_floor_tiers() {
    let history = imported_history(4, 2);
    let scratch = tempfile::tempdir().expect("scratch");
    let floor = || {
        page_set(
            &history,
            history.ancestors(),
            import_ancestry_page::Coverage::Floor,
            2,
        )
    };
    let attempt = |pages| {
        stage(
            &history,
            transfer(&history, scratch.path(), history.tip()),
            pages,
            BTreeSet::new(),
        )
        .err()
        .expect("reject")
        .to_string()
    };
    let mut pages = floor();
    pages[0].floor_tiers = None;
    assert!(attempt(pages).contains("tier summary absent"));
    let row = import_floor_tier_summary::Row {
        tier: 2,
        label: "legal".into(),
        tier_rows: 3,
    };
    for rows in [
        vec![import_floor_tier_summary::Row {
            tier_rows: 0,
            ..row.clone()
        }],
        vec![import_floor_tier_summary::Row {
            tier: 0,
            ..row.clone()
        }],
        vec![row.clone(), row.clone()],
    ] {
        let mut pages = floor();
        for page in &mut pages {
            page.floor_tiers = Some(ImportFloorTierSummary { rows: rows.clone() });
        }
        assert!(attempt(pages).contains("invalid import floor tier summary"));
    }
    let mut pages = floor();
    pages[1].floor_tiers = Some(ImportFloorTierSummary { rows: vec![row] });
    assert!(attempt(pages).contains("disagree about their floor"));
}

#[test]
fn path_into_fresh_clone_requires_fetching_the_import_tip_first() {
    let history = imported_history(8, 2);
    let scratch = tempfile::tempdir().expect("scratch");
    let receiver_dir = tempfile::tempdir().expect("receiver");
    let repository = receiver(&history, receiver_dir.path());
    let path: Vec<_> = history.chain[2..8].iter().rev().cloned().collect();
    let staged = stage(
        &history,
        transfer(&history, scratch.path(), &history.chain[2]),
        page_set(&history, &path, import_ancestry_page::Coverage::Path, 2),
        BTreeSet::new(),
    )
    .expect("proved path");
    let error = try_install(&history, &repository, staged)
        .expect_err("fresh PATH cannot install a full floor");
    assert!(
        error.to_string().contains("fetch the import tip first"),
        "{error}"
    );
}

#[test]
fn path_refreshes_the_whole_floor_summary_and_can_remove_constraints() {
    let history = imported_history(8, 4);
    let scratch = tempfile::tempdir().expect("scratch");
    let receiver_dir = tempfile::tempdir().expect("receiver");
    let repository = receiver(&history, receiver_dir.path());
    let full = stage(
        &history,
        transfer(&history, scratch.path(), history.tip()),
        page_set(
            &history,
            history.ancestors(),
            import_ancestry_page::Coverage::Floor,
            4,
        ),
        BTreeSet::new(),
    )
    .expect("floor");
    install(&history, &repository, full);
    let path: Vec<_> = history.chain[4..8].iter().rev().cloned().collect();
    for constrained in [true, false] {
        let mut pages = page_set(&history, &path, import_ancestry_page::Coverage::Path, 2);
        if constrained {
            for page in &mut pages {
                page.floor_tiers = Some(ImportFloorTierSummary {
                    rows: vec![import_floor_tier_summary::Row {
                        tier: 2,
                        label: "legal".into(),
                        tier_rows: 3,
                    }],
                });
            }
        }
        let staged = stage(
            &history,
            transfer(&history, scratch.path(), &history.chain[4]),
            pages,
            BTreeSet::from([history.tip().id()]),
        )
        .expect("path");
        install(&history, &repository, staged);
        // This member did not travel on the path. The summary still covers it.
        assert_eq!(
            repository
                .withholding_visibility_for_audience(
                    &history.chain[0].id(),
                    &repo::AudienceTier::Public
                )
                .expect("visibility")
                .is_some(),
            constrained
        );
        assert!(
            repository
                .withholding_visibility_for_audience(
                    &history.chain[0].id(),
                    &repo::AudienceTier::Restricted("legal".into())
                )
                .expect("label audience")
                .is_none()
        );
    }
}

#[cfg(target_os = "linux")]
fn peak_resident_bytes() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .expect("process memory")
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .expect("peak resident memory")
        .split_whitespace()
        .next()
        .expect("peak size")
        .parse::<usize>()
        .expect("KiB")
        * 1024
}

#[cfg(target_os = "linux")]
#[test]
fn ancestry_staging_peak_memory_is_bounded() {
    // A single-test subprocess excludes allocations by concurrent test cases.
    // Kernel high-water RSS includes transient decoding and writer allocations,
    // and catches retained payloads even if the graph's accounting is unchanged.
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "fetch::hosted::import_ancestry_tests::ancestry_staging_retains_bounded_allocations",
            "--nocapture",
        ])
        .env("HEDDLE_ANCESTRY_RSS_PROBE", "1")
        .output()
        .expect("isolated memory test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{stdout}");
}

/// Every file under the receiver's published source storage, relative to its
/// heddle directory, with its bytes.
fn published_files(repository: &Repository) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).expect("relative").to_path_buf(),
                    std::fs::read(&path).expect("file"),
                );
            }
        }
    }
    let root = repository.heddle_dir();
    let mut files = BTreeMap::new();
    for name in ["packs", "objects"] {
        walk(root, &root.join(name), &mut files);
    }
    files
}

/// Fresh clone of `history` into a new receiver. Returns the receiver, the
/// install result and how many files the install published.
fn fresh_clone(
    history: &ImportedHistory,
    scratch: &Path,
    receiver_dir: &Path,
) -> (Repository, Result<StateId, Error>, usize) {
    let repository = receiver(history, receiver_dir);
    let before = published_files(&repository).len();
    let staged = stage(
        history,
        transfer(history, scratch, history.tip()),
        page_set(
            history,
            history.ancestors(),
            import_ancestry_page::Coverage::Floor,
            1_000,
        ),
        BTreeSet::new(),
    )
    .expect("floor verifies against the signed tip");
    let started = std::time::Instant::now();
    let installed = try_install(history, &repository, staged);
    println!(
        "fresh clone install of {} converted commits: {:?}",
        history.ancestors().len(),
        started.elapsed()
    );
    let published = published_files(&repository).len() - before;
    (repository, installed, published)
}

fn assert_history_installed(repository: &Repository, history: &ImportedHistory) {
    use objects::store::ObjectStore;
    let store = Repository::open(repository.root()).expect("reopen");
    for state in history.ancestors().iter().chain([history.tip()]) {
        assert_eq!(
            store
                .store()
                .get_state(&state.id())
                .expect("read")
                .as_ref()
                .map(State::id),
            Some(state.id()),
            "every converted commit is installed"
        );
    }
}

/// HeddleCo/heddle#2023: a fresh clone of more than ~1,020 converted commits
/// failed with `installation entry budget exceeded`, because every State was
/// staged as its own loose file and journaled through the bounded undo log.
#[test]
fn fresh_clone_of_a_long_converted_history_installs_under_default_limits() {
    const LONG: usize = 2_100;
    const SHORT: usize = 64;
    let scratch = tempfile::tempdir().expect("scratch");

    let short = imported_history(SHORT, 2);
    let short_dir = tempfile::tempdir().expect("receiver");
    let (_, installed, short_published) = fresh_clone(&short, scratch.path(), short_dir.path());
    assert_eq!(installed.expect("short history installs"), short.tip().id());

    let long = imported_history(LONG, 2);
    let long_dir = tempfile::tempdir().expect("receiver");
    let (repository, installed, long_published) =
        fresh_clone(&long, scratch.path(), long_dir.path());
    assert_eq!(
        installed.expect("a 2,100-commit history installs under default limits"),
        long.tip().id()
    );
    assert_history_installed(&repository, &long);
    // Each published file costs a fixed number of journal entries and fsyncs.
    assert_eq!(
        long_published, short_published,
        "published files (and so undo entries and fsyncs) must not scale with commit count"
    );
    assert!(
        !repository
            .heddle_dir()
            .join("objects/states")
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some()),
        "converted commits are served from their pack, not per-commit loose files"
    );

    // A lazy fetch of the oldest converted commit proves membership with a
    // parent chain of more than 2,000 States the client already holds.
    let older = long.chain[2].clone();
    let path = long.chain[2..LONG]
        .iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    let staged = stage(
        &long,
        transfer(&long, scratch.path(), &older),
        page_set(&long, &path, import_ancestry_page::Coverage::Path, 1_000),
        BTreeSet::from([long.tip().id()]),
    )
    .expect("a deep commit stages through its parent chain proof");
    let before = published_files(&repository).len();
    assert_eq!(install(&long, &repository, staged), older.id());
    assert!(
        published_files(&repository).len() - before <= short_published,
        "a deep lazy fetch publishes no per-commit files"
    );
    let checkout = tempfile::tempdir().expect("older checkout");
    assert_eq!(
        checkout_files(&repository, &older, checkout.path()),
        ["OLD.txt"]
    );
}

const CRASH_CHILD: &str = "HEDDLE_TEST_HOSTED_INSTALL_CRASH_RECEIVER";

/// An install interrupted mid-publication (the process dies with the undo log
/// open) leaves the repository exactly as it was once recovery runs, and the
/// same clone then succeeds.
#[test]
fn crash_mid_long_history_install_recovers_an_intact_store() {
    const LONG: usize = 2_100;
    let history = imported_history(LONG, 2);
    let scratch = tempfile::tempdir().expect("scratch");
    if let Some(receiver_dir) = std::env::var_os(CRASH_CHILD) {
        // Child: publish two staged files, then die without unwinding.
        let repository = Repository::open(&receiver_dir).expect("receiver");
        let staged = stage(
            &history,
            transfer(&history, scratch.path(), history.tip()),
            page_set(
                &history,
                history.ancestors(),
                import_ancestry_page::Coverage::Floor,
                1_000,
            ),
            BTreeSet::new(),
        )
        .expect("stage");
        super::CRASH_AFTER_PUBLISHED.with(|after| after.set(Some(2)));
        let result = try_install(&history, &repository, staged);
        panic!("the crash point must abort before install returns: {result:?}");
    }

    let receiver_dir = tempfile::tempdir().expect("receiver");
    let repository = receiver(&history, receiver_dir.path());
    let before = published_files(&repository);
    let states_before = {
        use objects::store::ObjectStore;
        repository.store().list_states().expect("states")
    };
    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "crash_mid_long_history_install_recovers_an_intact_store",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD, receiver_dir.path())
        .output()
        .expect("crash child");
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        !child.status.success() && stderr.contains("hosted publish crash point reached"),
        "child must die at the crash point: {:?}\n{stderr}",
        child.status
    );
    let intent = repository.heddle_dir().join("hosted-install.intent");
    let crashed = published_files(&repository);
    assert!(
        intent.is_file() && crashed.len() > before.len(),
        "the crash must leave a partly published install behind"
    );

    // Opening the hosted trust runs installation recovery, as any reader or
    // writer of the repository does.
    drop(
        HostedTrust::open(
            repository.heddle_dir(),
            &history.root.authority,
            ReceiverClock,
        )
        .expect("recovery"),
    );
    assert!(!intent.exists(), "recovery retires the undo log");
    assert_eq!(
        published_files(&repository),
        before,
        "recovery restores source storage byte-for-byte"
    );
    {
        use objects::store::ObjectStore;
        let reopened = Repository::open(receiver_dir.path()).expect("reopen");
        assert_eq!(
            reopened.store().list_states().expect("states"),
            states_before,
            "no converted commit survives a rolled-back install"
        );
    }

    let staged = stage(
        &history,
        transfer(&history, scratch.path(), history.tip()),
        page_set(
            &history,
            history.ancestors(),
            import_ancestry_page::Coverage::Floor,
            1_000,
        ),
        BTreeSet::new(),
    )
    .expect("stage");
    assert_eq!(install(&history, &repository, staged), history.tip().id());
    assert_history_installed(&repository, &history);
}
