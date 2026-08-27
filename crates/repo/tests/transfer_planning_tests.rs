// SPDX-License-Identifier: Apache-2.0
//! Transfer-planning tests. Relocated from wire's `object_graph` tests when
//! graph planning moved into the objects crate; these exercise planning against
//! a real repository store.

use std::{
    collections::HashSet,
    sync::atomic::{AtomicUsize, Ordering},
};

use chrono::Utc;
use objects::{
    error::HeddleError,
    object::{
        Action, ActionId, AnnotatedTag, AnnotatedTagMarker, Attribution, Blob, ContentHash,
        Discussion, DiscussionResolution, DiscussionTurn, DiscussionsBlob, Principal,
        PurgeEvidence, Redaction, State, StateAttachment, StateAttachmentBody, StateId,
        StateSignature, StateVisibility, SymbolAnchor, Tree, TreeEntry, VisibilityTier,
    },
    store::{FsStore, ObjectStore, Result as StoreResult, SidecarStore},
    transfer::{
        ObjectId, ObjectInfo, ObjectType, PlannedObject, StateClosureOptions,
        enumerate_state_closure_plan_with_options,
        enumerate_state_closure_transfer_from_boundaries,
        enumerate_state_closure_transfer_with_options, enumerate_state_closure_with_options,
        missing_blobs_in_tree,
    },
};
use repo::Repository;
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};
use tempfile::TempDir;

fn pairs_from_full(objects: &[ObjectInfo]) -> HashSet<(ObjectId, ObjectType)> {
    objects
        .iter()
        .map(|info| (info.id.clone(), info.obj_type))
        .collect()
}

fn pairs_from_plan(objects: &[PlannedObject]) -> HashSet<(ObjectId, ObjectType)> {
    objects
        .iter()
        .map(|info| (info.id.clone(), info.obj_type))
        .collect()
}

fn object_info_fingerprint(
    objects: &[ObjectInfo],
) -> Vec<(ObjectId, ObjectType, u64, Option<ContentHash>)> {
    objects
        .iter()
        .map(|info| (info.id.clone(), info.obj_type, info.size, info.delta_base))
        .collect()
}

fn assert_plan_parity(
    repo: &Repository,
    state_id: StateId,
    options: StateClosureOptions,
) -> HashSet<(ObjectId, ObjectType)> {
    let full =
        enumerate_state_closure_with_options(repo.store(), state_id, options.clone()).unwrap();
    let plan = enumerate_state_closure_plan_with_options(repo.store(), state_id, options).unwrap();

    let full_pairs = pairs_from_full(&full);
    let plan_pairs = pairs_from_plan(&plan);
    assert_eq!(full_pairs, plan_pairs);
    full_pairs
}

fn assert_contains_object(
    objects: &HashSet<(ObjectId, ObjectType)>,
    id: ObjectId,
    obj_type: ObjectType,
) {
    assert!(
        objects.contains(&(id.clone(), obj_type)),
        "expected closure to contain {id:?} as {obj_type:?}: {objects:?}"
    );
}

#[test]
fn depth_one_transfer_includes_hdc1_anchor_and_installs_a_readable_tip() {
    let source_temp = TempDir::new().unwrap();
    let source = Repository::init_default(source_temp.path()).unwrap();
    for index in 0..128 {
        std::fs::write(
            source_temp.path().join(format!("fixture-{index:03}.txt")),
            format!("unchanged-{index}\n"),
        )
        .unwrap();
    }
    let root = source_temp.path().join("root.txt");
    std::fs::write(&root, "v1\n").unwrap();
    let anchor = source.snapshot(Some("anchor".to_string()), None).unwrap();
    std::fs::write(&root, "v2\n").unwrap();
    let middle = source.snapshot(Some("middle".to_string()), None).unwrap();
    std::fs::write(&root, "v3\n").unwrap();
    let tip = source.snapshot(Some("tip".to_string()), None).unwrap();

    let objects = enumerate_state_closure_with_options(
        source.store(),
        tip.state_id,
        StateClosureOptions {
            depth: Some(1),
            exclude_states: Vec::new(),
        },
    )
    .unwrap();
    let bundle = wire::build_native_pack(source.store(), &objects).unwrap();
    let destination_temp = TempDir::new().unwrap();
    let destination = FsStore::new(destination_temp.path().join(".heddle"));
    destination.init().unwrap();

    wire::install_received_pack(&destination, &bundle.pack_data, &bundle.index_data).unwrap();
    assert_eq!(
        destination.get_tree(&tip.tree).unwrap(),
        source.store().get_tree(&tip.tree).unwrap(),
        "the transferred HDC1 tip must reconstruct after installation",
    );

    let tip_info = objects
        .iter()
        .find(|info| info.id == ObjectId::Hash(tip.tree) && info.obj_type == ObjectType::Tree)
        .unwrap();
    assert_eq!(tip_info.delta_base, Some(anchor.tree));
    assert!(objects.iter().any(|info| {
        info.id == ObjectId::Hash(anchor.tree) && info.obj_type == ObjectType::Tree
    }));
    assert!(objects.iter().any(|info| {
        info.id == ObjectId::StateId(middle.state_id) && info.obj_type == ObjectType::State
    }));
    assert!(!objects.iter().any(|info| {
        info.id == ObjectId::StateId(anchor.state_id) && info.obj_type == ObjectType::State
    }));
}

