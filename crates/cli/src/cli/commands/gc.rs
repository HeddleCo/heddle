// SPDX-License-Identifier: Apache-2.0
//! Garbage collection command - clean up unreachable objects.
//!
//! **Redaction tombstones are structurally permanent.** They live in
//! `<heddle_dir>/redactions/<blob-hex>.bin`, *outside* the `objects/`
//! subtree GC operates on. `pack_objects` only walks loose blobs/trees
//! (see `crates/objects/src/store/fs/fs_pack.rs`); `prune_loose_objects`
//! only drops bytes whose canonical copy now lives in a pack. Neither
//! ever observes or touches a redaction file — they cannot be packed,
//! cannot be pruned, and cannot be lost to a `gc --prune --aggressive`
//! pass. We report the pinned count so the audit trail in `heddle gc`
//! output makes the invariant visible to operators.

use anyhow::Result;
use repo::Repository;
use serde::Serialize;

#[cfg(feature = "git-overlay")]
use crate::bridge::GitBridge;
use crate::cli::{Cli, render::write_json_stdout, should_output_json};

#[derive(Serialize, Default)]
struct GcOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    dry_run: bool,
    prune: bool,
    packed_count: u64,
    bytes_saved: u64,
    pruned_loose: u64,
    bytes_freed: u64,
    pinned_redactions: usize,
    preserved_redactions: usize,
    #[cfg(feature = "git-overlay")]
    pruned_git_mapping_entries: usize,
}

pub fn cmd_gc(cli: &Cli, prune: bool, aggressive: bool, dry_run: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let json = should_output_json(cli, Some(repo.config()));
    let mut summary = GcOutput {
        output_kind: "gc",
        action: "gc",
        status: "ok",
        dry_run,
        prune,
        ..Default::default()
    };

    // Snapshot redactions before GC so we can both report the pinned
    // count and (post-GC) assert that no record was disturbed. The
    // assertion is defence-in-depth: GC structurally cannot reach
    // these files, but the audit step costs O(redactions) and gives
    // operators a hard guarantee in writing.
    let redactions_before = repo.list_all_redactions().unwrap_or_default();
    let pinned_redactions: usize = redactions_before
        .iter()
        .map(|(_, blob)| blob.redactions.len())
        .sum();
    summary.pinned_redactions = pinned_redactions;

    if dry_run {
        let blobs = repo.store().list_blobs()?;
        let trees = repo.store().list_trees()?;
        let total_objects = blobs.len() + trees.len();
        summary.packed_count = total_objects as u64;
        summary.status = "dry_run";

        if !json {
            println!(
                "Would pack {} objects ({} blobs, {} trees)",
                total_objects,
                blobs.len(),
                trees.len()
            );

            if prune {
                println!("Would prune loose objects after packing");
            }
            if pinned_redactions > 0 {
                println!(
                    "Pinned {pinned_redactions} redaction tombstone(s) — never collected by GC"
                );
            }
        }
    } else {
        let (packed_count, bytes_saved) = repo.store().pack_objects(aggressive)?;
        summary.packed_count = packed_count;
        summary.bytes_saved = bytes_saved;

        if !json {
            if packed_count > 0 {
                println!(
                    "Packed {} objects (saved {} bytes)",
                    packed_count, bytes_saved
                );
            } else {
                println!("No objects to pack");
            }
        }

        repo.refs().pack_refs()?;

        #[cfg(feature = "git-overlay")]
        {
            let mut bridge = GitBridge::new(&repo);
            if bridge.is_initialized() {
                let removed = bridge.prune_unreachable_mapping_entries()?;
                summary.pruned_git_mapping_entries = removed;
                if !json && removed > 0 {
                    println!("Pruned {removed} stale Git-overlay mapping entries");
                }
            }
        }

        if prune {
            let (removed, bytes_freed) = repo.store().prune_loose_objects()?;
            summary.pruned_loose = removed;
            summary.bytes_freed = bytes_freed;
            if !json {
                if removed > 0 {
                    println!(
                        "Pruned {} loose objects (freed {} bytes)",
                        removed, bytes_freed
                    );
                } else {
                    println!("No loose objects to prune");
                }
            }
        }

        // Post-GC invariant: every redaction we saw at the start of
        // this run must still exist. We compare by (blob_hash,
        // redaction_count) — if a record disappeared, GC's structural
        // boundary was breached and the next reader would see secrets
        // we promised to hide.
        let redactions_after = repo.list_all_redactions().unwrap_or_default();
        let before_index: std::collections::HashMap<_, _> = redactions_before
            .iter()
            .map(|(blob, b)| (*blob, b.redactions.len()))
            .collect();
        for (blob, after_blob) in &redactions_after {
            let before_count = before_index.get(blob).copied().unwrap_or(0);
            if after_blob.redactions.len() < before_count {
                anyhow::bail!(
                    "GC invariant violated: redactions on blob {} dropped from {} to {} — \
                     refusing to claim a successful GC",
                    blob.short(),
                    before_count,
                    after_blob.redactions.len()
                );
            }
        }
        for (blob, _) in &redactions_before {
            if !redactions_after.iter().any(|(b, _)| b == blob) {
                anyhow::bail!(
                    "GC invariant violated: redactions file for blob {} disappeared — \
                     refusing to claim a successful GC",
                    blob.short()
                );
            }
        }
        if pinned_redactions > 0 {
            summary.preserved_redactions = pinned_redactions;
            if !json {
                println!(
                    "Preserved {pinned_redactions} redaction tombstone(s) across GC \
                     (structurally outside the object store)"
                );
            }
        }
    }

    if json {
        write_json_stdout(&summary)?;
    }
    Ok(())
}
