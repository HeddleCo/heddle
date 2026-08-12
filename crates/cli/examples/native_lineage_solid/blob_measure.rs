// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    measure::compress_frame,
    model::{ObjectRef, ObjectSet},
};

#[derive(Default, Serialize)]
pub struct RoundTripStats {
    pub checked: usize,
    pub typed_hash_mismatches: usize,
    pub sample_ids: Vec<String>,
}

#[derive(Default, Serialize)]
pub struct BlobMeasurement {
    pub objects: usize,
    pub source_bytes: u64,
    pub compressed_bytes: u64,
    pub frames: usize,
    pub frame_integrity_checks: usize,
    pub roundtrip: RoundTripStats,
}

pub fn measure_blobs(
    store: &objects::store::FsStore,
    order: &[ObjectRef],
    output_dir: &Path,
    frame_limit: usize,
) -> Result<BlobMeasurement> {
    let blobs = order
        .iter()
        .filter(|object| matches!(object, ObjectRef::Blob(_)))
        .collect::<Vec<_>>();
    let frames_dir = output_dir.join("blob-lineage.frames");
    fs::create_dir(&frames_dir)?;
    let mut measurement = BlobMeasurement::default();
    let mut frame = Vec::with_capacity(frame_limit);
    for (index, object) in blobs.iter().enumerate() {
        let source = object.load(store)?;
        if !frame.is_empty() && frame.len() + source.len() > frame_limit {
            flush_frame(&frames_dir, &mut frame, &mut measurement)?;
        }
        frame.extend_from_slice(&source);
        measurement.objects += 1;
        measurement.source_bytes += source.len() as u64;
        measurement.roundtrip.checked += 1;
        if measurement.roundtrip.sample_ids.len() < 3 {
            measurement.roundtrip.sample_ids.push(object.id());
        }
        report_progress("lineage blobs", index + 1, blobs.len());
    }
    flush_frame(&frames_dir, &mut frame, &mut measurement)?;
    measurement.frame_integrity_checks = verify_frames(&frames_dir)?;
    Ok(measurement)
}

pub fn fingerprint_objects(
    store: &objects::store::FsStore,
    objects: &ObjectSet,
) -> Result<(blake3::Hash, usize)> {
    let mut hasher = blake3::Hasher::new();
    let mut checked = 0usize;
    for object in objects.iter() {
        let data = object.load(store)?;
        hasher.update(object.kind().as_bytes());
        hasher.update(object.id().as_bytes());
        hasher.update(&(data.len() as u64).to_le_bytes());
        hasher.update(&data);
        checked += 1;
        report_progress("real-writer roundtrip", checked, objects.counts().total);
    }
    Ok((hasher.finalize(), checked))
}

fn flush_frame(
    directory: &Path,
    frame: &mut Vec<u8>,
    measurement: &mut BlobMeasurement,
) -> Result<()> {
    if frame.is_empty() {
        return Ok(());
    }
    measurement.compressed_bytes += compress_frame(directory, measurement.frames, frame)?;
    measurement.frames += 1;
    frame.clear();
    Ok(())
}

fn verify_frames(directory: &Path) -> Result<usize> {
    let mut paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for path in &paths {
        let status = Command::new("/usr/bin/zstd")
            .env_clear()
            .args(["-t", "-q"])
            .arg(path)
            .status()
            .with_context(|| format!("test zstd frame {}", path.display()))?;
        if !status.success() {
            bail!("zstd test failed for {} with {status}", path.display());
        }
    }
    Ok(paths.len())
}

fn report_progress(kind: &str, done: usize, total: usize) {
    if done.is_multiple_of(10_000) || done == total {
        eprintln!("{kind}: {done}/{total} objects");
    }
}