struct CountingStore<'a, S> {
    inner: &'a S,
    state_reads: AtomicUsize,
}

impl<'a, S> CountingStore<'a, S> {
    fn new(inner: &'a S) -> Self {
        Self {
            inner,
            state_reads: AtomicUsize::new(0),
        }
    }

    fn state_reads(&self) -> usize {
        self.state_reads.load(Ordering::SeqCst)
    }
}

impl<S: SidecarStore> SidecarStore for CountingStore<'_, S> {}

impl<S: ObjectStore> ObjectStore for CountingStore<'_, S> {
    fn get_blob(&self, hash: &ContentHash) -> StoreResult<Option<Blob>> {
        self.inner.get_blob(hash)
    }

    fn put_blob(&self, blob: &Blob) -> StoreResult<ContentHash> {
        self.inner.put_blob(blob)
    }

    fn has_blob(&self, hash: &ContentHash) -> StoreResult<bool> {
        self.inner.has_blob(hash)
    }

    fn get_tree(&self, hash: &ContentHash) -> StoreResult<Option<Tree>> {
        self.inner.get_tree(hash)
    }

    fn put_tree(&self, tree: &Tree) -> StoreResult<ContentHash> {
        self.inner.put_tree(tree)
    }

    fn has_tree(&self, hash: &ContentHash) -> StoreResult<bool> {
        self.inner.has_tree(hash)
    }

    fn get_state(&self, id: &StateId) -> StoreResult<Option<State>> {
        self.state_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_state(id)
    }

    fn put_state(&self, state: &State) -> StoreResult<()> {
        self.inner.put_state(state)
    }

    fn has_state(&self, id: &StateId) -> StoreResult<bool> {
        self.inner.has_state(id)
    }

    fn list_states(&self) -> StoreResult<Vec<StateId>> {
        self.inner.list_states()
    }

    fn get_action(&self, id: &ActionId) -> StoreResult<Option<Action>> {
        self.inner.get_action(id)
    }

    fn put_action(&self, action: &mut Action) -> StoreResult<ActionId> {
        self.inner.put_action(action)
    }

    fn list_actions(&self) -> StoreResult<Vec<ActionId>> {
        self.inner.list_actions()
    }

    fn list_blobs(&self) -> StoreResult<Vec<ContentHash>> {
        self.inner.list_blobs()
    }

    fn list_trees(&self) -> StoreResult<Vec<ContentHash>> {
        self.inner.list_trees()
    }
}

fn test_attribution() -> Attribution {
    Attribution::human(Principal::new("Graph Tester", "graph@example.com"))
}

#[test]
fn lean_closure_planner_matches_object_info_ids_and_types() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let lean = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    let full_pairs = full
        .into_iter()
        .map(|info| (info.id, info.obj_type))
        .collect::<std::collections::HashSet<_>>();
    let lean_pairs = lean
        .into_iter()
        .map(|info| (info.id, info.obj_type))
        .collect::<std::collections::HashSet<_>>();

    assert_eq!(full_pairs, lean_pairs);
    assert!(
        full_pairs
            .iter()
            .any(|(id, _)| matches!(id, ObjectId::StateId(_)))
    );
}

#[test]
fn state_closure_includes_annotated_tag_chain() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let state = repo.snapshot(Some("tagged".to_string()), None).unwrap();
    let inner = AnnotatedTag::new(
            GitObjectFormat::Sha1,
            b"object 1111111111111111111111111111111111111111\ntype commit\ntag inner\ntagger Test <test@example.com> 1700000000 +0100\n\ninner\n".to_vec(),
            None,
            None,
        )
        .unwrap();
    let inner_hash = repo.store().put_annotated_tag(&inner).unwrap();
    let outer = AnnotatedTag::new(
            GitObjectFormat::Sha1,
            b"object 2222222222222222222222222222222222222222\ntype tag\ntag outer\ntagger Test <test@example.com> 1700000001 -0730\n\nouter\n".to_vec(),
            Some(inner_hash),
            Some(AnnotatedTagMarker {
                name: "outer".to_string(),
                peeled_state: state.state_id,
            }),
        )
        .unwrap();
    let outer_hash = repo.store().put_annotated_tag(&outer).unwrap();

    let closure = assert_plan_parity(&repo, state.state_id, StateClosureOptions::default());
    assert_contains_object(
        &closure,
        ObjectId::Hash(inner_hash),
        ObjectType::AnnotatedTag,
    );
    assert_contains_object(
        &closure,
        ObjectId::Hash(outer_hash),
        ObjectType::AnnotatedTag,
    );
}

