// SPDX-License-Identifier: Apache-2.0

use std::io;

use crypto::{AeadError, SignerError};
use heddle_object_model::object::FacetKind;

/// Typed reason a broker request was denied. Callers classify by variant, not
/// by matching message text (see `heddle env run` recovery advice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerDenialReason {
    /// The request or grant is past its expiry.
    Expired,
    /// A purpose other than `run` was requested.
    PurposeNotAllowed,
    /// No provider handle is held for the named slot's recipients.
    NoProviderHandle(String),
    /// The requested time-to-live exceeds the broker's ceiling.
    TtlTooLong,
}

impl std::fmt::Display for BrokerDenialReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => f.write_str("request expired"),
            Self::PurposeNotAllowed => f.write_str("only the run purpose is authorized"),
            Self::NoProviderHandle(slot) => {
                write!(f, "no provider handle for slot {slot}")
            }
            Self::TtlTooLong => f.write_str("requested ttl exceeds the broker maximum"),
        }
    }
}

impl BrokerDenialReason {
    /// Stable machine code for IPC / recovery-advice classification.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::PurposeNotAllowed => "purpose_not_allowed",
            Self::NoProviderHandle(_) => "no_provider_handle",
            Self::TtlTooLong => "ttl_too_long",
        }
    }
}

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
    #[error("broker refused the request: {0}")]
    BrokerDenied(BrokerDenialReason),
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

pub type Result<T> = std::result::Result<T, EnvStoreError>;
