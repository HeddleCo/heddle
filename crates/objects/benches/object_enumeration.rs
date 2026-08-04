// SPDX-License-Identifier: Apache-2.0
//! Mixed loose/packed object-enumeration benchmark.
//!
//! Run: `cargo bench -p heddle-objects --bench object_enumeration --features bench`

use std::{fs, hint::black_box, path::Path};

use chrono::{TimeZone, Utc};
use criterion::{Criterion, criterion_group, criterion_main};
use heddle_format::compression::CompressionConfig;
use objects::{
    object::{Action, Attribution, ContentHash, Operation, Principal, StateId, Tree, TreeEntry},
    store::{
        FsStore, ObjectStore,
        pack::{ObjectType, PackBuilder},
    },
};
use tempfile::TempDir;

const LOOSE_PER_TYPE: usize = 2_048;
const PACKED_PER_TYPE: usize = 2_048;
const BLOB_SIZE: usize = 1_024;

struct EnumerationFixture {
    _dir: TempDir,
    store: FsStore,
}

fn write_loose_hash(root: &Path, kind: &str, hash: ContentHash) {
    let hex = hash.to_hex();
    let path = if kind == "actions" {
        root.join(kind).join(format!("{hex}.action"))
    } else {
        root.join("objects")
            .join(kind)
            .join(&hex[..2])
            .join(&hex[2..])
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, []).unwrap();
}

fn fixture_action(index: usize) -> Action {
    Action::new(
        None,
        StateId::from_bytes([7; 32]),
        Operation::Snapshot,
        format!("enumeration fixture action {index}"),
        Attribution::human(Principal::new("perf", "perf@heddle.local")),
    )
    .with_timestamp(Utc.timestamp_opt(index as i64, 0).unwrap())
}

fn build_fixture() -> EnumerationFixture {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    store.init().unwrap();

    for index in 0..LOOSE_PER_TYPE {
        let seed = index.to_le_bytes();
        write_loose_hash(
            dir.path(),
            "blobs",
            ContentHash::compute_typed("loose-blob", &seed),
        );
        write_loose_hash(
            dir.path(),
            "trees",
            ContentHash::compute_typed("loose-tree", &seed),
        );
        write_loose_hash(
            dir.path(),
            "actions",
            ContentHash::compute_typed("loose-action", &seed),
        );
    }

    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    for index in 0..PACKED_PER_TYPE {
        let mut blob = vec![0; BLOB_SIZE];
        blob[..size_of::<usize>()].copy_from_slice(&index.to_le_bytes());
        let blob_hash = ContentHash::compute_typed("blob", &blob);
        builder.add(blob_hash, ObjectType::Blob, blob);

        let tree = Tree::from_entries(vec![
            TreeEntry::file(format!("file-{index}.bin"), blob_hash, false).unwrap(),
        ]);
        builder.add(
            tree.hash(),
            ObjectType::Tree,
            rmp_serde::to_vec(&tree).unwrap(),
        );

        let action = fixture_action(index);
        builder.add(
            *action.compute_id().as_hash(),
            ObjectType::Action,
            rmp_serde::to_vec(&action).unwrap(),
        );
    }
    let (pack, index, _) = builder.build().unwrap();
    store.install_pack(&pack, &index).unwrap();

    EnumerationFixture { _dir: dir, store }
}

fn bench_object_enumeration(c: &mut Criterion) {
    let fixture = build_fixture();
    let expected_per_type = LOOSE_PER_TYPE + PACKED_PER_TYPE;
    assert_eq!(fixture.store.list_blobs().unwrap().len(), expected_per_type);
    assert_eq!(fixture.store.list_trees().unwrap().len(), expected_per_type);
    assert_eq!(
        fixture.store.list_actions().unwrap().len(),
        expected_per_type
    );

    c.bench_function("object_enumeration/mixed_6144_loose_6144_packed", |b| {
        b.iter(|| {
            black_box(fixture.store.list_blobs().unwrap());
            black_box(fixture.store.list_trees().unwrap());
            black_box(fixture.store.list_actions().unwrap());
        });
    });
}

criterion_group!(benches, bench_object_enumeration);
criterion_main!(benches);