#[test]
fn transfer_boundary_stops_at_server_head_without_walking_its_history() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let path = temp.path().join("story.txt");

    std::fs::write(&path, "base\n").unwrap();
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();
    std::fs::write(&path, "middle\n").unwrap();
    let middle = repo.snapshot(Some("middle".to_string()), None).unwrap();
    std::fs::write(&path, "tip\n").unwrap();
    let tip = repo.snapshot(Some("tip".to_string()), None).unwrap();

    let counting = CountingStore::new(repo.store());
    let transfer = enumerate_state_closure_transfer_from_boundaries(
        &counting,
        tip.state_id,
        &[middle.state_id],
        512,
    )
    .unwrap();
    let states = transfer
        .planned_objects
        .iter()
        .filter_map(|object| match object.id {
            ObjectId::StateId(state) if object.obj_type == ObjectType::State => Some(state),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(states, vec![tip.state_id]);
    assert!(!states.contains(&middle.state_id));
    assert!(!states.contains(&base.state_id));
    assert_eq!(
        counting.state_reads(),
        1,
        "the advertised server boundary must stop the walk before reading old states"
    );
}

#[test]
fn transfer_projection_matches_full_and_plan_on_mixed_state_closure_fixture() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    let excluded_blob = repo
        .store()
        .put_blob(&Blob::from("excluded"))
        .expect("put excluded blob");
    let excluded_tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("excluded.txt", excluded_blob, false).unwrap(),
        ]))
        .expect("put excluded tree");
    let excluded_parent = State::new(excluded_tree_hash, Vec::new(), test_attribution());
    repo.store()
        .put_state(&excluded_parent)
        .expect("put excluded parent");

    let redacted_blob = repo
        .store()
        .put_blob(&Blob::from("secret"))
        .expect("put redacted blob");
    let nested_blob = repo
        .store()
        .put_blob(&Blob::from("nested"))
        .expect("put nested blob");
    let symlink_blob = repo
        .store()
        .put_blob(&Blob::from("target"))
        .expect("put symlink blob");
    let context_blob = repo
        .store()
        .put_blob(&Blob::from("context"))
        .expect("put context blob");
    let provenance_blob = repo
        .store()
        .put_blob(&Blob::from("provenance"))
        .expect("put provenance blob");
    let risk_blob = repo
        .store()
        .put_blob(&Blob::from("risk"))
        .expect("put risk blob");
    let review_blob = repo
        .store()
        .put_blob(&Blob::from("review"))
        .expect("put review blob");
    let discussions_blob = repo
        .store()
        .put_blob(&Blob::from("discussion"))
        .expect("put discussion blob");
    let conflicts_blob = repo
        .store()
        .put_blob(&Blob::from("conflicts"))
        .expect("put conflicts blob");

    let nested_tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("nested.txt", nested_blob, false).unwrap(),
            TreeEntry::symlink("latest", symlink_blob).unwrap(),
        ]))
        .expect("put nested tree");
    let context_tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("context.txt", context_blob, false).unwrap(),
        ]))
        .expect("put context tree");
    let provenance_tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("lineage.txt", provenance_blob, false).unwrap(),
        ]))
        .expect("put provenance tree");
    let gitlink_target: GitObjectId = "0303030303030303030303030303030303030303"
        .parse()
        .expect("git oid");
    let root_tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("secret.txt", redacted_blob, false).unwrap(),
            TreeEntry::directory("nested", nested_tree_hash).unwrap(),
            TreeEntry::gitlink("vendor", gitlink_target).unwrap(),
        ]))
        .expect("put root tree");
    let state = State::new(
        root_tree_hash,
        vec![excluded_parent.state_id],
        test_attribution(),
    )
    .with_provenance(provenance_tree_hash);
    repo.store().put_state(&state).expect("put state");
    for body in [
        StateAttachmentBody::Context(context_tree_hash),
        StateAttachmentBody::RiskSignals(risk_blob),
        StateAttachmentBody::ReviewSignatures(review_blob),
        StateAttachmentBody::Discussions(discussions_blob),
        StateAttachmentBody::StructuredConflicts(conflicts_blob),
    ] {
        repo.put_state_attachment(&StateAttachment {
            state_id: state.id(),
            body,
            attribution: state.attribution.clone(),
            created_at: Utc::now(),
            supersedes: None,
        })
        .unwrap();
    }

    repo.put_redaction(Redaction {
        redacted_blob,
        state: state.state_id,
        path: "secret.txt".to_string(),
        reason: "test leak".to_string(),
        redactor: Principal::new("Tester", "tester@example.test"),
        redacted_at: Utc::now(),
        signature: None,
        purge: None,
        supersedes: None,
    })
    .expect("put redaction");
    repo.put_state_visibility(StateVisibility {
        state: state.state_id,
        tier: VisibilityTier::Restricted {
            scope_label: "security".to_string(),
        },
        embargo_until: None,
        declarer: Principal::new("Tester", "tester@example.test"),
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .expect("put visibility");

    let options = StateClosureOptions {
        depth: None,
        exclude_states: vec![excluded_parent.state_id],
    };
    let transfer = enumerate_state_closure_transfer_with_options(
        repo.store(),
        state.state_id,
        options.clone(),
        512,
    )
    .expect("transfer projection");

    let full = enumerate_state_closure_with_options(repo.store(), state.state_id, options.clone())
        .expect("full closure");
    let plan = enumerate_state_closure_plan_with_options(repo.store(), state.state_id, options)
        .expect("plan closure");
    assert_eq!(
        transfer
            .full_objects
            .as_deref()
            .map(object_info_fingerprint),
        Some(object_info_fingerprint(&full))
    );
    assert_eq!(transfer.planned_objects, plan);

    let full_pairs = pairs_from_full(&full);
    assert_eq!(full_pairs, pairs_from_plan(&plan));
    assert_contains_object(
        &full_pairs,
        ObjectId::StateId(state.state_id),
        ObjectType::State,
    );
    assert_contains_object(
        &full_pairs,
        ObjectId::StateId(state.state_id),
        ObjectType::StateVisibility,
    );
    assert_contains_object(&full_pairs, ObjectId::Hash(redacted_blob), ObjectType::Blob);
    assert_contains_object(
        &full_pairs,
        ObjectId::Hash(redacted_blob),
        ObjectType::Redaction,
    );
    for hash in [
        root_tree_hash,
        nested_tree_hash,
        context_tree_hash,
        provenance_tree_hash,
    ] {
        assert_contains_object(&full_pairs, ObjectId::Hash(hash), ObjectType::Tree);
    }
    for hash in [
        nested_blob,
        symlink_blob,
        context_blob,
        provenance_blob,
        risk_blob,
        review_blob,
        discussions_blob,
        conflicts_blob,
    ] {
        assert_contains_object(&full_pairs, ObjectId::Hash(hash), ObjectType::Blob);
    }
    assert!(!full_pairs.contains(&(
        ObjectId::StateId(excluded_parent.state_id),
        ObjectType::State
    )));
    assert!(!full_pairs.contains(&(ObjectId::Hash(excluded_tree_hash), ObjectType::Tree)));
    assert!(!full_pairs.contains(&(ObjectId::Hash(excluded_blob), ObjectType::Blob)));
}

#[test]
fn transfer_projection_reads_root_state_once_on_small_transfer() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let blob = repo
        .store()
        .put_blob(&Blob::from("hello\n"))
        .expect("put blob");
    let tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("README.md", blob, false).unwrap(),
        ]))
        .expect("put tree");
    let state = State::new(tree_hash, Vec::new(), test_attribution());
    repo.store().put_state(&state).expect("put state");
    let store = CountingStore::new(repo.store());

    let transfer = enumerate_state_closure_transfer_with_options(
        &store,
        state.state_id,
        StateClosureOptions::default(),
        512,
    )
    .expect("transfer projection");

    assert!(
        !transfer.planned_objects.is_empty(),
        "lean projection should be available"
    );
    assert!(transfer.full_objects.is_some());
    assert_eq!(
        store.state_reads(),
        1,
        "small transfer projection must not read the root state through a second closure walk"
    );
}

