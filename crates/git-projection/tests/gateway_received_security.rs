// SPDX-License-Identifier: Apache-2.0
//! Independent byte/closure checks at the hosted receiver's Git boundary.
//! Synthetic signatures are never treated as current account authorization.
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    gateway_received::{project_received_git_history, verify_received_git_history},
    gateway_view::ViewLimits,
};
use objects::{
    object::{
        Attribution, Blob, ContentHash, ObjectSource, Principal, State, StateId, Tree, TreeEntry,
        VisibilityTier,
        thread_replication::{
            AuthoredCapture, CaptureVisibility, ThreadOperation, ThreadOperationBody,
            initial_base::synthetic_initial_base,
        },
    },
    store::{InMemoryStore, ObjectStore},
};
use std::collections::BTreeSet;

struct Fixture {
    store: InMemoryStore,
    originals: Vec<SignedOperation>,
    base: StateId,
    tip: StateId,
    signer: Ed25519Signer,
    old_blob: ContentHash,
}
fn fixture() -> Fixture {
    let store = InMemoryStore::new();
    let signer = Ed25519Signer::from_seed(&[53; 32]).expect("synthetic signer");
    let seed = synthetic_initial_base().expect("canonical seed");
    // Exact SourcePacks omit the schema-defined initialization sentinel.
    // The receiver derives only that known empty State, never arbitrary parents.
    let mut parent = seed.id();
    let mut originals = Vec::new();
    let mut revisions = Vec::new();
    let mut old_blob = ContentHash::compute(b"unset");
    for index in 0..2 {
        let blob = Blob::new(format!("public retained native bytes {index}\n").into_bytes());
        store.put_blob(&blob).expect("blob");
        if index == 0 {
            old_blob = blob.hash();
        }
        let tree = Tree::from_git_entries(vec![
            TreeEntry::file("public.txt", blob.hash(), false).expect("entry"),
        ])
        .expect("tree");
        store.put_tree(&tree).expect("tree");
        let state = State::new_snapshot(
            tree.hash(),
            vec![parent],
            Attribution::human(Principal::new(
                "Receiver Fixture",
                "receiver@example.invalid",
            )),
        )
        .with_intent(format!("receiver capture {index}"));
        store.put_state(&state).expect("state");
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::compute(b"synthetic receiver Thread"),
            parents: originals
                .last()
                .map(|original: &SignedOperation| {
                    BTreeSet::from([original.verify().expect("original").id().expect("ID")])
                })
                .unwrap_or_default(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                state
                    .encode_current_msgpack()
                    .expect("canonical State")
                    .into(),
            )),
        };
        originals.push(SignedOperation::sign(&operation, &signer).expect("signed capture"));
        revisions.push(state.id());
        parent = state.id();
    }
    Fixture {
        store,
        originals,
        base: revisions[0],
        tip: revisions[1],
        signer,
        old_blob,
    }
}

#[test]
fn receiver_reconstructs_old_and_new_from_full_native_bytes_and_rejects_claimed_oid_substitution() {
    let f = fixture();
    let projection =
        project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default())
            .expect("full received graph");
    let old: [u8; 20] = projection
        .history()
        .git_oid(&f.base)
        .expect("old")
        .as_bytes()
        .try_into()
        .expect("SHA1");
    let new: [u8; 20] = projection
        .history()
        .git_oid(&f.tip)
        .expect("new")
        .as_bytes()
        .try_into()
        .expect("SHA1");
    verify_received_git_history(
        &f.store,
        &f.originals,
        f.base,
        f.tip,
        &old,
        &new,
        ViewLimits::default(),
    )
    .expect("receiver proves exact fast-forward");
    for (old, new, expected) in [
        ([9; 20], new, f.base),
        (old, [9; 20], f.base),
        (new, new, f.tip),
    ] {
        assert!(
            verify_received_git_history(
                &f.store,
                &f.originals,
                expected,
                f.tip,
                &old,
                &new,
                ViewLimits::default()
            )
            .is_err()
        );
    }
}

