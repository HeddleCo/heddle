// SPDX-License-Identifier: Apache-2.0

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use super::{GateLoad, create_store, direct_pack_names, scheduler, started_handle};
use crate::{
    object::{Blob, ContentHash},
    store::{
        FsRepackOperation, FsStore, ObjectStore, RepackError,
        pack::{ObjectType, PackObjectId},
    },
};

fn seed_packed_blobs(store: &FsStore, prefix: &str) -> Vec<(ContentHash, Vec<u8>)> {
    let blobs = (0..128)
        .map(|index| {
            let data = format!("{prefix}-{index:04}-{}", "payload".repeat(32)).into_bytes();
            let blob = Blob::from(data.clone());
            (blob.hash(), data)
        })
        .collect::<Vec<_>>();
    store.put_blobs_packed(blobs.clone()).unwrap();
    blobs
}

#[test]
fn concurrent_reads_and_pack_writes_see_no_loss_corruption_or_torn_reads() {
    let (_temp, store) = create_store();
    let initial = Arc::new(seed_packed_blobs(&store, "initial"));
    let (load, paused) = GateLoad::new(8);
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let handle = started_handle(scheduler(Some(load.clone())).repack_now(operation).unwrap());
    paused.recv_timeout(Duration::from_secs(5)).unwrap();

    let reader_store = store.clone();
    let reader_objects = Arc::clone(&initial);
    let finished = Arc::new(AtomicBool::new(false));
    let reader_finished = Arc::clone(&finished);
    let reader = thread::spawn(move || {
        let mut passes = 0usize;
        while !reader_finished.load(Ordering::Acquire) || passes < 20 {
            for (hash, expected) in reader_objects.iter() {
                let (object_type, actual) = reader_store
                    .get_pack_object(&PackObjectId::Hash(*hash))
                    .unwrap()
                    .expect("packed object must never disappear during cutover");
                assert_eq!(object_type, ObjectType::Blob);
                assert_eq!(&actual, expected, "reader observed torn/corrupt bytes");
                assert_eq!(ContentHash::compute_typed("blob", &actual), *hash);
            }
            passes += 1;
            thread::yield_now();
        }
    });

    let writer_store = store.clone();
    let writer = thread::spawn(move || seed_packed_blobs(&writer_store, "concurrent"));
    let concurrent = writer.join().unwrap();
    load.release();
    let report = handle.wait().unwrap();
    finished.store(true, Ordering::Release);
    reader.join().unwrap();
    assert_eq!(report.objects_repacked, initial.len() as u64);

    let reopened = FsStore::new(store.root());
    for (hash, expected) in initial.iter().chain(concurrent.iter()) {
        let actual = reopened
            .get_blob(hash)
            .unwrap()
            .expect("object survived repack");
        assert_eq!(actual.content(), expected);
        assert_eq!(actual.hash(), *hash);
    }
    assert_eq!(
        direct_pack_names(store.root())
            .iter()
            .filter(|name| name.ends_with(".pack"))
            .count(),
        2,
        "the concurrent pack must be preserved alongside the replacement"
    );
}

#[test]
fn cancellation_mid_repack_leaves_store_unchanged_and_rerunnable() {
    let (_temp, store) = create_store();
    let objects = seed_packed_blobs(&store, "cancel");
    let before = direct_pack_names(store.root());
    let (load, paused) = GateLoad::new(8);
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let handle = started_handle(scheduler(Some(load.clone())).repack_now(operation).unwrap());
    paused.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.cancel();
    assert_eq!(handle.wait().unwrap_err(), RepackError::Cancelled);
    assert_eq!(direct_pack_names(store.root()), before);

    for (hash, expected) in &objects {
        assert_eq!(store.get_blob(hash).unwrap().unwrap().content(), expected);
    }
    load.release();
    let rerun = Arc::new(FsRepackOperation::new(store.clone()));
    let report = started_handle(scheduler(None).repack_now(rerun).unwrap())
        .wait()
        .unwrap();
    assert_eq!(report.objects_repacked, objects.len() as u64);
    for (hash, expected) in &objects {
        let actual = store.get_blob(hash).unwrap().unwrap();
        assert_eq!(actual.content(), expected);
        assert_eq!(actual.hash(), *hash);
    }
}