#[test]
fn transfer_projection_drops_full_descriptors_after_threshold() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    let transfer = enumerate_state_closure_transfer_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
        0,
    )
    .expect("transfer projection");

    assert!(
        !transfer.planned_objects.is_empty(),
        "lean projection should still be available over the threshold"
    );
    assert!(transfer.full_objects.is_none());
}

#[test]
fn depth_and_exclude_options_match_between_full_and_plan() {
    use std::collections::BTreeMap;

    use objects::object::{BindingDelta, SemanticIndexRoot, SemanticTreeNode};

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let path = temp.path().join("story.txt");

    std::fs::write(&path, "base\n").unwrap();
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();
    std::fs::write(&path, "middle\n").unwrap();
    let middle = repo.snapshot(Some("middle".to_string()), None).unwrap();
    std::fs::write(&path, "tip\n").unwrap();
    let tip = repo.snapshot(Some("tip".to_string()), None).unwrap();

    let (semantic_tree, semantic_digest) = SemanticTreeNode::new(Vec::new());
    let semantic_tree_hash = repo
        .store()
        .put_blob(&Blob::new(semantic_tree.encode().unwrap()))
        .unwrap();
    let attach_delta = |state: StateId, parent: Option<ContentHash>| {
        let delta = BindingDelta::new(parent, Vec::new());
        let delta_hash = repo
            .store()
            .put_blob(&Blob::new(delta.encode().unwrap()))
            .unwrap();
        let root = SemanticIndexRoot::new(1, BTreeMap::new(), semantic_tree_hash, semantic_digest)
            .with_binding_delta(delta_hash, 1);
        let root_hash = repo
            .store()
            .put_blob(&Blob::new(root.encode().unwrap()))
            .unwrap();
        repo.put_state_attachment(&StateAttachment {
            state_id: state,
            body: StateAttachmentBody::SemanticIndex(root_hash),
            attribution: test_attribution(),
            created_at: Utc::now(),
            supersedes: None,
        })
        .unwrap();
        delta_hash
    };
    let base_delta = attach_delta(base.state_id, None);
    let middle_delta = attach_delta(middle.state_id, Some(base_delta));
    let tip_delta = attach_delta(tip.state_id, Some(middle_delta));

    let depth_zero = assert_plan_parity(
        &repo,
        tip.state_id,
        StateClosureOptions {
            depth: Some(0),
            exclude_states: Vec::new(),
        },
    );
    assert!(depth_zero.contains(&(ObjectId::StateId(tip.state_id), ObjectType::State)));
    assert!(!depth_zero.contains(&(ObjectId::StateId(middle.state_id), ObjectType::State)));
    assert!(!depth_zero.contains(&(ObjectId::StateId(base.state_id), ObjectType::State)));
    assert!(depth_zero.contains(&(ObjectId::Hash(tip_delta), ObjectType::Blob)));
    assert!(!depth_zero.contains(&(ObjectId::Hash(middle_delta), ObjectType::Blob)));
    assert!(!depth_zero.contains(&(ObjectId::Hash(base_delta), ObjectType::Blob)));

    let depth_one = assert_plan_parity(
        &repo,
        tip.state_id,
        StateClosureOptions {
            depth: Some(1),
            exclude_states: Vec::new(),
        },
    );
    assert!(depth_one.contains(&(ObjectId::StateId(tip.state_id), ObjectType::State)));
    assert!(depth_one.contains(&(ObjectId::StateId(middle.state_id), ObjectType::State)));
    assert!(!depth_one.contains(&(ObjectId::StateId(base.state_id), ObjectType::State)));
    assert!(depth_one.contains(&(ObjectId::Hash(tip_delta), ObjectType::Blob)));
    assert!(depth_one.contains(&(ObjectId::Hash(middle_delta), ObjectType::Blob)));
    assert!(!depth_one.contains(&(ObjectId::Hash(base_delta), ObjectType::Blob)));

    let exclude_middle = assert_plan_parity(
        &repo,
        tip.state_id,
        StateClosureOptions {
            depth: None,
            exclude_states: vec![middle.state_id],
        },
    );
    assert!(exclude_middle.contains(&(ObjectId::StateId(tip.state_id), ObjectType::State)));
    assert!(!exclude_middle.contains(&(ObjectId::StateId(middle.state_id), ObjectType::State)));
    assert!(!exclude_middle.contains(&(ObjectId::StateId(base.state_id), ObjectType::State)));
}

