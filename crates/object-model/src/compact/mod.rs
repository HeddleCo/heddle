// SPDX-License-Identifier: Apache-2.0
//! Lossless columnar encodings used by background compact repacks.

mod dictionary;
mod io;
mod state;
mod state_decode;
mod tree;

pub use state::{encode_state_frame, is_state_frame};
pub use state_decode::decode_state_frame;
use thiserror::Error;
pub use tree::{decode_tree_frame, encode_tree_frame, encoded_tree_size, is_tree_frame};

/// Compact metadata encoding failure.
#[derive(Debug, Error)]
pub enum CompactError {
    /// The frame is malformed, truncated, or internally inconsistent.
    #[error("invalid compact frame: {0}")]
    Invalid(String),
    /// A decoded tree violates the native tree invariants.
    #[error(transparent)]
    Tree(#[from] crate::object::TreeError),
    /// A decoded Git object id is invalid.
    #[error("invalid compact git object id: {0}")]
    GitObjectId(String),
    /// A decoded spool id is invalid.
    #[error("invalid compact spool id: {0}")]
    SpoolId(String),
    /// A compact verification value cannot be serialized losslessly.
    #[error(transparent)]
    MessagePackEncode(#[from] rmp_serde::encode::Error),
    /// A compact verification value cannot be deserialized losslessly.
    #[error(transparent)]
    MessagePackDecode(#[from] rmp_serde::decode::Error),
}

pub(crate) type Result<T> = std::result::Result<T, CompactError>;

pub(crate) fn invalid(message: impl Into<String>) -> CompactError {
    CompactError::Invalid(message.into())
}

#[cfg(test)]
mod tests;
