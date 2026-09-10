// SPDX-License-Identifier: MIT OR Apache-2.0

/// A fail-closed verification error.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// A field, enum, canonical ordering, or fixed-width value is invalid.
    #[error("invalid owner-authorization object: {0}")]
    Invalid(String),
    /// A cryptographic signature or its signer id is invalid.
    #[error("owner-authorization signature verification failed")]
    InvalidSignature,
    /// A signed chain is detached, forked, duplicated, or incomplete.
    #[error("owner-authorization chain is broken: {0}")]
    BrokenChain(String),
    /// A recovery threshold is not satisfied.
    #[error("recovery threshold is not satisfied: required {required}, got {actual}")]
    RecoveryThreshold {
        /// Required number of distinct guardian signatures.
        required: u32,
        /// Number of valid distinct guardian signatures supplied.
        actual: usize,
    },
    /// A verified capability does not cover the requested operation.
    #[error("capability does not grant {0}")]
    CapabilityDenied(String),
    /// The evidence has expired at the caller-supplied evaluation time.
    #[error("owner-authorization object is expired")]
    Expired,
    /// The evidence is not yet valid at the caller-supplied evaluation time.
    #[error("owner-authorization object is not yet valid")]
    NotYetValid,
    /// Protobuf bytes have aliases, unknown fields, duplicates, or trailing data.
    #[error("owner-authorization protobuf is not canonical")]
    NonCanonicalProtobuf,
    /// An encoded input exceeds a v2 byte ceiling.
    #[error("owner-authorization input exceeds {limit} bytes")]
    TooLarge {
        /// Maximum accepted byte count.
        limit: usize,
    },
    /// A protobuf object could not be decoded.
    #[error("owner-authorization protobuf decode failed: {0}")]
    Decode(String),
    /// Subject Biscuit verification failed.
    #[error("owner-authorization Biscuit failed: {0}")]
    Biscuit(String),
}

/// Result type for verification primitives.
pub type Result<T> = std::result::Result<T, Error>;

impl From<prost::DecodeError> for Error {
    fn from(error: prost::DecodeError) -> Self {
        Self::Decode(error.to_string())
    }
}