struct OmitBlob<'a> {
    inner: &'a InMemoryStore,
    missing: ContentHash,
}
impl ObjectSource for OmitBlob<'_> {
    fn get_tree(&self, id: &ContentHash) -> objects::store::Result<Option<Tree>> {
        ObjectStore::get_tree(self.inner, id)
    }
    fn get_state(&self, id: &StateId) -> objects::store::Result<Option<State>> {
        ObjectStore::get_state(self.inner, id)
    }
    fn get_blob(&self, id: &ContentHash) -> objects::store::Result<Option<Blob>> {
        if id == &self.missing {
            Ok(None)
        } else {
            ObjectStore::get_blob(self.inner, id)
        }
    }
}
#[test]
fn complete_tip_pack_does_not_replace_missing_historical_bytes_or_original_admission() {
    let f = fixture();
    let missing = OmitBlob {
        inner: &f.store,
        missing: f.old_blob,
    };
    let error = project_received_git_history(&missing, &f.originals, f.tip, ViewLimits::default())
        .err()
        .expect("historical blob is mandatory");
    assert!(
        error.to_string().contains("historical blob missing"),
        "{error}"
    );
    let error =
        project_received_git_history(&f.store, &f.originals[1..], f.tip, ViewLimits::default())
            .err()
            .expect("old signed original is mandatory");
    assert!(
        error.to_string().contains("exact signed original"),
        "{error}"
    );
}

#[test]
fn one_public_original_cannot_launder_a_second_private_original_for_the_same_state() {
    let mut f = fixture();
    let mut hidden = f.originals[0].verify().expect("original");
    let ThreadOperationBody::Capture(capture) = &mut hidden.body else {
        panic!("capture")
    };
    capture.result.visibility = Some(CaptureVisibility {
        state: Some(VisibilityTier::Private {
            scope_label: "withheld".into(),
        }),
        embargo_until: None,
        entries: vec![],
    });
    f.originals.push(
        SignedOperation::sign(&hidden, &f.signer).expect("private original with genuine signature"),
    );
    let error = project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default())
        .err()
        .expect("all distinct originals intersect");
    assert!(
        error.to_string().contains("visibility withholds"),
        "{error}"
    );
}

#[test]
fn changed_unhashed_fidelity_cannot_reuse_a_valid_signed_state() {
    let f = fixture();
    let mut changed = ObjectStore::get_state(&f.store, &f.base)
        .expect("state")
        .expect("base");
    changed.git_lossy = true;
    assert_eq!(changed.id(), f.base);
    f.store
        .put_state(&changed)
        .expect("alter unsigned fidelity field");
    let error = project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default())
        .err()
        .expect("full canonical signed bytes required");
    assert!(
        error.to_string().contains("exact signed original"),
        "{error}"
    );
}

#[test]
fn receiver_full_history_budgets_include_retained_ancestors() {
    let f = fixture();
    let base = ObjectStore::get_state(&f.store, &f.base)
        .expect("state")
        .expect("base");
    let tree = ObjectStore::get_tree(&f.store, &base.tree)
        .expect("tree")
        .expect("tree");
    let one_blob = ObjectStore::get_blob(&f.store, &f.old_blob)
        .expect("blob")
        .expect("blob")
        .content()
        .len();
    for limits in [
        ViewLimits {
            states: 2,
            ..ViewLimits::default()
        },
        ViewLimits {
            entries: tree.entries().len(),
            ..ViewLimits::default()
        },
        ViewLimits {
            bytes: one_blob,
            ..ViewLimits::default()
        },
    ] {
        assert!(
            project_received_git_history(&f.store, &f.originals, f.tip, limits).is_err(),
            "old and new histories share one bounded closure"
        );
    }
}

#[test]
fn unreachable_signed_originals_are_not_silently_accepted_as_part_of_the_selected_history() {
    let mut f = fixture();
    let mut unrelated = f.originals[0].verify().expect("original");
    let source = unrelated.source_state().expect("source").expect("state");
    let changed = State::new_snapshot(source.tree, vec![], source.attribution)
        .with_intent("unrelated signed root");
    f.store
        .put_state(&changed)
        .expect("unrelated immutable state");
    let ThreadOperationBody::Capture(capture) = &mut unrelated.body else {
        panic!("capture")
    };
    capture.result.state = changed.encode_current_msgpack().expect("State");
    f.originals
        .push(SignedOperation::sign(&unrelated, &f.signer).expect("unrelated genuine signature"));
    let error = project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default())
        .err()
        .expect("exact selected history only");
    assert!(
        error.to_string().contains("unrelated source originals"),
        "{error}"
    );
}

