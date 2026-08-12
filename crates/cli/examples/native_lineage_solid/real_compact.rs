// SPDX-License-Identifier: Apache-2.0

use std::{fs, num::NonZeroUsize, sync::Arc};

use anyhow::{Context, Result, bail};
use objects::store::{
    FsRepackOperation, FsStore, RepackPolicy, RepackResourceLimits, RepackSchedule,
    RepackScheduler,
    pack::{ObjectType, PackReader},
};
use serde::Serialize;

use crate::{
    blob_measure::fingerprint_objects,
    model::{ObjectRef, ObjectSet},
};

#[derive(Serialize)]
pub struct RealCompactMeasurement {
    pub source_blob_bytes: u64,
    pub source_tree_bytes: u64,
    pub source_state_bytes: u64,
    pub compact_blob_bytes: u64,
    pub compact_tree_bytes: u64,
    pub compact_state_bytes: u64,
    pub index_bytes: u64,
    pub replacement_pack_bytes: u64,
    pub roundtrip_checked: usize,
    pub source_fingerprint: String,
    pub reconstructed_fingerprint: String,
    pub byte_identical: bool,
}

pub fn measure_real_compact_repack(
    store: &FsStore,
    objects: &ObjectSet,
) -> Result<RealCompactMeasurement> {
    let (source_blob_bytes, source_tree_bytes, source_state_bytes) =
        source_object_bytes(store, objects)?;
    let (source_fingerprint, _) = fingerprint_objects(store, objects)?;
    run_repack(store)?;
    store.clear_recent_object_caches();
    let (reconstructed_fingerprint, roundtrip_checked) = fingerprint_objects(store, objects)?;
    if source_fingerprint != reconstructed_fingerprint {
        bail!("real compact writer changed native object bytes");
    }
    let (
        compact_blob_bytes,
        compact_tree_bytes,
        compact_state_bytes,
        index_bytes,
        replacement_pack_bytes,
    ) = physical_object_bytes(store)?;
    Ok(RealCompactMeasurement {
        source_blob_bytes,
        source_tree_bytes,
        source_state_bytes,
        compact_blob_bytes,
        compact_tree_bytes,
        compact_state_bytes,
        index_bytes,
        replacement_pack_bytes,
        roundtrip_checked,
        source_fingerprint: source_fingerprint.to_hex().to_string(),
        reconstructed_fingerprint: reconstructed_fingerprint.to_hex().to_string(),
        byte_identical: true,
    })
}

fn source_object_bytes(store: &FsStore, objects: &ObjectSet) -> Result<(u64, u64, u64)> {
    let mut blobs = 0u64;
    let mut trees = 0u64;
    let mut states = 0u64;
    for object in objects.iter() {
        match object {
            ObjectRef::Tree(_) => trees += object.load(store)?.len() as u64,
            ObjectRef::State(_) => states += object.load(store)?.len() as u64,
            ObjectRef::Blob(_) => blobs += object.load(store)?.len() as u64,
        }
    }
    Ok((blobs, trees, states))
}

fn run_repack(store: &FsStore) -> Result<()> {
    let scheduler = RepackScheduler::new(
        RepackPolicy::default(),
        RepackResourceLimits::new(NonZeroUsize::MIN).with_io_rate(None),
    );
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let RepackSchedule::Started(handle) = scheduler.repack_now(operation)? else {
        bail!("real compact repack did not start");
    };
    handle.wait()?;
    Ok(())
}

fn physical_object_bytes(store: &FsStore) -> Result<(u64, u64, u64, u64, u64)> {
    let packs_dir = store.root().join("packs");
    let paths = fs::read_dir(&packs_dir)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|value| value == "pack"))
        .map(|pack| {
            let index = pack.with_extension("idx");
            (pack, index)
        })
        .collect::<Vec<_>>();
    let mut blob_bytes = 0u64;
    let mut tree_bytes = 0u64;
    let mut state_bytes = 0u64;
    let mut index_bytes = 0u64;
    let mut pack_bytes = 0u64;
    for (pack, index) in paths {
        let reader = PackReader::open(&pack, &index)
            .with_context(|| format!("open replacement pack {}", pack.display()))?;
        blob_bytes += reader.encoded_payload_bytes(ObjectType::Blob)?;
        tree_bytes += reader.encoded_payload_bytes(ObjectType::Tree)?;
        state_bytes += reader.encoded_payload_bytes(ObjectType::State)?;
        index_bytes += fs::metadata(&index)?.len();
        pack_bytes += fs::metadata(&pack)?.len();
    }
    Ok((blob_bytes, tree_bytes, state_bytes, index_bytes, pack_bytes))
}
