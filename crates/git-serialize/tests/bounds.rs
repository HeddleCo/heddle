// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use heddle_git_serialize::{
    GitObjectKind, NativeObjectId, ObjectFormat, ObjectId, ObjectReader, ObjectSink,
    ReconstructionError, ReconstructionLimits, object_id, reconstruct_closure, serialize_blob,
    serialize_commit, serialize_tree,
};
use heddle_object_model::object::{
    Attribution, Blob, ContentHash, Principal, RawGitMode, SpoolId, State, StateId, Tree,
    TreeEntry, thread_replication::initial_base::synthetic_initial_base,
};

struct Reader {
    trees: BTreeMap<ContentHash, Tree>,
    state: Option<State>,
    blob: Blob,
    reads: AtomicUsize,
}

impl ObjectReader for Reader {
    type Error = &'static str;
    async fn read_state(&self, id: StateId, _: usize) -> Result<State, Self::Error> {
        self.state
            .as_ref()
            .filter(|state| state.id() == id)
            .cloned()
            .ok_or("missing state")
    }
    async fn read_tree(&self, id: ContentHash, _: usize) -> Result<Tree, Self::Error> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.trees.get(&id).cloned().ok_or("missing tree")
    }
    async fn read_blob(&self, id: ContentHash, max_bytes: usize) -> Result<Blob, Self::Error> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.blob.size() > max_bytes {
            return Err("reader byte limit");
        }
        if id != self.blob.hash() {
            return Err("missing blob");
        }
        Ok(self.blob.clone())
    }
}

#[derive(Default)]
struct Discard {
    count: usize,
}
impl ObjectSink for Discard {
    type Error = &'static str;
    async fn write_object(
        &mut self,
        _: NativeObjectId,
        _: GitObjectKind,
        _: ObjectId,
        _: &[u8],
    ) -> Result<(), Self::Error> {
        self.count += 1;
        Ok(())
    }
}

fn deep(depth: usize) -> (Reader, ContentHash, ObjectId) {
    let blob = Blob::from_slice(b"leaf");
    let mut native = blob.hash();
    let mut git =
        object_id(ObjectFormat::Sha1, GitObjectKind::Blob, blob.content()).expect("blob id");
    let mut trees = BTreeMap::new();
    for level in 0..depth {
        let entry = if level == 0 {
            TreeEntry::file("child", native, false)
        } else {
            TreeEntry::directory("child", native)
        }
        .expect("entry");
        let tree = Tree::from_entries(vec![entry]);
        // Independent raw Git layout, without the serializer under test.
        let mut body = if level == 0 {
            b"100644 child\0".to_vec()
        } else {
            b"40000 child\0".to_vec()
        };
        body.extend_from_slice(git.as_bytes());
        git = object_id(ObjectFormat::Sha1, GitObjectKind::Tree, &body).expect("tree id");
        native = tree.hash();
        trees.insert(native, tree);
    }
    (
        Reader {
            state: None,
            trees,
            blob,
            reads: AtomicUsize::new(0),
        },
        native,
        git,
    )
}

#[test]
fn deep_tree_on_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let (reader, root, expected) = deep(12_000);
            let mut sink = Discard::default();
            let limits = ReconstructionLimits {
                max_depth: 12_001,
                max_pending_links: 12_000,
                ..Default::default()
            };
            let actual = pollster::block_on(reconstruct_closure(
                &reader,
                &mut sink,
                NativeObjectId::Tree(root),
                ObjectFormat::Sha1,
                limits,
            ))
            .expect("deep iterative closure");
            assert_eq!(actual, expected);
            assert_eq!(sink.count, 12_001);
            assert_eq!(
                reader.reads.load(Ordering::Relaxed),
                12_001,
                "one read per distinct object"
            );
        })
        .expect("2 MiB stack thread")
        .join()
        .expect("stack-safe reconstruction");
}

#[test]
fn wide_tree_streams_shared_blobs_on_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let blob = Blob::from_slice(b"shared");
            let blob_id = object_id(ObjectFormat::Sha1, GitObjectKind::Blob, blob.content())
                .expect("blob oid");
            let mut expected_body = Vec::new();
            let entries = (0..20_000)
                .map(|n| {
                    let name = format!("file-{n:05}");
                    expected_body.extend_from_slice(format!("100644 {name}\0").as_bytes());
                    expected_body.extend_from_slice(blob_id.as_bytes());
                    TreeEntry::file(name, blob.hash(), false).expect("entry")
                })
                .collect();
            let tree = Tree::from_entries(entries);
            let root = tree.hash();
            let reader = Reader {
                state: None,
                trees: BTreeMap::from([(root, tree)]),
                blob,
                reads: AtomicUsize::new(0),
            };
            let mut sink = Discard::default();
            let actual = pollster::block_on(reconstruct_closure(
                &reader,
                &mut sink,
                NativeObjectId::Tree(root),
                ObjectFormat::Sha1,
                ReconstructionLimits::default(),
            ))
            .expect("wide closure");
            assert_eq!(
                actual,
                object_id(ObjectFormat::Sha1, GitObjectKind::Tree, &expected_body)
                    .expect("expected wide oid")
            );
            assert_eq!(sink.count, 2);
            assert_eq!(reader.reads.load(Ordering::Relaxed), 2);
        })
        .expect("thread")
        .join()
        .expect("wide stack safety");
}

