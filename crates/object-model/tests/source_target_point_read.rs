//! Resolving one shared target must not enumerate unrelated capture bindings.
use std::{cell::Cell, collections::BTreeMap};

use heddle_object_model::{
    error::Result,
    object::{
        Blob, CollaborationRevision, CollaborationScope, ContentHash, ObjectSource, State, StateId,
        Tree,
        source_target::{
            SourceFileCore, SourceSelector, SourceTargetCore,
            capture::{
                self, FileResolution, ResolutionStatus, SourceTargetSnapshot, TargetResolution,
            },
        },
        source_target_map::{MapBudget, SourceTargetMap, SourceTargetMapStore},
    },
};
use uuid::Uuid;

#[derive(Default)]
struct Memory {
    blobs: BTreeMap<ContentHash, Vec<u8>>,
    reads: Cell<usize>,
}
impl Memory {
    fn put(&mut self, bytes: Vec<u8>) -> ContentHash {
        let hash = ContentHash::compute_typed("blob", &bytes);
        self.blobs.insert(hash, bytes);
        hash
    }
}
impl ObjectSource for Memory {
    fn get_tree(&self, _: &ContentHash) -> Result<Option<Tree>> {
        Ok(None)
    }
    fn get_state(&self, _: &StateId) -> Result<Option<State>> {
        Ok(None)
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.blobs.get(hash).cloned().map(Blob::new))
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> Result<Option<u64>> {
        Ok(self.blobs.get(hash).map(|bytes| bytes.len() as u64))
    }
}
impl SourceTargetMapStore for Memory {
    type Error = String;
    fn read(
        &mut self,
        hash: ContentHash,
        max: usize,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        let bytes = self.blobs.get(&hash);
        if bytes.is_some_and(|bytes| bytes.len() > max) {
            return Err("test read exceeds bound".into());
        }
        Ok(bytes.cloned())
    }
    fn write(&mut self, hash: ContentHash, bytes: Vec<u8>) -> std::result::Result<(), String> {
        self.blobs.insert(hash, bytes);
        Ok(())
    }
}

struct Fixture {
    memory: Memory,
    snapshot: SourceTargetSnapshot,
    descriptor: ContentHash,
    targets: Vec<(ContentHash, ContentHash)>,
}
fn fixture(count: usize) -> Fixture {
    let scope = CollaborationScope {
        spool: Uuid::from_u128(1),
        thread: Some(ContentHash::from_bytes([2; 32])),
    };
    let state = StateId::from_bytes([3; 32]);
    let mut memory = Memory::default();
    let mut snapshot = SourceTargetSnapshot {
        version: 1,
        scope: scope.clone(),
        state,
        collaboration_frontier: ContentHash::from_bytes([4; 32]),
        files: None,
        targets: None,
    };
    let mut targets = Vec::new();
    for index in 0..count {
        let file = SourceFileCore {
            scope: scope.clone(),
            revision: CollaborationRevision::State { state_id: state },
            path: format!("original/{index}.rs"),
        };
        let file_id = file.id().expect("valid file core");
        let core = SourceTargetCore {
            file: file_id,
            revision: file.revision.clone(),
            selector: SourceSelector::File,
        };
        let id = core.id().expect("valid target core");
        let target = TargetResolution {
            core,
            selector: SourceSelector::File,
            status: ResolutionStatus::Resolved,
        };
        let file = FileResolution {
            core: file,
            path: format!("renamed/{index}.rs"),
            blob: Some(ContentHash::from_bytes([5; 32])),
            status: ResolutionStatus::Resolved,
        };
        let file_hash = memory.put(capture::encode(&file).expect("file encoding"));
        let target_hash = memory.put(capture::encode(&target).expect("target encoding"));
        let mut budget = MapBudget::new(200, 500_000, 200, 500_000);
        snapshot.files = SourceTargetMap::update(
            &mut memory,
            snapshot.files,
            file_id,
            Some(file_hash),
            &mut budget,
        )
        .expect("file trie update");
        snapshot.targets = SourceTargetMap::update(
            &mut memory,
            snapshot.targets,
            id,
            Some(target_hash),
            &mut budget,
        )
        .expect("target trie update");
        targets.push((id, target_hash));
    }
    let descriptor = memory.put(capture::encode(&snapshot).expect("snapshot encoding"));
    Fixture {
        memory,
        snapshot,
        descriptor,
        targets,
    }
}

#[test]
fn one_target_reads_only_two_trie_routes_in_a_large_capture() {
    let f = fixture(2048);
    let resolved = capture::resolve_target(
        &f.memory,
        f.descriptor,
        &f.snapshot.scope,
        f.snapshot.state,
        f.targets[77].0,
    )
    .expect("bounded target lookup")
    .expect("known target");
    assert_eq!(resolved.file.path, "renamed/77.rs");
    assert_eq!(
        resolved.file.core.path, "original/77.rs",
        "authored evidence stays immutable"
    );
    assert!(
        f.memory.reads.get() < 25,
        "point read must not enumerate capture: {} blobs",
        f.memory.reads.get()
    );
}

#[test]
fn an_unrelated_missing_resolution_does_not_block_a_retained_target() {
    let mut f = fixture(128);
    f.memory.blobs.remove(&f.targets[90].1);
    let resolved = capture::resolve_target(
        &f.memory,
        f.descriptor,
        &f.snapshot.scope,
        f.snapshot.state,
        f.targets[7].0,
    )
    .expect("unrelated partial transfer is outside the read route")
    .expect("retained target");
    assert_eq!(resolved.file.path, "renamed/7.rs");
    assert!(
        capture::closure(&f.memory, f.descriptor, &f.snapshot.scope, f.snapshot.state).is_err(),
        "full transfer verification still requires the complete closure"
    );
}

#[test]
fn target_lookup_checks_descriptor_scope_and_selected_core_identity() {
    let mut f = fixture(2);
    let wrong_scope = CollaborationScope {
        spool: Uuid::from_u128(9),
        thread: f.snapshot.scope.thread,
    };
    assert!(
        capture::resolve_target(
            &f.memory,
            f.descriptor,
            &wrong_scope,
            f.snapshot.state,
            f.targets[0].0
        )
        .is_err()
    );
    let mut budget = MapBudget::new(200, 500_000, 200, 500_000);
    f.snapshot.targets = SourceTargetMap::update(
        &mut f.memory,
        f.snapshot.targets,
        f.targets[0].0,
        Some(f.targets[1].1),
        &mut budget,
    )
    .expect("valid map with wrongly addressed record");
    f.descriptor = f
        .memory
        .put(capture::encode(&f.snapshot).expect("snapshot encoding"));
    let error = capture::resolve_target(
        &f.memory,
        f.descriptor,
        &f.snapshot.scope,
        f.snapshot.state,
        f.targets[0].0,
    )
    .expect_err("record must match requested core");
    assert!(
        error
            .to_string()
            .contains("source target core identity mismatch")
    );
}
