// SPDX-License-Identifier: Apache-2.0
//! Throwaway compact-metadata measurement harness for heddle#1325.

#[path = "native_lineage_solid/blob_lineage.rs"]
mod blob_lineage;
#[path = "native_lineage_solid/blob_paths.rs"]
mod blob_paths;
#[path = "native_lineage_solid/compact_io.rs"]
mod compact_io;
#[path = "native_lineage_solid/compact_measure.rs"]
mod compact_measure;
#[path = "native_lineage_solid/compact_state.rs"]
mod compact_state;
#[path = "native_lineage_solid/compact_state_decode.rs"]
mod compact_state_decode;
#[path = "native_lineage_solid/compact_state_dictionary.rs"]
mod compact_state_dictionary;
#[path = "native_lineage_solid/compact_state_measure.rs"]
mod compact_state_measure;
#[path = "native_lineage_solid/compact_tree.rs"]
mod compact_tree;
#[path = "native_lineage_solid/git_baseline.rs"]
mod git_baseline;
#[path = "native_lineage_solid/lineage.rs"]
mod lineage;
#[path = "native_lineage_solid/measure.rs"]
mod measure;
#[path = "native_lineage_solid/model.rs"]
mod model;
#[path = "native_lineage_solid/tree_access.rs"]
mod tree_access;
#[path = "native_lineage_solid/tree_lineage.rs"]
mod tree_lineage;

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use objects::{object::StateId, store::FsStore};
use serde::Serialize;

use crate::{
    compact_measure::{BlobMeasurement, TreeMeasurement, measure_blobs, measure_trees},
    compact_state_measure::{StateMeasurement, measure_states},
    git_baseline::{GitBaseline, measure_git},
    lineage::build_lineage_order,
    model::{ObjectCounts, ObjectRef, load_object_set},
    tree_lineage::{TreeLineageStats, tree_path_order},
};

const DEFAULT_FRAME_BYTES: usize = 12 * 1024 * 1024;
const INDEX_BYTES_PER_OBJECT: u64 = 36;

#[derive(Serialize)]
struct Results {
    repository: String,
    frame_bytes: usize,
    index_bytes_per_object: u64,
    object_counts: ObjectCounts,
    git: GitBaseline,
    trees: TreeMeasurement,
    states: StateMeasurement,
    blobs: BlobMeasurement,
    tree_lineage: TreeLineageStats,
    lineage_walk: lineage::LineageStats,
    comparisons: Comparisons,
    regated_total: RegatedTotal,
}

#[derive(Serialize)]
struct KindComparison {
    git_raw_bytes: u64,
    native_msgpack_bytes: u64,
    positional_msgpack_bytes: u64,
    compact_raw_bytes: u64,
    compact_compressed_bytes: u64,
    compact_raw_to_git: f64,
    compact_compressed_to_git: f64,
    msgpack_key_bytes: u64,
}

#[derive(Serialize)]
struct Comparisons {
    trees: KindComparison,
    states: KindComparison,
}

#[derive(Serialize)]
struct RegatedTotal {
    compact_metadata_bytes: u64,
    blob_lineage_frame_bytes: u64,
    index_bytes: u64,
    total_bytes: u64,
    git_pack_bytes: u64,
    git_pack_and_idx_bytes: u64,
    total_to_git_pack: f64,
    total_to_git_pack_and_idx: f64,
}