#[test]
fn traversal_limits_fail_without_unbounded_reads() {
    let (reader, root, _) = deep(8);
    for (limits, reason, max_reads) in [
        (
            ReconstructionLimits {
                max_depth: 3,
                ..Default::default()
            },
            "depth",
            3,
        ),
        (
            ReconstructionLimits {
                max_pending_links: 2,
                ..Default::default()
            },
            "pending links",
            3,
        ),
        (
            ReconstructionLimits {
                max_objects: 4,
                ..Default::default()
            },
            "objects",
            4,
        ),
    ] {
        reader.reads.store(0, Ordering::Relaxed);
        let mut sink = Discard::default();
        let result = pollster::block_on(reconstruct_closure(
            &reader,
            &mut sink,
            NativeObjectId::Tree(root),
            ObjectFormat::Sha1,
            limits,
        ));
        assert!(
            matches!(result, Err(ReconstructionError::LimitExceeded(actual)) if actual == reason)
        );
        assert!(reader.reads.load(Ordering::Relaxed) <= max_reads);
        assert_eq!(sink.count, 0);
    }
}

#[test]
fn reader_and_sink_errors_propagate() {
    let (reader, root, _) = deep(1);
    let missing = pollster::block_on(reconstruct_closure(
        &reader,
        &mut Discard::default(),
        NativeObjectId::Blob(ContentHash::from_bytes([99; 32])),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ));
    assert!(matches!(
        missing,
        Err(ReconstructionError::Reader("missing blob"))
    ));
    struct Reject;
    impl ObjectSink for Reject {
        type Error = &'static str;
        async fn write_object(
            &mut self,
            _: NativeObjectId,
            _: GitObjectKind,
            _: ObjectId,
            _: &[u8],
        ) -> Result<(), Self::Error> {
            Err("sink rejected")
        }
    }
    let rejected = pollster::block_on(reconstruct_closure(
        &reader,
        &mut Reject,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ));
    assert!(matches!(
        rejected,
        Err(ReconstructionError::Sink("sink rejected"))
    ));
}

#[test]
fn blob_passthrough_and_mixed_formats() {
    let blob = Blob::from_slice(b"raw\0\xff");
    assert_eq!(serialize_blob(&blob), b"raw\0\xff");
    assert_eq!(serialize_blob(&blob).as_ptr(), blob.content().as_ptr());
    let state = synthetic_initial_base().expect("state");
    assert!(
        serialize_commit(
            &state,
            &ObjectId::empty_tree(ObjectFormat::Sha1),
            &[ObjectId::null(ObjectFormat::Sha256)]
        )
        .is_err()
    );
    let tree = Tree::from_entries(vec![
        TreeEntry::file("file", blob.hash(), false).expect("entry"),
    ]);
    assert!(
        serialize_tree(&tree, ObjectFormat::Sha1, |_| Ok(ObjectId::null(
            ObjectFormat::Sha256
        )))
        .is_err()
    );
}

#[test]
fn hosted_seed_is_not_a_git_parent_and_lossy_imports_fail_closed() {
    let seed = synthetic_initial_base().expect("seed");
    let empty = Tree::new();
    let state = State::new_snapshot(
        empty.hash(),
        vec![seed.id()],
        Attribution::human(Principal::new("A", "a@x")),
    );
    let root = state.id();
    let expected = object_id(
        ObjectFormat::Sha1,
        GitObjectKind::Commit,
        &serialize_commit(&state, &ObjectId::empty_tree(ObjectFormat::Sha1), &[]).expect("commit"),
    )
    .expect("commit oid");
    let mut reader = Reader {
        trees: BTreeMap::from([(empty.hash(), empty)]),
        state: Some(state),
        blob: Blob::from_slice(b""),
        reads: AtomicUsize::new(0),
    };
    let mut sink = Discard::default();
    let actual = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Commit(root),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ))
    .expect("seed parent omitted");
    assert_eq!(actual, expected);
    assert_eq!(sink.count, 2);
    let seed_root = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Commit(seed.id()),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ));
    assert!(matches!(seed_root, Err(ReconstructionError::Serialize(_))));
    reader.state.as_mut().expect("state").git_lossy = true;
    let lossy = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Commit(root),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ));
    assert!(matches!(lossy, Err(ReconstructionError::Serialize(_))));
    assert_eq!(sink.count, 2);
}

