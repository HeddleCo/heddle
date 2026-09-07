// SPDX-License-Identifier: Apache-2.0

use std::io;

use crypto::{AeadError, SignerError};

/// Failures from the local env-store store.
#[derive(Debug, thiserror::Error)]
pub enum EnvStoreError {
    #[error("env-store encoding failed: {0}")]
    Encoding(String),
    #[error("env-store decoding failed: {0}")]
    Decoding(String),
    #[error("unsupported env-store schema version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid env store: {0}")]
    Invalid(String),
    #[error("profile {0} was not found")]
    ProfileNotFound(String),
    #[error("slot {0} was not found")]
    SlotNotFound(String),
    #[error("recipient {0} was not found")]
    RecipientNotFound(String),
    #[error("lifecycle {from} cannot move to {to}")]
    IllegalLifecycle { from: String, to: String },
    #[error("refusing to decrypt a {0} env-store version")]
    DecryptForbidden(String),
    #[error("no provider handle for slot {0}")]
    NoProviderHandle(String),
    #[error("aead error: {0}")]
    Aead(#[from] AeadError),
    #[error("signature error: {0}")]
    Signature(#[from] SignerError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, EnvStoreError>;
