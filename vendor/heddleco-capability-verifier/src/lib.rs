// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pure verification for Heddle's owner-anchored authorization contract.
//!
//! The crate accepts caller-supplied public evidence and time, and returns an
//! authorization decision. It performs no I/O, obtains no clock, and exposes
//! no private-key construction or signing API.

mod canonical;
mod capability;
mod crypto;
mod decision;
mod error;
mod keyring;
mod limits;
mod operation;
mod owner;
mod transfer;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub mod conformance;

pub use capability::{
    VerifiedAuthorizationBundle, VerifiedCapability, verify_authorization_bundle,
    verify_capability_chain,
};
pub use decision::{Decision, Denial};
pub use error::{Error, Result};
pub use keyring::{VerifiedCloneKeyring, verify_clone_keyring, verify_clone_keyring_bytes};
pub use limits::VerificationLimits;
pub use operation::{
    PurgeContext, canonical_purge_operation, verify_purge_authorization,
    verify_purge_authorization_bytes,
};
pub use owner::{
    DEFAULT_RECOVERY_WINDOW_SECS, VerifiedOwnerBinding, VerifiedOwnerState,
    VerifiedSpoolOwnerGenesis, apply_transition, apply_transition_with_timelock,
    effective_recovery_window, verify_owner_key_binding, verify_owner_root,
    verify_spool_owner_genesis, verify_transition_timelock,
};
pub use transfer::{
    TransferOwner, VerifiedResourceTransfer, verify_resource_transfer, verify_transfer_audit_chain,
};

/// Generated public wire types used by this verifier.
pub mod wire {
    pub use heddle_api::heddle::api::v1alpha1::*;
}

/// The exact API contract version used by this release line.
pub const HEDDLE_API_REQUIREMENT: &str = "0.15";

#[cfg(test)]
mod tests;
