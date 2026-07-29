//! Inert client primitives for owner-anchored authorization.
//!
//! This module deliberately has no hosted transport, CLI dispatch, bearer
//! conversion, or clone hook. It can construct and verify the reviewed wire
//! objects and persist a clone keyring, but no current authorization path
//! imports it. Weft's exclusive cutover must wire the module and remove the
//! server-minted path in the same change.

mod anonymous;
mod bootstrap;
mod canonical;
mod capability;
mod error;
mod key;
mod keyring;
mod keyring_verification;
mod limits;
mod recovery;
mod root;
mod transition;
pub mod wire;

pub use anonymous::{
    create_anonymous_credential, create_anonymous_registration, verify_anonymous_credential,
    verify_anonymous_registration,
};
pub use bootstrap::{
    bootstrap_challenge, create_deferred_bootstrap, create_human_bootstrap,
    verify_bootstrap_response, verify_deferred_bootstrap,
};
pub use capability::{
    VerifiedAuthorizationBundle, VerifiedCapability, create_child_capability,
    create_direct_capability, mint_subject_biscuit, verify_authorization_bundle,
    verify_capability_chain,
};
pub use error::{AuthorizationError, Result};
pub use key::{AuthorizationKey, PaperRecoveryKit};
pub use keyring::{
    CloneKeyringStore, OfflineAuthorizer, OfflineRequest, VerifiedCloneKeyring,
    create_clone_keyring,
};
pub use limits::VerificationLimits;
pub use recovery::{
    CustodialRecoveryConfirmation, GuardianSigner, RecoverySetup, confirm_custodial_weft_only,
};
pub use root::{
    VerifiedOwnerState, create_deferred_owner_root, create_human_owner_root, verify_owner_root,
};
pub use transition::{
    apply_transition, create_claim_transition, create_recovery_policy_transition,
    create_recovery_transition, create_rotation_transition,
};
