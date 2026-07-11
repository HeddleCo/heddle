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
//! pass. We report the pinned count so the audit trail in `heddle maintenance gc`
//! output makes the invariant visible to operators.

use anyhow::Result;
use heddle_core::{
    gc_plan::{
        gc_consolidated_mirror_message, gc_dry_run_messages, gc_pack_message,
        gc_preserved_redactions_message, gc_prune_loose_message, gc_pruned_git_mapping_message,
        gc_status_token, plan_gc_dry_run,
    },
    maintenance_plan::{run_pack_install_recover_line, run_unpaired_packs_pruned_line},
};
use objects::store::{AnyStore, ObjectStore, recover_pack_install_intents};
use serde::Serialize;

use crate::cli::{Cli, render::write_json_stdout, should_output_json};
#[cfg(feature = "git-overlay")]
use crate::git_projection_engine::GitProjection;

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
    /// L8 Option D: unpaired `.pack` files removed (no matching `.idx`).
    unpaired_packs_pruned: u64,
    /// L8 install-intent recover: intents completed during this GC.
    pack_install_intents_completed: u64,
    /// L8 install-intent recover: intents aborted during this GC.
    pack_install_intents_aborted: u64,
    pinned_redactions: usize,
    preserved_redactions: usize,
    #[cfg(feature = "git-overlay")]
    pruned_git_mapping_entries: usize,
    #[cfg(feature = "git-overlay")]
    consolidated_mirror_loose: usize,
}

pub fn cmd_gc(cli: &Cli, prune: bool, aggressive: bool, dry_run: bool) -> Result<()> {
    let repo = cli.open_repo()?;
    let json = should_output_json(cli, Some(repo.config()));
    let mut summary = GcOutput {
        output_kind: "gc",
        action: "gc",
        status: gc_status_token(dry_run),
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
        let plan = plan_gc_dry_run(blobs.len(), trees.len());
        summary.packed_count = plan.packed_count;
        summary.status = plan.status;

        if !json {
            let _ = prune;
            for line in gc_dry_run_messages(blobs.len(), trees.len(), pinned_redactions) {
                println!("{line}");
            }
        }
    } else {
        let (packed_count, bytes_saved) = repo.store().pack_objects(aggressive)?;
        summary.packed_count = packed_count;
        summary.bytes_saved = bytes_saved;

        if !json {
            println!("{}", gc_pack_message(packed_count, bytes_saved));
        }

        repo.refs().pack_refs()?;

        #[cfg(feature = "git-overlay")]
        {
            let mut bridge = GitProjection::new(&repo);
            if bridge.is_initialized() {
                let removed = bridge.prune_unreachable_mapping_entries()?;
                summary.pruned_git_mapping_entries = removed;
                if !json {
                    if let Some(msg) = gc_pruned_git_mapping_message(removed) {
                        println!("{msg}");
                    }
                }

                // Consolidate the Bridge Mirror (`.heddle/git`): pack its
                // loose objects and drop the redundant loose copies. The mirror
                // is a separate object store (Sley's Git ODB) from Heddle's
                // native store packed above, and accumulates a loose object per
                // minted/imported commit, tree, and blob — the dominant
                // uninstrumented read cost. Lossless + OID-preserving (packs
                // every object on disk, content-addressed); see
                // `GitProjection::consolidate_mirror`.
                let consolidated = bridge.consolidate_mirror()?;
                summary.consolidated_mirror_loose = consolidated;
                if !json {
                    if let Some(msg) = gc_consolidated_mirror_message(consolidated) {
                        println!("{msg}");
                    }
                }
            }
        }

        // Consolidation prune: drop the loose copies of objects that now
        // live in the pack we just wrote. This is intrinsic to what a GC
        // *is* — a GC that packs without pruning leaves every object in
        // BOTH places, so the object store has strictly more sources to
        // search and read commands (status/diff/verification) get slower
        // instead of faster. The prune only removes loose objects whose
        // canonical copy is now in a pack, so it never loses data
        // (fsck stays clean). It therefore runs unconditionally, not
        // behind `--prune`. The `prune`/`aggressive` flags are retained
        // for callers/scripts but no longer gate this safe step.
        let _ = prune;
        let (removed, bytes_freed) = repo.store().prune_loose_objects()?;
        summary.pruned_loose = removed;
        summary.bytes_freed = bytes_freed;
        if !json {
            println!("{}", gc_prune_loose_message(removed, bytes_freed));
        }

        // L8 residual: recover pack-install intents, then prune unpaired
        // packs (Option D). Safe for correctness — loaders never open
        // unpaired packs — and bounds crash-window disk leak. Prefer the
        // public recover free-fn + FsStore::prune_unpaired_packs when the
        // store is the filesystem backend.
        let packs = repo.heddle_dir().join("packs");
        let recover = recover_pack_install_intents(&packs)?;
        summary.pack_install_intents_completed = recover.completed;
        summary.pack_install_intents_aborted = recover.aborted;
        if !json {
            println!(
                "{}",
                run_pack_install_recover_line(recover.completed, recover.aborted)
            );
        }
        let (unpaired_removed, unpaired_bytes) = match repo.store() {
            AnyStore::Fs(fs) => fs.prune_unpaired_packs()?,
        };
        summary.unpaired_packs_pruned = unpaired_removed;
        if !json {
            println!(
                "{}",
                run_unpaired_packs_pruned_line(unpaired_removed, unpaired_bytes)
            );
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
                if let Some(msg) = gc_preserved_redactions_message(pinned_redactions) {
                    println!("{msg}");
                }
            }
        }
    }

    if json {
        write_json_stdout(&summary)?;
    }
    Ok(())
}
