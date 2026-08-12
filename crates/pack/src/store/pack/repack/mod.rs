// SPDX-License-Identifier: Apache-2.0
//! Storage-agnostic background repack scheduling.
//!
//! The scheduler decides when and how much work may run. Implementations own
//! their storage-specific prepare, verification, and atomic-cutover protocol.
//! Native packs, hosted projections, and future compact frame writers can
//! therefore share resource control without sharing storage assumptions.

mod policy;
mod scheduler;
mod types;

pub use policy::{RepackInventory, RepackPolicy, RepackReason};
pub use scheduler::RepackScheduler;
pub use types::{
    CancellationToken, LoadMonitor, RepackContext, RepackError, RepackHandle, RepackOperation,
    RepackOutcome, RepackReport, RepackResourceLimits, RepackSchedule,
};

#[cfg(test)]
mod tests;
