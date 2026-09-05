// SPDX-License-Identifier: Apache-2.0

use std::io;

use crypto::{AeadError, SignerError};
use heddle_object_model::object::FacetKind;

/// Failures from the local runtime-profile store.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeProfileError {
    #[error("runtime-profile encoding failed: {0}")]
    Encoding(String),
    #[error("runtime-profile decoding failed: {0}")]
    Decoding(String),
    #[error("unsupported runtime-profile schema version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid runtime profile: {0}")]
    Invalid(String),
    #[error("profile {0} was not found")]
    ProfileNotFound(String),
    #[error("slot {0} was not found")]
    SlotNotFound(String),
    #[error("recipient {0} was not found")]
    RecipientNotFound(String),
    #[error("lifecycle {from} cannot move to {to}")]
    IllegalLifecycle { from: String, to: String },
    #[error("refusing to decrypt a {0} runtime-profile version")]
    DecryptForbidden(String),
    #[error("broker refused the request: {0}")]
    BrokerDenied(String),
    #[error("decrypt grant {0} is not valid")]
    InvalidGrant(String),
    #[error("reserved materialization path {0} cannot be captured")]
    ReservedMaterialization(String),
    #[error("{0} is not a source-history root")]
    FacetExcluded(FacetKind),
    #[error("aead error: {0}")]
    Aead(#[from] AeadError),
    #[error("signature error: {0}")]
    Signature(#[from] SignerError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeProfileError>;
