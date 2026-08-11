// SPDX-License-Identifier: Apache-2.0
//! Throwaway measurement harness for heddle#1324.

#[path = "native_lineage_solid/blob_lineage.rs"]
mod blob_lineage;
#[path = "native_lineage_solid/lineage.rs"]
mod lineage;
#[path = "native_lineage_solid/measure.rs"]
mod measure;
#[path = "native_lineage_solid/model.rs"]
mod model;
#[path = "native_lineage_solid/tree_access.rs"]
mod tree_access;

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use objects::store::FsStore;
use serde::Serialize;

use crate::{
    lineage::build_lineage_order,
    measure::{OrderMeasurement, measure_order, write_renames},
    model::{ObjectCounts, ObjectRef, load_object_set},
};

const DEFAULT_FRAME_BYTES: usize = 12 * 1024 * 1024;

#[derive(Serialize)]
struct Results {
    repository: String,
    frame_bytes: usize,
    object_counts: ObjectCounts,
    lineage: OrderMeasurement,
    hash: OrderMeasurement,
    lineage_walk: lineage::LineageStats,
}

fn main() -> Result<()> {
    let (repo_root, output_dir, frame_bytes) = parse_args()?;
    fs::create_dir(&output_dir).with_context(|| {
        format!(
            "create fresh output directory {} (it must not already exist)",
            output_dir.display()
        )
    })?;

    let heddle_dir = repo_root.join(".heddle");
    if !heddle_dir.is_dir() {
        bail!(
            "{} is not an adopted Heddle repository",
            repo_root.display()
        );
    }
    let store = FsStore::new(&heddle_dir);
    store.reload_packs()?;
    let objects = load_object_set(&store)?;
    eprintln!(
        "native objects: {} states, {} trees, {} blobs",
        objects.states.len(),
        objects.trees.len(),
        objects.blobs.len()
    );

    let lineage = build_lineage_order(&store, &objects)?;
    write_renames(&output_dir.join("renames.tsv"), &lineage.renames)?;
    let lineage_measurement =
        measure_order(&store, &lineage.order, &output_dir, "lineage", frame_bytes)?;

    let mut hash_order = objects.all_refs();
    hash_order.sort_by_key(ObjectRef::sort_key);
    let hash_measurement = measure_order(&store, &hash_order, &output_dir, "hash", frame_bytes)?;

    let results = Results {
        repository: repo_root.display().to_string(),
        frame_bytes,
        object_counts: objects.counts(),
        lineage: lineage_measurement,
        hash: hash_measurement,
        lineage_walk: lineage.stats,
    };
    let json = serde_json::to_vec_pretty(&results)?;
    fs::write(output_dir.join("results.json"), &json)?;
    println!("{}", String::from_utf8(json)?);
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf, usize)> {
    let mut args = env::args_os().skip(1);
    let Some(repo_root) = args.next() else {
        bail!("usage: native_lineage_solid <adopted-repo> <fresh-output-dir> [frame-mib]");
    };
    let Some(output_dir) = args.next() else {
        bail!("usage: native_lineage_solid <adopted-repo> <fresh-output-dir> [frame-mib]");
    };
    let frame_bytes = match args.next() {
        Some(value) => value
            .to_string_lossy()
            .parse::<usize>()
            .context("frame-mib must be a positive integer")?
            .checked_mul(1024 * 1024)
            .context("frame-mib overflow")?,
        None => DEFAULT_FRAME_BYTES,
    };
    if frame_bytes == 0 || args.next().is_some() {
        bail!("usage: native_lineage_solid <adopted-repo> <fresh-output-dir> [frame-mib]");
    }
    Ok((repo_root.into(), output_dir.into(), frame_bytes))
}
