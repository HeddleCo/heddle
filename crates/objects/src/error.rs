// SPDX-License-Identifier: Apache-2.0
//! Shared error types across Heddle crates.

use crate::object::{ChangeId, ContentHash, TreeError};

/// Machine-readable storage error category.
///
/// Hosted/cloud adapters should return errors that classify into these
/// buckets so callers can decide retry, CAS handling, and corruption policy
/// without parsing human-facing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageErrorKind {
    Missing,
    Conflict,
    CasMismatch,
    AlreadyExists,
    Transient,
    Corrupt,
    Unsupported,
    Invalid,
    Other,
}

impl std::fmt::Display for StorageErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            StorageErrorKind::Missing => "missing",
            StorageErrorKind::Conflict => "conflict",
            StorageErrorKind::CasMismatch => "cas-mismatch",
            StorageErrorKind::AlreadyExists => "already-exists",
            StorageErrorKind::Transient => "transient",
            StorageErrorKind::Corrupt => "corrupt",
            StorageErrorKind::Unsupported => "unsupported",
            StorageErrorKind::Invalid => "invalid",
            StorageErrorKind::Other => "other",
        };
        f.write_str(name)
    }
}

/// Error type for repository/storage-adjacent operations.
#[derive(Debug, thiserror::Error)]
pub enum HeddleError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("state not found: {0}")]
    StateNotFound(ChangeId),
    #[error("invalid object: {0}")]
    InvalidObject(String),
    #[error("storage {kind}: {message}")]
    Storage {
        kind: StorageErrorKind,
        message: String,
    },
    #[error("repository not found at {0}")]
    RepositoryNotFound(std::path::PathBuf),
    #[error("repository already exists at {0}")]
    RepositoryExists(std::path::PathBuf),
    #[error(
        "repository config at {path} uses repository format {found} but this binary supports {supported}; upgrade heddle or run `heddle migrate`"
    )]
    RepositoryFormatTooNew {
        path: std::path::PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("configuration parse error at {path}: {source}")]
    ConfigParse {
        path: std::path::PathBuf,
        // Keep the original `toml::de::Error` as the error source — not a
        // flattened string — so `HeddleExitCode::from_error` can still
        // downcast through the chain and classify config-parse failures as
        // EX_DATAERR (65) rather than falling through to EX_IOERR (74).
        #[source]
        source: toml::de::Error,
    },
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("compression error: {0}")]
    Compression(String),
    #[error("invalid ref name: {0}")]
    InvalidRefName(String),
    #[error("file too large: {0} bytes")]
    InvalidFileSize(u64),
    #[error("symlink target escapes repository: {0}")]
    InvalidSymlinkTarget(std::path::PathBuf),
    #[error("object corruption: expected {expected}, found {found}")]
    Corruption {
        expected: ContentHash,
        found: ContentHash,
    },
    #[error(
        "missing {object_type} object: {id} (run `heddle fsck --full` to inspect store integrity)"
    )]
    MissingObject { object_type: String, id: String },
    #[error("invalid tree entry: {0}")]
    InvalidTreeEntry(#[from] TreeError),
}

impl HeddleError {
    pub fn storage(kind: StorageErrorKind, message: impl Into<String>) -> Self {
        Self::Storage {
            kind,
            message: message.into(),
        }
    }

    pub fn storage_kind(&self) -> StorageErrorKind {
        match self {
            HeddleError::NotFound(_)
            | HeddleError::StateNotFound(_)
            | HeddleError::MissingObject { .. } => StorageErrorKind::Missing,
            HeddleError::Conflict(_) => StorageErrorKind::Conflict,
            HeddleError::Corruption { .. } => StorageErrorKind::Corrupt,
            HeddleError::InvalidObject(_) | HeddleError::InvalidTreeEntry(_) => {
                StorageErrorKind::Invalid
            }
            HeddleError::RepositoryExists(_) => StorageErrorKind::AlreadyExists,
            HeddleError::Io(err) => match err.kind() {
                std::io::ErrorKind::NotFound => StorageErrorKind::Missing,
                std::io::ErrorKind::AlreadyExists => StorageErrorKind::AlreadyExists,
                std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::WouldBlock => StorageErrorKind::Transient,
                _ => StorageErrorKind::Other,
            },
            HeddleError::Storage { kind, .. } => *kind,
            _ => StorageErrorKind::Other,
        }
    }

    pub fn is_retryable_storage_error(&self) -> bool {
        self.storage_kind() == StorageErrorKind::Transient
    }
}

impl From<rmp_serde::encode::Error> for HeddleError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        HeddleError::Serialization(e.to_string())
    }
}

impl From<rmp_serde::decode::Error> for HeddleError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        HeddleError::Serialization(e.to_string())
    }
}

impl From<toml::de::Error> for HeddleError {
    fn from(e: toml::de::Error) -> Self {
        HeddleError::Config(e.to_string())
    }
}

impl From<toml::ser::Error> for HeddleError {
    fn from(e: toml::ser::Error) -> Self {
        HeddleError::Config(e.to_string())
    }
}

impl From<serde_json::Error> for HeddleError {
    fn from(e: serde_json::Error) -> Self {
        HeddleError::Serialization(e.to_string())
    }
}

/// Result type for repository/storage-adjacent operations.
pub type Result<T> = std::result::Result<T, HeddleError>;

#[cfg(test)]
mod tests {
    use super::{HeddleError, StorageErrorKind};

    #[test]
    fn storage_error_kind_classifies_io_and_explicit_storage_errors() {
        let timed_out = HeddleError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "remote store timed out",
        ));
        assert_eq!(timed_out.storage_kind(), StorageErrorKind::Transient);
        assert!(timed_out.is_retryable_storage_error());

        let unsupported =
            HeddleError::storage(StorageErrorKind::Unsupported, "backend cannot write packs");
        assert_eq!(unsupported.storage_kind(), StorageErrorKind::Unsupported);
        assert!(!unsupported.is_retryable_storage_error());
    }
}
