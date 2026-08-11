// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};
use objects::{object::Tree, store::FsStore};
use serde::Serialize;

use crate::{
    compact_tree::{TreeBreakdown, decode_tree, encode_tree},
    measure::compress_frame,
    model::ObjectRef,
};

#[derive(Default, Serialize)]
pub struct RoundTripStats {
    pub checked: usize,
    pub value_mismatches: usize,
    pub typed_hash_mismatches: usize,
    pub native_payload_mismatches: usize,
    pub sample_ids: Vec<String>,
}

impl RoundTripStats {
    pub fn checked(&mut self, id: String) {
        self.checked += 1;
        if self.sample_ids.len() < 3 {
            self.sample_ids.push(id);
        }
    }
}

#[derive(Default, Serialize)]
pub struct CommonMeasurement {
    pub objects: usize,
    pub source_msgpack_bytes: u64,
    pub source_positional_msgpack_bytes: u64,
    pub compact_raw_bytes: u64,
    pub compressed_bytes: u64,
    pub frames: usize,
    pub frame_integrity_checks: usize,
    pub roundtrip: RoundTripStats,
}

#[derive(Serialize)]
pub struct TreeMeasurement {
    #[serde(flatten)]
    pub common: CommonMeasurement,
    pub breakdown: TreeBreakdown,
}

#[derive(Serialize)]
pub struct BlobMeasurement {
    #[serde(flatten)]
    pub common: CommonMeasurement,
}

pub fn measure_trees(
    store: &FsStore,
    order: &[objects::object::ContentHash],
    output_dir: &Path,
    frame_limit: usize,
) -> Result<TreeMeasurement> {
    let frames_dir = output_dir.join("compact-tree.frames");
    fs::create_dir(&frames_dir)?;
    let mut common = CommonMeasurement::default();
    let mut breakdown = TreeBreakdown::default();
    let mut frame = Vec::with_capacity(frame_limit);
    for (index, hash) in order.iter().enumerate() {
        let object = ObjectRef::Tree(*hash);
        let source = object.load(store)?;
        let tree: Tree = rmp_serde::from_slice(&source)?;
        let positional = rmp_serde::to_vec(&tree)?;
        let (compact, object_breakdown) = encode_tree(&tree)?;
        verify_tree(&object, &tree, &source, &compact, &mut common.roundtrip)?;
        if !frame.is_empty() && frame.len() + compact.len() > frame_limit {
            common.compressed_bytes += compress_frame(&frames_dir, common.frames, &frame)?;
            common.frames += 1;
            frame.clear();
        }
        frame.extend_from_slice(&compact);
        common.objects += 1;
        common.source_msgpack_bytes += source.len() as u64;
        common.source_positional_msgpack_bytes += positional.len() as u64;
        common.compact_raw_bytes += compact.len() as u64;
        breakdown.add(&object_breakdown);
        report_progress("compact trees", index + 1, order.len());
    }
    flush_last(&frames_dir, &mut frame, &mut common)?;
    common.frame_integrity_checks = verify_frames(&frames_dir)?;
    Ok(TreeMeasurement { common, breakdown })
}

pub fn measure_blobs(
    store: &FsStore,
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
    let mut common = CommonMeasurement::default();
    let mut frame = Vec::with_capacity(frame_limit);
    for (index, object) in blobs.iter().enumerate() {
        let source = object.load(store)?;
        if !frame.is_empty() && frame.len() + source.len() > frame_limit {
            common.compressed_bytes += compress_frame(&frames_dir, common.frames, &frame)?;
            common.frames += 1;
            frame.clear();
        }
        frame.extend_from_slice(&source);
        common.objects += 1;
        common.source_msgpack_bytes += source.len() as u64;
        common.compact_raw_bytes += source.len() as u64;
        common.roundtrip.checked(object.id());
        report_progress("lineage blobs", index + 1, blobs.len());
    }
    flush_last(&frames_dir, &mut frame, &mut common)?;
    common.frame_integrity_checks = verify_frames(&frames_dir)?;
    Ok(BlobMeasurement { common })
}

fn verify_tree(
    object: &ObjectRef,
    source_tree: &Tree,
    source_bytes: &[u8],
    compact: &[u8],
    stats: &mut RoundTripStats,
) -> Result<()> {
    let decoded = decode_tree(compact)?;
    if decoded != *source_tree {
        stats.value_mismatches += 1;
        bail!("compact tree value mismatch for {}", object.id());
    }
    let ObjectRef::Tree(expected) = object else {
        unreachable!();
    };
    if decoded.hash() != *expected {
        stats.typed_hash_mismatches += 1;
        bail!("compact tree typed-hash mismatch for {}", object.id());
    }
    if rmp_serde::to_vec_named(&decoded)? != source_bytes {
        stats.native_payload_mismatches += 1;
        bail!("compact tree native-payload mismatch for {}", object.id());
    }
    stats.checked(object.id());
    Ok(())
}

pub fn flush_last(
    frames_dir: &Path,
    frame: &mut Vec<u8>,
    common: &mut CommonMeasurement,
) -> Result<()> {
    if !frame.is_empty() {
        common.compressed_bytes += compress_frame(frames_dir, common.frames, frame)?;
        common.frames += 1;
        frame.clear();
    }
    Ok(())
}

pub fn verify_frames(frames_dir: &Path) -> Result<usize> {
    let mut paths = fs::read_dir(frames_dir)?
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

pub fn report_progress(kind: &str, done: usize, total: usize) {
    if done.is_multiple_of(10_000) || done == total {
        eprintln!("{kind}: {done}/{total} objects");
    }
}