fn main() -> Result<()> {
    let (repo_root, git_dir, output_dir, frame_bytes) = parse_args()?;
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
    let counts = objects.counts();
    eprintln!(
        "native objects: {} states, {} trees, {} blobs",
        counts.states, counts.trees, counts.blobs
    );

    let git = measure_git(&git_dir)?;
    validate_corpus_counts(&counts, &git)?;
    let lineage = build_lineage_order(&store, &objects)?;
    measure::write_renames(&output_dir.join("renames.tsv"), &lineage.renames)?;
    let state_order = lineage
        .order
        .iter()
        .filter_map(|object| match object {
            ObjectRef::State(id) => Some(*id),
            _ => None,
        })
        .collect::<Vec<StateId>>();
    let (tree_order, tree_lineage) = tree_path_order(&store, &objects, &state_order)?;

    let states = measure_states(&store, &state_order, &output_dir, frame_bytes)?;
    let trees = measure_trees(&store, &tree_order, &output_dir, frame_bytes)?;
    let blobs = measure_blobs(&store, &lineage.order, &output_dir, frame_bytes)?;
    let comparisons = comparisons(&git, &trees, &states);
    let regated_total = regate(&counts, &git, &trees, &states, &blobs);
    let results = Results {
        repository: repo_root.display().to_string(),
        frame_bytes,
        index_bytes_per_object: INDEX_BYTES_PER_OBJECT,
        object_counts: counts,
        git,
        trees,
        states,
        blobs,
        tree_lineage,
        lineage_walk: lineage.stats,
        comparisons,
        regated_total,
    };
    let json = serde_json::to_vec_pretty(&results)?;
    fs::write(output_dir.join("results.json"), &json)?;
    println!("{}", String::from_utf8(json)?);
    Ok(())
}

fn comparisons(
    git: &GitBaseline,
    trees: &TreeMeasurement,
    states: &StateMeasurement,
) -> Comparisons {
    Comparisons {
        trees: compare(git.trees.bytes, &trees.common),
        states: compare(git.commits.bytes, &states.common),
    }
}

fn compare(git_raw_bytes: u64, value: &compact_measure::CommonMeasurement) -> KindComparison {
    KindComparison {
        git_raw_bytes,
        native_msgpack_bytes: value.source_msgpack_bytes,
        positional_msgpack_bytes: value.source_positional_msgpack_bytes,
        compact_raw_bytes: value.compact_raw_bytes,
        compact_compressed_bytes: value.compressed_bytes,
        compact_raw_to_git: ratio(value.compact_raw_bytes, git_raw_bytes),
        compact_compressed_to_git: ratio(value.compressed_bytes, git_raw_bytes),
        msgpack_key_bytes: value
            .source_msgpack_bytes
            .saturating_sub(value.source_positional_msgpack_bytes),
    }
}

fn regate(
    counts: &ObjectCounts,
    git: &GitBaseline,
    trees: &TreeMeasurement,
    states: &StateMeasurement,
    blobs: &BlobMeasurement,
) -> RegatedTotal {
    let compact_metadata_bytes = trees.common.compressed_bytes + states.common.compressed_bytes;
    let index_bytes = counts.total as u64 * INDEX_BYTES_PER_OBJECT;
    let total_bytes = compact_metadata_bytes + blobs.common.compressed_bytes + index_bytes;
    let git_pack_and_idx_bytes = git.pack_bytes + git.idx_bytes;
    RegatedTotal {
        compact_metadata_bytes,
        blob_lineage_frame_bytes: blobs.common.compressed_bytes,
        index_bytes,
        total_bytes,
        git_pack_bytes: git.pack_bytes,
        git_pack_and_idx_bytes,
        total_to_git_pack: ratio(total_bytes, git.pack_bytes),
        total_to_git_pack_and_idx: ratio(total_bytes, git_pack_and_idx_bytes),
    }
}

fn validate_corpus_counts(counts: &ObjectCounts, git: &GitBaseline) -> Result<()> {
    if counts.states as u64 != git.commits.objects
        || counts.trees as u64 != git.trees.objects
        || counts.blobs as u64 != git.blobs.objects
    {
        bail!("native and Git corpus object counts differ");
    }
    Ok(())
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 / denominator as f64
}

fn parse_args() -> Result<(PathBuf, PathBuf, PathBuf, usize)> {
    let mut args = env::args_os().skip(1);
    let usage =
        "usage: native_lineage_solid <adopted-repo> <git-dir> <fresh-output-dir> [frame-mib]";
    let repo_root = args.next().with_context(|| usage)?;
    let git_dir = args.next().with_context(|| usage)?;
    let output_dir = args.next().with_context(|| usage)?;
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
        bail!(usage);
    }
    Ok((
        repo_root.into(),
        git_dir.into(),
        output_dir.into(),
        frame_bytes,
    ))
}