#[test]
fn mixed_native_and_imported_history_keeps_native_recipe_seed_and_exact_received_git_bytes() {
    use objects::object::thread_replication::{
        git_import_converter::{GitImportGraph, GitImportRawCommit},
        git_import_graph::{GitObjectFormat, GitObjectId},
    };
    use sley::{GitObjectType, Repository as GitRepository};
    let mut f = fixture();
    let native = project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default())
        .expect("native history control");
    let old = native.history().git_oid(&f.tip).expect("native tip OID");
    let sink = GitRepository::open(native.git_dir()).expect("isolated fixture Git");
    let blob = Blob::new(b"imported executable contents\n".to_vec());
    f.store.put_blob(&blob).expect("imported native blob");
    let git_blob = sink.write_blob(blob.content()).expect("Git blob");
    let tree = Tree::from_git_entries(vec![
        TreeEntry::file("imported.sh", blob.hash(), true).expect("executable entry"),
    ])
    .expect("native imported tree");
    f.store.put_tree(&tree).expect("store imported tree");
    let mut raw_tree = b"100755 imported.sh\0".to_vec();
    raw_tree.extend_from_slice(git_blob.as_bytes());
    let git_tree = sink
        .write_raw_object(GitObjectType::Tree, raw_tree)
        .expect("Git tree");
    let body = format!("tree {git_tree}\nparent {old}\nauthor Untrusted Git Author <untrusted@example.invalid> 1700000000 -0700\ncommitter Other Git Committer <committer@example.invalid> 1700000001 +0530\nencoding UTF-8\n\nImported message keeps its exact bytes.\n").into_bytes();
    let new = sink
        .write_raw_object(GitObjectType::Commit, body.clone())
        .expect("original imported Git commit");
    let oid = GitObjectId::Sha1(new.as_bytes().try_into().expect("SHA1"));
    let imported = GitImportGraph::convert_raw_commit(
        GitImportRawCommit {
            oid: &oid,
            object_format: GitObjectFormat::Sha1,
            raw_commit: &body,
            heddle_note: None,
        },
        tree.hash(),
        vec![f.tip],
        false,
        |_| Ok(None),
    )
    .expect("byte-faithful native imported State");
    assert!(!imported.git_lossy);
    f.store.put_state(&imported).expect("imported State");
    let previous = f
        .originals
        .last()
        .expect("native tip original")
        .verify()
        .expect("signature");
    let operation = ThreadOperation {
        version: 1,
        thread: previous.thread,
        parents: BTreeSet::from([previous.id().expect("operation ID")]),
        publisher: f.signer.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(AuthoredCapture::local(
            imported
                .encode_current_msgpack()
                .expect("canonical State")
                .into(),
        )),
    };
    f.originals
        .push(SignedOperation::sign(&operation, &f.signer).expect("actual capture signature"));
    let projection =
        project_received_git_history(&f.store, &f.originals, imported.id(), ViewLimits::default())
            .expect("mixed native/imported receiver history");
    assert_eq!(
        projection.history().git_oid(&f.tip),
        Some(old),
        "native no-hosted-URL recipe stays unchanged"
    );
    assert_eq!(projection.history().git_oid(&imported.id()), Some(new));
    assert_eq!(
        projection.history().states().len(),
        3,
        "synthetic initialization has no Git commit"
    );
    let git = GitRepository::open(projection.git_dir()).expect("fresh reconstructed Git sink");
    assert_eq!(
        git.read_object(&new)
            .expect("reconstructed imported commit")
            .body
            .as_slice(),
        body.as_slice()
    );
    verify_received_git_history(
        &f.store,
        &f.originals,
        f.tip,
        imported.id(),
        &old.as_bytes().try_into().expect("SHA1"),
        &new.as_bytes().try_into().expect("SHA1"),
        ViewLimits::default(),
    )
    .expect("exact native to imported fast-forward proof");
}

#[test]
fn schema_defined_seed_may_be_absent_but_cannot_be_replaced_by_unhashed_metadata() {
    let f = fixture();
    let mut altered = synthetic_initial_base().expect("canonical sentinel");
    let expected = altered.id();
    altered.git_lossy = true;
    assert_eq!(
        altered.id(),
        expected,
        "flag is deliberately outside State identity"
    );
    f.store
        .put_state(&altered)
        .expect("hostile supplied sentinel");
    assert!(
        project_received_git_history(&f.store, &f.originals, f.tip, ViewLimits::default()).is_err(),
        "derived missing sentinel never permits a supplied noncanonical replacement"
    );
}
