use std::collections::BTreeSet;

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use objects::object::{
    Attribution, CollaborationActor, CollaborationMetadata, CollaborationSourceAnchor,
    ContextRevision, Principal, Tree, TreeEntry,
    source_target::{SourceTargetReference, capture::SourceTargetSnapshot},
};

use super::*;
fn put(store: &impl ObjectStore, value: &impl serde::Serialize) -> Result<ContentHash> {
    Ok(store.put_blob(&Blob::new(capture::encode(value)?))?)
}

fn state(repo: &Repository, parent: StateId, path: &str, text: &str) -> State {
    let blob = Blob::new(text.as_bytes().to_vec());
    let hash = repo.store().put_blob(&blob).expect("blob");
    let mut tree = Tree::new();
    tree.insert(TreeEntry::file(path, hash, false).expect("file entry"));
    let tree = repo.store().put_tree(&tree).expect("tree");
    let state = State::new_snapshot(
        tree,
        vec![parent],
        Attribution::human(Principal::new("author", "author@example.test")),
    );
    repo.store().put_state(&state).expect("state");
    state
}
fn fixture() -> (
    tempfile::TempDir,
    Repository,
    ThreadReplica,
    State,
    SourceTargetReference,
) {
    let directory = tempfile::tempdir().expect("repo");
    let repo = Repository::init_default(directory.path()).expect("init");
    let base = state(
        &repo,
        repo.head().expect("head").expect("base"),
        "main.rs",
        "one\ntwo\nthree\n",
    );
    let replica = repo
        .create_native_thread("tracked", base.id(), None, "shared references")
        .expect("thread");
    let scope = replica.reference_scope().expect("scope");
    let file = SourceFileCore {
        scope: scope.clone(),
        revision: CollaborationRevision::State {
            state_id: base.id(),
        },
        path: "main.rs".into(),
    };
    let core = SourceTargetCore {
        file: file.id().expect("file identity"),
        revision: file.revision.clone(),
        selector: SourceSelector::Lines {
            range: SourceLineRange {
                start: 1,
                end: 2,
                start_affinity: SourceAffinity::After,
                end_affinity: SourceAffinity::Before,
            },
        },
    };
    let target = SourceTargetReference {
        target: core.id().expect("target"),
        binding: SourceTargetBinding::ViewedThread,
    };
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("signer");
    for n in 1..=32 {
        let source = CollaborationSourceAnchor {
            revision: file.revision.clone(),
            path: file.path.clone(),
            symbol_id: String::new(),
            start_line: Some(2),
            end_line: Some(2),
            target: Some(target.clone()),
        };
        let context = ContextRevision {
            version: 2,
            id: uuid::Uuid::from_u128(n),
            parents: vec![],
            metadata: CollaborationMetadata {
                scope: scope.clone(),
                actor: CollaborationActor {
                    principal_id: uuid::Uuid::from_u128(1),
                    agent_id: None,
                },
                mentions: vec![],
            },
            anchor: CollaborationAnchor::Source { source },
            content: format!("Annotation {n}"),
            tags: vec![],
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: 100,
        };
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread,
            parents: BTreeSet::new(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Context(context.encode().expect("context")),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("sign");
        assert_eq!(
            replica
                .receive(&signed, repo.store(), |_| Ok(()))
                .expect("context admission"),
            super::super::Admission::Accepted
        );
    }
    (directory, repo, replica, base, target)
}
fn captured(
    repo: &Repository,
    replica: &ThreadReplica,
    name: &str,
    state: &State,
) -> (SignedOperation, ReferenceClosure) {
    let id = repo
        .record_native_capture(name, state.id())
        .expect("production capture");
    let signed = replica
        .operation(&id)
        .expect("operation")
        .expect("present")
        .0;
    let op = signed.verify().expect("signed capture");
    let proof = op
        .reference_proof(&replica.genesis().expect("genesis"))
        .expect("reference proof")
        .expect("tracked closure");
    let closure = capture::closure(
        &Source(repo.store()),
        proof.descriptor,
        &proof.scope,
        proof.state,
    )
    .expect("verified canonical closure");
    (signed, closure)
}

#[test]
fn production_capture_tracks_shared_lines_and_fork_moves_independently() {
    let (_directory, repo, replica, base, target) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    let (original, closure) = captured(&repo, &replica, "tracked", &first);
    assert!(
        matches!(closure.targets[&target.target].selector,SourceSelector::Lines {range} if range.start==2&&range.end==3)
    );
    assert_eq!(
        replica
            .connect()
            .expect("db")
            .query_row(
                "SELECT count(*) FROM reference_seeds WHERE thread=?1",
                [replica.thread.as_bytes()],
                |r| r.get::<_, i64>(0)
            )
            .expect("seed count"),
        1,
        "32 referrers share one target seed"
    );
    let before = replica
        .accepted_page(ThreadFacet::Discussion, None, 100)
        .expect("signed annotation history");
    let child = repo
        .create_native_thread("child", first.id(), Some("tracked"), "fork")
        .expect("fork");
    assert_eq!(
        crate::reference_projection::root(
            &child.connect().expect("db"),
            &child.reference_scope().expect("scope")
        )
        .expect("fork root"),
        Some(closure.snapshot.targets)
    );
    let grandchild = repo
        .create_native_thread("grandchild", first.id(), Some("child"), "nested fork")
        .expect("nested fork");
    assert_eq!(
        crate::reference_projection::root(
            &grandchild.connect().expect("db"),
            &grandchild.reference_scope().expect("scope")
        )
        .expect("nested fork root"),
        Some(closure.snapshot.targets)
    );
    let next = state(&repo, first.id(), "renamed.rs", "zero\none\ntwo\nthree\n");
    let (_, forked) = captured(&repo, &child, "child", &next);
    assert_eq!(
        forked.files.values().next().expect("file").path,
        "renamed.rs"
    );
    assert_eq!(
        forked.snapshot.targets, closure.snapshot.targets,
        "file rename leaves every target selector untouched"
    );
    assert_eq!(
        replica
            .accepted_page(ThreadFacet::Discussion, None, 100)
            .expect("history"),
        before,
        "capture creates no per-annotation rebinding operations"
    );
    let parent_scope = replica.reference_scope().expect("scope");
    let child_scope = child.reference_scope().expect("scope");
    let named = SourceTargetReference {
        target: target.target,
        binding: SourceTargetBinding::NamedThread {
            scope: parent_scope.clone(),
        },
    };
    assert_eq!(
        crate::reference_projection::resolve(
            &child.connect().expect("db"),
            &child_scope,
            &named,
            &mut budget()
        )
        .expect("named parent"),
        crate::reference_projection::resolve(
            &replica.connect().expect("db"),
            &parent_scope,
            &target,
            &mut budget()
        )
        .expect("parent")
    );
    let retry = repo
        .record_native_capture("tracked", first.id())
        .expect("exact retry");
    assert_eq!(
        replica
            .operation(&retry)
            .expect("operation")
            .expect("record")
            .0,
        original
    );
    let pinned = SourceTargetReference {
        target: target.target,
        binding: SourceTargetBinding::PinnedRevision {
            scope: parent_scope,
            revision: CollaborationRevision::State {
                state_id: first.id(),
            },
        },
    };
    assert_eq!(
        child
            .resolve_source_target(&repo, &pinned, next.id())
            .expect("pinned")
            .expect("resolved")
            .file
            .path,
        "main.rs"
    );
    assert_eq!(
        child
            .resolve_source_target(&repo, &target, next.id())
            .expect("viewed child")
            .expect("resolved")
            .file
            .path,
        "renamed.rs"
    );
}

#[test]
fn signed_scope_corruption_cannot_publish_reference_root_or_advance_admission() {
    let (_directory, repo, replica, base, _) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    let capture = replica.prepare_capture(&repo, &first).expect("prepare");
    let mut snapshot: SourceTargetSnapshot = capture::decode(
        repo.store()
            .get_blob(&capture.source_targets.expect("descriptor"))
            .expect("blob")
            .expect("present")
            .content(),
    )
    .expect("descriptor");
    snapshot.scope.thread = Some(ContentHash::from_bytes([99; 32]));
    let altered = Capture {
        source_targets: Some(put(repo.store(), &snapshot).expect("altered immutable descriptor")),
        ..capture
    };
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("signer");
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread,
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(objects::object::thread_replication::AuthoredCapture::local(altered)),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("valid publisher signature");
    let before = replica.generation().expect("generation");
    assert!(replica.receive(&signed, repo.store(), |_| Ok(())).is_err());
    assert_eq!(replica.generation().expect("generation"), before);
    assert!(
        replica
            .operation(&operation.id().expect("id"))
            .expect("lookup")
            .is_none()
    );
    assert_eq!(
        crate::reference_projection::root(
            &replica.connect().expect("db"),
            &replica.reference_scope().expect("scope")
        )
        .expect("root"),
        None
    );
}

#[test]
fn source_pack_exports_and_validates_exact_signed_reference_closure() {
    use objects::store::pack::{
        PackReader, StreamingPackBuilder, build_source_pack_with_references,
    };
    let (_directory, repo, replica, base, _) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    let (signed, closure) = captured(&repo, &replica, "tracked", &first);
    let proof = signed
        .verify()
        .expect("signature")
        .reference_proof(&replica.genesis().expect("genesis"))
        .expect("proof")
        .expect("tracked");
    let scratch = tempfile::tempdir().expect("pack scratch");
    let path = scratch.path().join("source.pack");
    let index = scratch.path().join("source.idx");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("pack file");
    let builder = StreamingPackBuilder::new(
        file,
        index.clone(),
        Default::default(),
        scratch.path().join("buckets"),
    )
    .expect("builder");
    let (file, _) = build_source_pack_with_references(
        builder,
        &Source(repo.store()),
        &first,
        std::slice::from_ref(&proof),
        1024,
        8 * 1024 * 1024,
    )
    .expect("production reference pack");
    drop(file);
    let reader = PackReader::open(&path, &index).expect("reader");
    let objects = reader
        .validate_source_closure_with_references(
            &first,
            std::slice::from_ref(&proof),
            1024,
            8 * 1024 * 1024,
        )
        .expect("exact reference closure");
    assert_eq!(
        objects.len(),
        3 + closure.blobs.len(),
        "State/tree/source blob plus exact reference closure"
    );
    assert!(
        reader
            .validate_source_closure(&first, 1024, 8 * 1024 * 1024)
            .is_err(),
        "extra reference bytes require signed selection proof"
    );
    let mut changed = proof;
    changed.scope.thread = Some(ContentHash::from_bytes([99; 32]));
    assert!(
        reader
            .validate_source_closure_with_references(&first, &[changed], 1024, 8 * 1024 * 1024)
            .is_err(),
        "valid blob addresses cannot replace scope proof"
    );
}

#[test]
fn local_integration_inherits_source_and_target_roots_and_next_capture_keeps_them() {
    use objects::object::{
        VisibilityTier, thread_replication::local_integration::LocalIntegration,
    };
    let (_directory, repo, target, base, reference) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    let (parent, _) = captured(&repo, &target, "tracked", &first);
    let source = repo
        .create_native_thread("integration-source", first.id(), Some("tracked"), "fork")
        .expect("fork");
    let moved = state(&repo, first.id(), "renamed.rs", "zero\none\ntwo\nthree\n");
    let (original, _) = captured(&repo, &source, "integration-source", &moved);
    let source_operation = original.verify().expect("source").id().expect("source ID");
    let result = State::new_merge(
        moved.tree,
        vec![first.id(), moved.id()],
        Attribution::human(Principal::new("integrator", "")),
    );
    repo.store().put_state(&result).expect("result");
    let prepared = target
        .prepare_integration(&repo, &result, source.thread, source_operation)
        .expect("prepare exact integration roots");
    let signer = Ed25519Signer::from_seed(&[92; 32]).expect("signer");
    let parents = BTreeSet::from([parent.verify().expect("parent").id().expect("parent ID")]);
    let receipt = LocalIntegration {
        author: objects::object::thread_replication::SourceAuthor::LocalKey,
        version: 1,
        spool: target.reference_scope().expect("scope").spool,
        device: signer.public_key().try_into().expect("key"),
        source_thread: source.thread,
        source_operation,
        source_revision: moved.id(),
        target_thread: target.thread,
        expected_target_frontier: parents.clone(),
        result: prepared,
        result_visibility: VisibilityTier::Public,
        initiating_request_proof: ContentHash::from_bytes([3; 32]),
        local_policy_version: ContentHash::from_bytes([4; 32]),
        executed_at_ms: 100,
    };
    let operation = ThreadOperation {
        version: 1,
        thread: target.thread,
        parents,
        publisher: receipt.device,
        body: ThreadOperationBody::LocalIntegration(receipt.encode().expect("receipt")),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("signed integration");
    assert_eq!(
        target
            .receive_local_integration_cas(&signed, repo.store(), |_| Ok(()))
            .expect("admission"),
        super::super::Admission::Accepted
    );
    let resolved = target
        .resolve_source_target(&repo, &reference, result.id())
        .expect("resolve landed target")
        .expect("landed target present");
    assert_eq!(resolved.file.path, "renamed.rs");
    assert_eq!(
        resolved.file.status,
        ResolutionStatus::Resolved,
        "unambiguous result selects matching source file"
    );
    assert_eq!(resolved.target.status, ResolutionStatus::Resolved);
    let next = state(
        &repo,
        result.id(),
        "renamed.rs",
        "extra\nzero\none\ntwo\nthree\n",
    );
    let (_, closure) = captured(&repo, &target, "tracked", &next);
    assert!(
        matches!(closure.targets[&reference.target].selector, SourceSelector::Lines {range} if range.start==3 && range.end==4)
    );
    let mut omitted = receipt;
    omitted.result.source_targets = None;
    assert!(
        omitted
            .validate_source(&original.verify().expect("source"))
            .is_err(),
        "integration cannot drop source closure"
    );
}

#[test]
fn capture_cannot_drop_parent_or_fork_reference_closure() {
    let (_directory, repo, replica, base, _) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    let (parent, _) = captured(&repo, &replica, "tracked", &first);
    let signer = Ed25519Signer::from_seed(&[93; 32]).expect("signer");
    let next = state(
        &repo,
        first.id(),
        "main.rs",
        "next\nzero\none\ntwo\nthree\n",
    );
    let omitted = ThreadOperation {
        version: 1,
        thread: replica.thread,
        parents: BTreeSet::from([parent.verify().expect("parent").id().expect("parent ID")]),
        publisher: signer.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(objects::object::thread_replication::AuthoredCapture::local(next.encode_current_msgpack().expect("State").into())),
    };
    let signed = SignedOperation::sign(&omitted, &signer).expect("signed");
    assert!(
        matches!(replica.receive(&signed,repo.store(), |_|Ok(())).expect("record rejection"),super::super::Admission::Rejected(reason) if reason.contains("drops inherited reference closure"))
    );
    let fork = repo
        .create_native_thread("drop-fork", first.id(), Some("tracked"), "fork")
        .expect("fork");
    let omitted = ThreadOperation {
        thread: fork.thread,
        parents: BTreeSet::new(),
        ..omitted
    };
    let signed = SignedOperation::sign(&omitted, &signer).expect("signed");
    assert!(
        fork.receive(&signed, repo.store(), |_| Ok(()))
            .expect_err("fork cannot drop inherited root")
            .to_string()
            .contains("drops inherited fork reference closure")
    );
}

#[test]
fn integration_keeps_competing_branch_locations_ambiguous() {
    let (_directory, repo, target, base, reference) = fixture();
    let first = state(&repo, base.id(), "main.rs", "zero\none\ntwo\nthree\n");
    captured(&repo, &target, "tracked", &first);
    let source = repo
        .create_native_thread("competing-source", first.id(), Some("tracked"), "fork")
        .expect("fork");
    let left = state(&repo, first.id(), "left.rs", "zero\none\ntwo\nthree\n");
    captured(&repo, &target, "tracked", &left);
    let right = state(&repo, first.id(), "right.rs", "zero\none\ntwo\nthree\n");
    let (original, _) = captured(&repo, &source, "competing-source", &right);
    let blob = repo
        .store()
        .put_blob(&Blob::new(b"zero\none\ntwo\nthree\n".to_vec()))
        .expect("blob");
    let mut tree = Tree::new();
    tree.insert(TreeEntry::file("left.rs", blob, false).expect("left"));
    tree.insert(TreeEntry::file("right.rs", blob, false).expect("right"));
    let tree = repo.store().put_tree(&tree).expect("tree");
    let result = State::new_merge(
        tree,
        vec![left.id(), right.id()],
        Attribution::human(Principal::new("integrator", "")),
    );
    let prepared = target
        .prepare_integration(
            &repo,
            &result,
            source.thread,
            original.verify().expect("operation").id().expect("id"),
        )
        .expect("prepare competing locations");
    let closure = capture::closure(
        &Source(repo.store()),
        prepared.source_targets.expect("descriptor"),
        &target.reference_scope().expect("scope"),
        result.id(),
    )
    .expect("closure");
    let selected = &closure.targets[&reference.target];
    assert_eq!(selected.status, ResolutionStatus::Ambiguous);
    assert_eq!(
        closure.files[&selected.core.file].status,
        ResolutionStatus::Ambiguous
    );
}