#[test]
fn shared_tree_and_blob_references_are_emitted_once() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    let shared_blob = Blob::from("shared contents\n");
    let shared_blob_hash = repo.store().put_blob(&shared_blob).unwrap();
    let shared_tree = Tree::from_entries(vec![
        TreeEntry::file("shared.txt", shared_blob_hash, false).unwrap(),
    ]);
    let shared_tree_hash = repo.store().put_tree(&shared_tree).unwrap();
    let root = Tree::from_entries(vec![
        TreeEntry::directory("left", shared_tree_hash).unwrap(),
        TreeEntry::directory("right", shared_tree_hash).unwrap(),
    ]);
    let root_hash = repo.store().put_tree(&root).unwrap();
    let state = State::new(root_hash, Vec::new(), test_attribution());
    repo.store().put_state(&state).unwrap();

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    assert_eq!(
        pairs_from_full(&full),
        pairs_from_plan(&plan),
        "full and lean closure enumerators must dedup the same objects"
    );

    assert_eq!(
            full.iter()
                .filter(|info| info.id == ObjectId::Hash(root_hash)
                    && info.obj_type == ObjectType::Tree)
                .count(),
            1
        );
    assert_eq!(
        full.iter()
            .filter(|info| info.id == ObjectId::Hash(shared_tree_hash)
                && info.obj_type == ObjectType::Tree)
            .count(),
        1
    );
    assert_eq!(
        full.iter()
            .filter(|info| info.id == ObjectId::Hash(shared_blob_hash)
                && info.obj_type == ObjectType::Blob)
            .count(),
        1
    );
}