#[test]
fn spoollinks_and_oversized_outputs_fail_closed() {
    let spoollink = TreeEntry::spoollink(
        "child",
        SpoolId::parse("owner/child").expect("spool"),
        StateId::from_bytes([1; 32]),
    )
    .expect("spoollink");
    let tree = Tree::from_entries(vec![spoollink]);
    let root = tree.hash();
    let reader = Reader {
        trees: BTreeMap::from([(root, tree)]),
        state: None,
        blob: Blob::from_slice(b"leaf"),
        reads: AtomicUsize::new(0),
    };
    let mut sink = Discard::default();
    let result = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits::default(),
    ));
    assert!(matches!(result, Err(ReconstructionError::Serialize(_))));
    assert_eq!(sink.count, 0);

    // Raw modes store their leading-zero count compactly. A modest native
    // tree can therefore expand beyond the Git output budget.
    let blob = Blob::from_slice(b"leaf");
    let mut digits = vec![b'0'; 250];
    digits.extend_from_slice(b"100644");
    let mode = RawGitMode::parse(&digits).expect("padded mode");
    let entries = (0..100)
        .map(|n| {
            TreeEntry::file(format!("file-{n:03}"), blob.hash(), false)
                .expect("file")
                .with_raw_git_mode(mode)
                .expect("raw mode")
        })
        .collect();
    let tree = Tree::from_git_entries(entries).expect("compact native layout");
    let root = tree.hash();
    let reader = Reader {
        trees: BTreeMap::from([(root, tree)]),
        state: None,
        blob,
        reads: AtomicUsize::new(0),
    };
    let result = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_object_bytes: 16 * 1024,
            ..Default::default()
        },
    ));
    assert!(matches!(
        result,
        Err(ReconstructionError::LimitExceeded("object bytes"))
    ));
    assert_eq!(sink.count, 0, "oversized tree rejected before descent");
}

#[test]
fn total_bytes_reserved_before_descent_and_reader_allocation() {
    let (reader, root, _) = deep(1);
    let mut sink = Discard::default();
    // Tree content is 33 bytes and the blob content is four bytes.
    let result = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_total_bytes: 32,
            ..Default::default()
        },
    ));
    assert!(matches!(
        result,
        Err(ReconstructionError::LimitExceeded("total bytes"))
    ));
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    assert_eq!(sink.count, 0);
    let result = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_total_bytes: 36,
            ..Default::default()
        },
    ));
    assert!(matches!(
        result,
        Err(ReconstructionError::Reader("reader byte limit"))
    ));
    assert_eq!(sink.count, 0);
    pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_total_bytes: 37,
            ..Default::default()
        },
    ))
    .expect("exact total byte budget");
    assert_eq!(sink.count, 2);
}

#[test]
fn sink_cached_children_skip_reads_and_are_memo_bounded() {
    struct Cached {
        id: ObjectId,
        count: usize,
    }
    impl ObjectSink for Cached {
        type Error = &'static str;
        fn resolved(&self, native: NativeObjectId) -> Option<ObjectId> {
            matches!(native, NativeObjectId::Blob(_)).then_some(self.id)
        }
        async fn write_object(
            &mut self,
            _: NativeObjectId,
            _: GitObjectKind,
            _: ObjectId,
            _: &[u8],
        ) -> Result<(), Self::Error> {
            self.count += 1;
            Ok(())
        }
    }
    let (reader, root, expected) = deep(1);
    let mut sink = Cached {
        id: object_id(
            ObjectFormat::Sha1,
            GitObjectKind::Blob,
            reader.blob.content(),
        )
        .expect("blob"),
        count: 0,
    };
    let result = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_objects: 1,
            ..Default::default()
        },
    ));
    assert!(matches!(
        result,
        Err(ReconstructionError::LimitExceeded("objects"))
    ));
    assert_eq!(sink.count, 0);
    reader.reads.store(0, Ordering::Relaxed);
    let actual = pollster::block_on(reconstruct_closure(
        &reader,
        &mut sink,
        NativeObjectId::Tree(root),
        ObjectFormat::Sha1,
        ReconstructionLimits {
            max_objects: 2,
            ..Default::default()
        },
    ))
    .expect("cached child");
    assert_eq!(actual, expected);
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    assert_eq!(sink.count, 1);
    sink.id = ObjectId::null(ObjectFormat::Sha256);
    assert!(matches!(
        pollster::block_on(reconstruct_closure(
            &reader,
            &mut sink,
            NativeObjectId::Tree(root),
            ObjectFormat::Sha1,
            ReconstructionLimits::default()
        )),
        Err(ReconstructionError::Serialize(_))
    ));
}
