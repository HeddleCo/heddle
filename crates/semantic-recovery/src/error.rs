// SPDX-License-Identifier: Apache-2.0

use std::{io, path::PathBuf};

/// Failures isolated to the rebuildable semantic recovery sidecar.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// A local model asset was missing.
    #[error("semantic recovery model asset is missing: {0}")]
    MissingModelAsset(PathBuf),
    /// The supplied ONNX model was not the pinned artifact.
    #[error("semantic recovery model digest mismatch: expected {expected}, found {actual}")]
    ModelDigestMismatch {
        /// Required SHA-256 digest.
        expected: &'static str,
        /// Digest of the supplied file.
        actual: String,
    },
    /// Local embedding inference failed.
    #[error("local embedding failed: {0}")]
    Embedding(String),
    /// Vectors or documents did not satisfy the index contract.
    #[error("invalid recovery index input: {0}")]
    InvalidInput(String),
    /// The sidecar encoding was corrupt or unsupported.
    #[error("invalid recovery sidecar: {0}")]
    InvalidSidecar(String),
    /// A filesystem operation failed.
    #[error("semantic recovery sidecar I/O failed: {0}")]
    Io(#[from] io::Error),
    /// MessagePack encoding or decoding failed.
    #[error("semantic recovery sidecar codec failed: {0}")]
    Codec(String),
}

/// Result type for semantic recovery operations.
pub type Result<T> = std::result::Result<T, RecoveryError>;