#[test]
fn state_closure_skips_gitlink_targets() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let target: GitObjectId = "0303030303030303030303030303030303030303"
        .parse()
        .expect("git oid");
    let root = Tree::from_entries(vec![
        TreeEntry::gitlink("vendor", target).expect("gitlink entry"),
    ]);
    let root_hash = repo.store().put_tree(&root).unwrap();
    let state = State::new(root_hash, Vec::new(), test_attribution());
    repo.store().put_state(&state).unwrap();

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    assert_eq!(pairs_from_full(&full), pairs_from_plan(&plan));
    assert!(
        !full.iter().any(|info| info.obj_type == ObjectType::Blob),
        "gitlinks carry foreign Git commit ids, not Heddle blob dependencies: {full:?}"
    );
    assert!(
        full.iter().any(|info| {
            info.id == ObjectId::Hash(root_hash) && info.obj_type == ObjectType::Tree
        })
    );
}

#[test]
fn missing_blobs_in_tree_skips_gitlinks_and_walks_nested_side_paths() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let present_blob = repo
        .store()
        .put_blob(&Blob::from("already local"))
        .expect("put present blob");
    let missing_nested = ContentHash::from_bytes([7; 32]);
    let missing_symlink = ContentHash::from_bytes([8; 32]);
    let nested_tree = Tree::from_entries(vec![
        TreeEntry::file("remote.txt", missing_nested, false).unwrap(),
        TreeEntry::symlink("remote-link", missing_symlink).unwrap(),
    ]);
    let nested_tree_hash = repo
        .store()
        .put_tree(&nested_tree)
        .expect("put nested tree");
    let gitlink_target: GitObjectId = "0404040404040404040404040404040404040404"
        .parse()
        .expect("git oid");
    let root = Tree::from_entries(vec![
        TreeEntry::file("local.txt", present_blob, false).unwrap(),
        TreeEntry::directory("nested", nested_tree_hash).unwrap(),
        TreeEntry::gitlink("vendor", gitlink_target).unwrap(),
    ]);
    let root_hash = repo.store().put_tree(&root).expect("put root tree");

    let missing = missing_blobs_in_tree(repo.store(), root_hash).expect("missing blobs");

    assert_eq!(
        missing.into_iter().collect::<HashSet<_>>(),
        HashSet::from([missing_nested, missing_symlink])
    );
}

/// Once a redaction is declared for a blob in a snapshot, the
/// state closure must include an `ObjectType::Redaction` entry
/// keyed on that blob's hash — that's the wire-side signal the
/// receiver replays.
#[test]
fn enumerate_state_closure_emits_redaction_for_redacted_blob() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("secret.toml"), "api_token = \"x\"\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    // Find the blob hash for secret.toml by walking the snapshot's tree.
    let tree = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .expect("tree present");
    let blob_hash = tree
        .iter()
        .find(|e| e.name() == "secret.toml")
        .expect("entry present")
        .blob_hash()
        .expect("secret.toml is a blob");

    let redaction = Redaction {
        redacted_blob: blob_hash,
        state: state.state_id,
        path: "secret.toml".to_string(),
        reason: "test leak".to_string(),
        redactor: Principal {
            name: "Tester".into(),
            email: "tester@heddle.sh".into(),
        },
        redacted_at: Utc::now(),
        signature: None,
        purge: None,
        supersedes: None,
    };
    repo.put_redaction(redaction).unwrap();

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    assert!(
        full.iter()
            .any(|info| info.obj_type == ObjectType::Redaction
                && info.id == ObjectId::Hash(blob_hash)),
        "full closure must include a Redaction entry for the redacted blob"
    );
    assert!(
        plan.iter()
            .any(|p| p.obj_type == ObjectType::Redaction && p.id == ObjectId::Hash(blob_hash)),
        "plan closure must include a Redaction entry for the redacted blob"
    );
}

