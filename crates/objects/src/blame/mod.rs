// SPDX-License-Identifier: Apache-2.0
//! Storage-neutral resumable blame slices.
//!
//! Heddle owns ancestry walk, path lookup, first-parent merge policy, LCS
//! tie-breaking, origin finalization, and provenance invariants. Callers own
//! job rows, leases, and persistence of the returned frontier.

mod advance;
mod finalize;
mod lookup;
mod mapping;
mod parent;
mod prepare;
mod run;
mod types;

#[cfg(test)]
mod tests;

pub use advance::advance_file_blame_slice;
pub use finalize::finalize_file_provenance;
pub use prepare::prepare_file_blame;
pub use run::blame_file;
pub use types::{
    BlameFrontierGroup, BlameFrontierRecord, BlameLineMap, BlamePreparation, BlameSliceAdvance,
    BlameSliceError, BlameSliceLimits, OriginRange, origin_from_state,
};
