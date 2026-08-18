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
    pub storage_before: StorageFootprint,
    pub storage_after: StorageFootprint,
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

/// Physical bytes occupied by retention-equalized native objects.
#[derive(Debug, Serialize)]
pub struct StorageFootprint {
    pub loose_blob_bytes: u64,
    pub loose_tree_bytes: u64,
    pub loose_state_bytes: u64,
    pub pack_bytes: u64,
    pub index_bytes: u64,
    pub total_bytes: u64,
}

pub fn measure_real_compact_repack(
    store: &FsStore,
    objects: &ObjectSet,
) -> Result<RealCompactMeasurement> {
    let storage_before = measure_store_footprint(store.root())?;
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
    let storage_after = measure_store_footprint(store.root())?;
    Ok(RealCompactMeasurement {
        storage_before,
        storage_after,
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

fn measure_store_footprint(root: &std::path::Path) -> Result<StorageFootprint> {
    let objects = root.join("objects");
    let loose_blob_bytes = regular_file_bytes(&objects.join("blobs"))?;
    let loose_tree_bytes = regular_file_bytes(&objects.join("trees"))?;
    let loose_state_bytes = regular_file_bytes(&objects.join("states"))?;
    let packs = root.join("packs");
    let pack_bytes = extension_bytes(&packs, "pack")?;
    let index_bytes = extension_bytes(&packs, "idx")?;
    let total_bytes = loose_blob_bytes
        .saturating_add(loose_tree_bytes)
        .saturating_add(loose_state_bytes)
        .saturating_add(pack_bytes)
        .saturating_add(index_bytes);
    Ok(StorageFootprint {
        loose_blob_bytes,
        loose_tree_bytes,
        loose_state_bytes,
        pack_bytes,
        index_bytes,
        total_bytes,
    })
}

fn regular_file_bytes(root: &std::path::Path) -> Result<u64> {
    if !root.try_exists()? {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "object-store footprint refuses symlink {}",
                entry.path().display()
            );
        }
        if file_type.is_dir() {
            total = total.saturating_add(regular_file_bytes(&entry.path())?);
        } else if file_type.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn extension_bytes(directory: &std::path::Path, extension: &str) -> Result<u64> {
    if !directory.try_exists()? {
        return Ok(0);
    }
    fs::read_dir(directory)?.try_fold(0u64, |total, entry| {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == extension)
        {
            Ok(total.saturating_add(entry.metadata()?.len()))
        } else {
            Ok(total)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_counts_only_native_object_payloads_and_indexes() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("objects/blobs/ab")).unwrap();
        fs::create_dir_all(root.join("objects/trees/cd")).unwrap();
        fs::create_dir_all(root.join("objects/states")).unwrap();
        fs::create_dir_all(root.join("packs")).unwrap();
        fs::write(root.join("objects/blobs/ab/blob"), b"blob").unwrap();
        fs::write(root.join("objects/trees/cd/tree"), b"tree!!").unwrap();
        fs::write(root.join("objects/states/state"), b"state").unwrap();
        fs::write(root.join("packs/one.pack"), b"packpack").unwrap();
        fs::write(root.join("packs/one.idx"), b"index").unwrap();
        fs::write(root.join("config.toml"), vec![0; 100]).unwrap();

        let measured = measure_store_footprint(root).unwrap();
        assert_eq!(measured.loose_blob_bytes, 4);
        assert_eq!(measured.loose_tree_bytes, 6);
        assert_eq!(measured.loose_state_bytes, 5);
        assert_eq!(measured.pack_bytes, 8);
        assert_eq!(measured.index_bytes, 5);
        assert_eq!(measured.total_bytes, 28);
    }
}
