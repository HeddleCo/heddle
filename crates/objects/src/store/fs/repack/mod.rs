// SPDX-License-Identifier: Apache-2.0
//! Filesystem payload for the storage-agnostic repack scheduler.

mod blob_lineage;
#[cfg(test)]
mod blob_lineage_tests;
mod blob_renames;
mod blob_writer;
mod cheap_fold;
mod compact;
mod cutover;
mod operation;
mod staging;

pub use cheap_fold::{AutomaticRepackLock, SnapshotPackFold};
pub(super) use cutover::acquire_repack_lock_blocking;
pub use operation::FsRepackOperation;

#[cfg(test)]
mod tests;