#[test]
fn missing_merely_redacted_blob_still_fails_closure_planning() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("secret.toml"), "api_token = \"x\"\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
    let blob_hash = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .unwrap()
        .iter()
        .find(|entry| entry.name() == "secret.toml")
        .unwrap()
        .blob_hash()
        .unwrap();
    repo.put_redaction(Redaction {
        redacted_blob: blob_hash,
        state: state.state_id,
        path: "secret.toml".to_string(),
        reason: "test leak".to_string(),
        redactor: Principal::new("Tester", "tester@heddle.sh"),
        redacted_at: Utc::now(),
        signature: None,
        purge: None,
        supersedes: None,
    })
    .unwrap();
    let store = repo.store();
    store.remove_blob_everywhere(&blob_hash).unwrap();

    let error = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .expect_err("redaction without purge authority must not excuse missing bytes");
    assert!(matches!(
        error,
        HeddleError::MissingObject { object_type, .. }
            if object_type == "blob"
    ));
}

#[test]
fn purged_blob_closure_carries_sidecar_without_deleted_bytes() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("secret.toml"), "api_token = \"x\"\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
    let blob_hash = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .unwrap()
        .iter()
        .find(|entry| entry.name() == "secret.toml")
        .unwrap()
        .blob_hash()
        .unwrap();
    repo.put_redaction(Redaction {
        redacted_blob: blob_hash,
        state: state.state_id,
        path: "secret.toml".to_string(),
        reason: "test leak".to_string(),
        redactor: Principal::new("Tester", "tester@heddle.sh"),
        redacted_at: Utc::now(),
        signature: None,
        purge: Some(PurgeEvidence {
            purger: Principal::new("Owner", "owner@heddle.sh"),
            purged_at: Utc::now(),
            signature: StateSignature {
                algorithm: "ed25519".to_string(),
                public_key: "11".repeat(32),
                signature: "22".repeat(64),
            },
        }),
        supersedes: None,
    })
    .unwrap();
    let store = repo.store();
    store.remove_blob_everywhere(&blob_hash).unwrap();

    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .expect("purge sidecar replaces intentionally deleted blob in closure");
    assert!(!plan.iter().any(|object| {
        object.id == ObjectId::Hash(blob_hash) && object.obj_type == ObjectType::Blob
    }));
    assert!(plan.iter().any(|object| {
        object.id == ObjectId::Hash(blob_hash) && object.obj_type == ObjectType::Redaction
    }));
}

#[test]
fn enumerate_state_closure_emits_state_visibility_for_visible_state() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    repo.put_state_visibility(StateVisibility {
        state: state.state_id,
        tier: VisibilityTier::Restricted {
            scope_label: "security-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Tester".into(),
            email: "tester@heddle.sh".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    assert!(
        full.iter()
            .any(|info| info.obj_type == ObjectType::StateVisibility
                && info.id == ObjectId::StateId(state.state_id)),
        "full closure must include a StateVisibility entry for the visible state"
    );
    assert!(
        plan.iter()
            .any(|p| p.obj_type == ObjectType::StateVisibility
                && p.id == ObjectId::StateId(state.state_id)),
        "plan closure must include a StateVisibility entry for the visible state"
    );
}

#[test]
fn enumerate_state_closure_emits_state_metadata_blobs() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    let principal = Principal::new("Tester", "tester@example.test");
    let discussion_bytes = DiscussionsBlob::new(vec![Discussion {
        id: "disc-1".to_string(),
        anchor: SymbolAnchor::new("src/lib.rs", "answer"),
        opened_against_state: state.state_id,
        opened_at: 1_782_400_000,
        thread_ref: None,
        turns: vec![DiscussionTurn {
            author: principal,
            body: "Should this sync?".to_string(),
            posted_at: 1_782_400_000,
            references: Vec::new(),
        }],
        resolution: DiscussionResolution::Open,
        body_changed_since_open: false,
        orphaned: false,
        visibility: VisibilityTier::default(),
        resolved_annotation_id: None,
    }])
    .encode()
    .expect("encode discussions");
    let discussion_hash = repo
        .store()
        .put_blob(&Blob::new(discussion_bytes))
        .expect("put discussions blob");
    let risk_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"risk signals"))
        .expect("put risk blob");
    let review_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"review signatures"))
        .expect("put review blob");
    let conflicts_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"structured conflicts"))
        .expect("put conflicts blob");
    for body in [
        StateAttachmentBody::RiskSignals(risk_hash),
        StateAttachmentBody::ReviewSignatures(review_hash),
        StateAttachmentBody::Discussions(discussion_hash),
        StateAttachmentBody::StructuredConflicts(conflicts_hash),
    ] {
        repo.put_state_attachment(&StateAttachment {
            state_id: state.id(),
            body,
            attribution: state.attribution.clone(),
            created_at: Utc::now(),
            supersedes: None,
        })
        .unwrap();
    }

    let full = enumerate_state_closure_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();
    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    for metadata_hash in [risk_hash, review_hash, discussion_hash, conflicts_hash] {
        assert!(
            full.iter().any(|info| info.obj_type == ObjectType::Blob
                && info.id == ObjectId::Hash(metadata_hash)),
            "full closure must include state metadata blob {metadata_hash}"
        );
        assert!(
            plan.iter()
                .any(|p| p.obj_type == ObjectType::Blob && p.id == ObjectId::Hash(metadata_hash)),
            "plan closure must include state metadata blob {metadata_hash}"
        );
    }
}

