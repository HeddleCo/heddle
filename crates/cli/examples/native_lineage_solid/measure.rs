// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, bail};
use objects::store::FsStore;
use serde::Serialize;

use crate::{lineage::RenameRecord, model::ObjectRef};

#[derive(Serialize)]
pub struct TypeBytes {
    pub states: u64,
    pub trees: u64,
    pub blobs: u64,
    pub total: u64,
}

#[derive(Serialize)]
pub struct OrderMeasurement {
    pub objects: usize,
    pub raw_bytes: TypeBytes,
    pub compressed_bytes: u64,
    pub frames: usize,
    pub index_estimate_bytes: u64,
    pub bounded_plus_index_bytes: u64,
}

pub fn measure_order(
    store: &FsStore,
    order: &[ObjectRef],
    output_dir: &Path,
    name: &str,
    frame_limit: usize,
) -> Result<OrderMeasurement> {
    let raw_path = output_dir.join(format!("{name}.raw"));
    let manifest_path = output_dir.join(format!("{name}.manifest.tsv"));
    let frames_dir = output_dir.join(format!("{name}.frames"));
    fs::create_dir(&frames_dir)?;
    let mut raw = BufWriter::new(File::create(raw_path)?);
    let mut manifest = BufWriter::new(File::create(manifest_path)?);
    writeln!(manifest, "kind\tid\toffset\tlength")?;
    let mut type_bytes = TypeBytes {
        states: 0,
        trees: 0,
        blobs: 0,
        total: 0,
    };
    let mut frame = Vec::with_capacity(frame_limit);
    let mut frame_count = 0;
    let mut compressed_bytes = 0;
    let mut offset = 0u64;
    for (index, object) in order.iter().enumerate() {
        let bytes = object.load(store)?;
        raw.write_all(&bytes)?;
        writeln!(
            manifest,
            "{}\t{}\t{}\t{}",
            object.kind(),
            object.id(),
            offset,
            bytes.len()
        )?;
        add_type_bytes(&mut type_bytes, object, bytes.len() as u64);
        offset += bytes.len() as u64;
        frame.extend_from_slice(&bytes);
        if frame.len() >= frame_limit {
            compressed_bytes += compress_frame(&frames_dir, frame_count, &frame)?;
            frame.clear();
            frame_count += 1;
        }
        if (index + 1) % 10_000 == 0 || index + 1 == order.len() {
            eprintln!("{name} order: {}/{} objects", index + 1, order.len());
        }
    }
    if !frame.is_empty() {
        compressed_bytes += compress_frame(&frames_dir, frame_count, &frame)?;
        frame_count += 1;
    }
    raw.flush()?;
    manifest.flush()?;
    let index_estimate_bytes = (order.len() as u64) * 40;
    Ok(OrderMeasurement {
        objects: order.len(),
        raw_bytes: type_bytes,
        compressed_bytes,
        frames: frame_count,
        index_estimate_bytes,
        bounded_plus_index_bytes: compressed_bytes + index_estimate_bytes,
    })
}

fn add_type_bytes(total: &mut TypeBytes, object: &ObjectRef, bytes: u64) {
    match object {
        ObjectRef::State(_) => total.states += bytes,
        ObjectRef::Tree(_) => total.trees += bytes,
        ObjectRef::Blob(_) => total.blobs += bytes,
    }
    total.total += bytes;
}

fn compress_frame(frames_dir: &Path, index: usize, bytes: &[u8]) -> Result<u64> {
    let raw_path = frames_dir.join("current.raw");
    let compressed_path = frames_dir.join(format!("{index:05}.zst"));
    fs::write(&raw_path, bytes)?;
    let status = Command::new("/usr/bin/zstd")
        .env_clear()
        .args(["-19", "--long=27", "-q", "-f"])
        .arg(&raw_path)
        .arg("-o")
        .arg(&compressed_path)
        .status()
        .context("run /usr/bin/zstd -19 --long=27")?;
    if !status.success() {
        bail!("zstd failed for frame {} with {status}", raw_path.display());
    }
    fs::remove_file(&raw_path)?;
    Ok(fs::metadata(compressed_path)?.len())
}

pub fn write_renames(path: &Path, renames: &[RenameRecord]) -> Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(output, "child_state\tparent_state\tkind\tfrom\tto")?;
    for rename in renames {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}",
            rename.child.to_string_full(),
            rename.parent.to_string_full(),
            if rename.exact { "exact" } else { "similarity" },
            rename.from,
            rename.to
        )?;
    }
    Ok(())
}
