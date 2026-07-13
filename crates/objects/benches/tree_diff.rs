// SPDX-License-Identifier: Apache-2.0
//! Merge-join tree diff hot-path benchmarks.
//!
//! Run: `cargo bench -p heddle-objects --bench tree_diff`

use std::{collections::HashMap, hint::black_box, sync::RwLock};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use objects::{
    object::{Action, ActionId, Blob, ContentHash, State, StateId, Tree, TreeEntry, diff_trees},
    store::{ObjectStore, Result},
    sync::RwLockExt,
};

const SIZES: &[usize] = &[1_000, 10_000, 100_000];

#[derive(Clone, Copy)]
enum DeltaShape {
    Small,
    Large,
    Disjoint,
}

impl DeltaShape {
    fn label(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Large => "large",
            Self::Disjoint => "disjoint",
        }
    }
}

#[derive(Default)]
struct BenchStore {
    blobs: RwLock<HashMap<ContentHash, Blob>>,
    trees: RwLock<HashMap<ContentHash, Tree>>,
}

impl ObjectStore for BenchStore {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        Ok(self.blobs.read_or_poisoned().get(hash).cloned())
    }

    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        let hash = blob.hash();
        self.blobs.write_or_poisoned().insert(hash, blob.clone());
        Ok(hash)
    }

    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.blobs.read_or_poisoned().contains_key(hash))
    }

    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        Ok(self.trees.read_or_poisoned().get(hash).cloned())
    }

    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        self.trees.write_or_poisoned().insert(hash, tree.clone());
        Ok(hash)
    }

    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.trees.read_or_poisoned().contains_key(hash))
    }

    fn get_state(&self, _id: &StateId) -> Result<Option<State>> {
        Ok(None)
    }

    fn put_state(&self, _state: &State) -> Result<()> {
        Ok(())
    }

    fn has_state(&self, _id: &StateId) -> Result<bool> {
        Ok(false)
    }

    fn list_states(&self) -> Result<Vec<StateId>> {
        Ok(Vec::new())
    }

    fn get_action(&self, _id: &ActionId) -> Result<Option<Action>> {
        Ok(None)
    }

    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        Ok(action.id())
    }

    fn list_actions(&self) -> Result<Vec<ActionId>> {
        Ok(Vec::new())
    }

    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        Ok(self.blobs.read_or_poisoned().keys().copied().collect())
    }

    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        Ok(self.trees.read_or_poisoned().keys().copied().collect())
    }
}

fn blob_hash(store: &BenchStore, content: String) -> ContentHash {
    store.put_blob(&Blob::from(content)).unwrap()
}

fn tree_for(store: &BenchStore, size: usize, shape: DeltaShape, side: usize) -> ContentHash {
    let mut entries = Vec::with_capacity(size);
    for i in 0..size {
        let include = match shape {
            DeltaShape::Disjoint if side == 0 => i % 2 == 0,
            DeltaShape::Disjoint if side == 1 => i % 2 == 1,
            _ => true,
        };
        if !include {
            continue;
        }

        let changed = match shape {
            DeltaShape::Small => side == 1 && i % (size / 100).max(1) == 0,
            DeltaShape::Large => side == 1 && i % 2 == 0,
            DeltaShape::Disjoint => false,
        };
        let name = format!("file-{i:06}.txt");
        let content = format!(
            "entry={i}; side={}; changed={changed}\n",
            side * changed as usize
        );
        entries.push(TreeEntry::file(name, blob_hash(store, content), false).unwrap());
    }
    let tree = Tree::from_entries(entries);
    store.put_tree(&tree).unwrap()
}

fn bench_tree_diff(c: &mut Criterion) {
    let shapes = [DeltaShape::Small, DeltaShape::Large, DeltaShape::Disjoint];
    for shape in shapes {
        let mut group = c.benchmark_group(format!("tree_diff_{}", shape.label()));
        for &size in SIZES {
            let store = BenchStore::default();
            let from = tree_for(&store, size, shape, 0);
            let to = tree_for(&store, size, shape, 1);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
                b.iter(|| {
                    let changes =
                        diff_trees(black_box(&store), black_box(&from), black_box(&to)).unwrap();
                    black_box(changes.len());
                });
            });
        }
        group.finish();
    }
}

criterion_group!(benches, bench_tree_diff);
criterion_main!(benches);