/// A pushed state's semantic-index attachment RECORD must be excluded from
/// the push pack (it rides the sidecar lane) while its semantic-index
/// content blobs still ride the pack in both directions, and the same
/// record stays packable server->client on pull.
#[test]
fn semantic_index_attachment_excluded_from_push_pack_but_kept_for_pull() {
    use std::collections::BTreeMap;

    use objects::object::{BindingDelta, FileBindingDelta, SemanticIndexRoot, SemanticTreeNode};

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

    // Minimal valid semantic-index fixture: an empty tree node under a root.
    let (node, node_digest) = SemanticTreeNode::new(Vec::new());
    let node_hash = repo
        .store()
        .put_blob(&Blob::new(node.encode().unwrap()))
        .expect("put semantic tree node");
    let base_delta = BindingDelta::new(
        None,
        vec![FileBindingDelta::new(
            "unreachable-parent.rs",
            None,
            Vec::new(),
        )],
    );
    let base_delta_hash = repo
        .store()
        .put_blob(&Blob::new(base_delta.encode().unwrap()))
        .expect("put base binding delta");
    let delta = BindingDelta::new(Some(base_delta_hash), Vec::new());
    let delta_hash = repo
        .store()
        .put_blob(&Blob::new(delta.encode().unwrap()))
        .expect("put binding delta");
    let root = SemanticIndexRoot::new(1, BTreeMap::new(), node_hash, node_digest)
        .with_binding_delta(delta_hash, 1);
    let root_hash = repo
        .store()
        .put_blob(&Blob::new(root.encode().unwrap()))
        .expect("put semantic index root");
    repo.put_state_attachment(&StateAttachment {
        state_id: state.state_id,
        body: StateAttachmentBody::SemanticIndex(root_hash),
        attribution: test_attribution(),
        created_at: Utc::now(),
        supersedes: None,
    })
    .unwrap();

    let plan = enumerate_state_closure_plan_with_options(
        repo.store(),
        state.state_id,
        StateClosureOptions::default(),
    )
    .unwrap();

    // Every StateAttachment record (at least the semantic index authored
    // above) is push-excluded and pull-included.
    let attachments: Vec<_> = plan
        .iter()
        .filter(|p| p.obj_type == ObjectType::StateAttachment)
        .collect();
    assert!(
        !attachments.is_empty(),
        "closure must contain the semantic-index attachment record"
    );
    for attachment in &attachments {
        assert!(matches!(attachment.id, ObjectId::StateAttachment { .. }));
        // Push: excluded from the pack (sidecar lane). Pull: kept in pack.
        assert!(
            !attachment.obj_type.packable_for_push(),
            "attachment record must be excluded from the push pack"
        );
        assert!(
            attachment.obj_type.packable_for_pull(),
            "attachment record must stay in the pull pack"
        );
    }

    // The semantic-index CONTENT blobs are ordinary content-addressed
    // objects and still ride the pack in both directions — only the
    // attachment record is sidecar'd on push.
    for content in [root_hash, node_hash, delta_hash] {
        let obj = plan
            .iter()
            .find(|p| p.id == ObjectId::Hash(content))
            .unwrap_or_else(|| panic!("semantic content blob {content} in closure"));
        assert_eq!(obj.obj_type, ObjectType::Blob);
        assert!(obj.obj_type.packable_for_push());
        assert!(obj.obj_type.packable_for_pull());
    }
    assert!(
        !plan
            .iter()
            .any(|object| object.id == ObjectId::Hash(base_delta_hash)),
        "a binding delta belonging to an unreachable parent state must not ride this state's closure"
    );

    // Partitioning the plan by push-packability puts the record on the
    // sidecar side and never in the pack side.
    let (push_pack, push_sidecar): (Vec<_>, Vec<_>) =
        plan.iter().partition(|p| p.obj_type.packable_for_push());
    assert!(
        push_sidecar
            .iter()
            .any(|p| p.obj_type == ObjectType::StateAttachment),
        "attachment record routed to the push sidecar partition"
    );
    assert!(
        !push_pack
            .iter()
            .any(|p| p.obj_type == ObjectType::StateAttachment),
        "attachment record must not be in the push pack partition"
    );
}
