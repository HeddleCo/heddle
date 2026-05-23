// SPDX-License-Identifier: Apache-2.0
//! Cryptographic signature metadata for states.

use serde::{Deserialize, Serialize};

/// Signature information for a state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSignature {
    /// Signature algorithm identifier.
    pub algorithm: String,
    /// Public key in hex format.
    pub public_key: String,
    /// Signature in hex format.
    pub signature: String,
}

impl StateSignature {
    /// Get the algorithm name.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }
}

/// Signature verification result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    /// The signature is valid.
    Valid,
    /// The signature is invalid.
    Invalid,
    /// The state has no signature.
    Unsigned,
}

impl SignatureStatus {
    /// Check if this represents a valid signature.
    pub fn is_valid(self) -> bool {
        self == SignatureStatus::Valid
    }

    /// Check if this represents an unsigned state.
    pub fn is_unsigned(self) -> bool {
        self == SignatureStatus::Unsigned
    }
}
