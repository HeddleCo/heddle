// SPDX-License-Identifier: Apache-2.0
//! Reproduce the bundled v1 tree/state zstd dictionary and its holdout measurement.

use std::{env, fs, path::PathBuf};

use chrono::{TimeZone, Utc};
use objects::object::{
    Attribution, ChangeId, ContentHash, Principal, State, StateId, Tree, TreeEntry,
};

const DICTIONARY_SIZE: usize = 8 * 1024;
const MIN_COMPRESSION_SIZE: usize = 256;
const PLAIN_HEADER_LEN: usize = 9;
const DICTIONARY_HEADER_LEN: usize = 13;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: train_tree_state_dictionary <output-path>")?;
    let training = corpus(0..512, 0..384)?;
    let holdout = corpus(512..640, 384..512)?;
    let dictionary = zstd::dict::from_samples(&training, DICTIONARY_SIZE)?;
    fs::write(&output, &dictionary)?;

    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dictionary)?;
    let raw_size: usize = holdout.iter().map(Vec::len).sum();
    let plain_size: usize = holdout
        .iter()
        .map(|sample| {
            if sample.len() < MIN_COMPRESSION_SIZE {
                return Ok(sample.len());
            }
            zstd::encode_all(sample.as_slice(), 3).map(|encoded| {
                if encoded.len() < sample.len() {
                    encoded.len() + PLAIN_HEADER_LEN
                } else {
                    sample.len()
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    let dictionary_size: usize = holdout
        .iter()
        .map(|sample| {
            if sample.len() < MIN_COMPRESSION_SIZE {
                return Ok(sample.len());
            }
            compressor.compress(sample).map(|encoded| {
                if encoded.len() < sample.len() {
                    encoded.len() + DICTIONARY_HEADER_LEN
                } else {
                    sample.len()
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    let reduction = 100.0 * (plain_size - dictionary_size) as f64 / plain_size as f64;

    println!("training samples: {}", training.len());
    println!(
        "training bytes: {}",
        training.iter().map(Vec::len).sum::<usize>()
    );
    println!("dictionary bytes: {}", dictionary.len());
    println!("holdout samples: {}", holdout.len());
    println!("holdout raw bytes: {raw_size}");
    println!("holdout plain-zstd bytes: {plain_size}");
    println!("holdout dictionary-zstd bytes: {dictionary_size}");
    println!("holdout reduction: {reduction:.2}%");
    println!("dictionary blake3: {}", blake3::hash(&dictionary).to_hex());
    Ok(())
}

fn corpus(
    trees: impl Iterator<Item = usize>,
    states: impl Iterator<Item = usize>,
) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    trees
        .map(tree_sample)
        .chain(states.map(state_sample))
        .collect()
}

fn tree_sample(index: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const NAMES: &[&str] = &[
        "AGENTS.md",
        "Cargo.lock",
        "Cargo.toml",
        "CONTEXT.md",
        "LICENSE",
        "README.md",
        "architecture.rs",
        "codec.rs",
        "compression.rs",
        "config.rs",
        "error.rs",
        "fs_store.rs",
        "hash.rs",
        "lib.rs",
        "main.rs",
        "manifest.rs",
        "mod.rs",
        "object.rs",
        "pack.rs",
        "reader.rs",
        "repository.rs",
        "state.rs",
        "store.rs",
        "tests.rs",
        "tree.rs",
        "types.rs",
        "varint.rs",
        "wire.rs",
        "writer.rs",
    ];
    let entry_count = 8 + index % 57;
    let entries = (0..entry_count)
        .map(|entry| {
            let base = NAMES[(entry + index / 8) % NAMES.len()];
            let name = format!("{entry:03}_{base}");
            let generation = index / 8;
            let hash = ContentHash::compute(
                format!("tree-generation-{generation}-entry-{entry}").as_bytes(),
            );
            if entry.is_multiple_of(11) {
                TreeEntry::directory(name, hash)
            } else if entry.is_multiple_of(17) {
                TreeEntry::symlink(name, hash)
            } else {
                TreeEntry::file(name, hash, entry.is_multiple_of(13))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rmp_serde::to_vec(&Tree::from_entries(entries))?)
}

fn state_sample(index: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let tree = ContentHash::compute(format!("state-tree-{index}").as_bytes());
    let parent_count = 1 + usize::from(index.is_multiple_of(19));
    let parents = (0..parent_count)
        .map(|parent| {
            let mut bytes = [0_u8; 32];
            bytes[..8].copy_from_slice(&(index.saturating_sub(parent + 1) as u64).to_be_bytes());
            StateId::from_bytes(bytes)
        })
        .collect();
    let attribution = Attribution::human(Principal::new(
        format!("Contributor {}", index % 23),
        format!("contributor{}@example.com", index % 23),
    ));
    let mut state = State::new(tree, parents, attribution)
        .with_intent(format!("Refine storage path for revision {index}"))
        .with_confidence(0.75 + (index % 25) as f32 / 100.0);
    let mut change_id = [0_u8; 16];
    change_id[..8].copy_from_slice(&(index as u64).to_be_bytes());
    state.change_id = ChangeId::from_bytes(change_id);
    state.created_at = Utc
        .timestamp_opt(1_700_000_000 + index as i64 * 60, 0)
        .single()
        .ok_or("invalid fixture timestamp")?;
    Ok(rmp_serde::to_vec(&state)?)
}
