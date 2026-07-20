// SPDX-License-Identifier: Apache-2.0
//! Shared error types across Heddle crates.

use std::{error::Error, fmt, path::Path};

use crate::object::{ContentHash, StateId, TreeError};

/// Structured recovery details that can cross the embeddable facade boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryDetails {
    pub kind: &'static str,
    pub error: String,
    pub hint: String,
    pub unsafe_condition: String,
    pub would_change: String,
    pub preserved: String,
    /// Explicit, path-specific recovery commands. When present these override
    /// the `kind`-keyed fallback the CLI envelope would otherwise reconstruct
    /// (the first entry is the primary command). `None` = use the generic
    /// per-`kind` recovery mapping.
    pub recovery_commands: Option<Vec<String>>,
}

impl RecoveryDetails {
    pub fn safety_refusal(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        already_preserved: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            error: error.into(),
            hint: hint.into(),
            unsafe_condition: unsafe_condition.into(),
            would_change: would_change.into(),
            preserved: already_preserved.into(),
            recovery_commands: None,
        }
    }

    /// Attach explicit, path-specific recovery commands (the first entry is the
    /// primary command). Used where the callsite has context — e.g. a source
    /// checkout path — that the `kind`-keyed CLI fallback cannot reconstruct.
    #[must_use]
    pub fn with_recovery_commands(mut self, commands: Vec<String>) -> Self {
        self.recovery_commands = Some(commands);
        self
    }

    pub fn invalid_usage(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::safety_refusal(
            kind,
            error,
            hint,
            "the command arguments do not describe a valid operation",
            "running with ambiguous or invalid arguments could target the wrong repository state or metadata",
            "no repository objects, refs, metadata, or worktree files were changed",
        )
    }

    pub fn feature_unavailable(command: &str, feature: &str) -> Self {
        Self::safety_refusal(
            "feature_unavailable",
            format!("{command} requires building heddle with --features {feature}"),
            format!(
                "Use a binary built with the `{feature}` feature, or rerun without the feature-specific flag."
            ),
            format!("this heddle binary was built without the `{feature}` feature"),
            format!("{command} cannot run because the requested analysis engine is unavailable"),
            "repository state, refs, and worktree files were left unchanged",
        )
    }

    pub fn serialization_error(detail: impl fmt::Display) -> Self {
        Self::safety_refusal(
            "state_corrupted",
            "Repository state is corrupted or unreadable",
            "Inspect repository integrity before attempting repair.",
            format!("a stored repository object failed to decode: {detail}"),
            "continuing would read or write through repository state Heddle cannot decode",
            "the command stopped before mutating repository state; intact objects were left unchanged",
        )
    }

    pub fn repository_integrity_error(error: impl Into<String>) -> Self {
        Self::safety_refusal(
            "repository_integrity_error",
            error,
            "Inspect repository integrity, then restore or repair the reported object/ref.",
            "repository object or ref integrity did not pass validation",
            "continuing could compound corruption or hide the missing object",
            "the command stopped before applying the requested mutation",
        )
    }

    pub fn repository_not_found(path: &Path) -> Self {
        Self::safety_refusal(
            "repository_not_found",
            format!("repository not found at {}", path.display()),
            "Initialize the requested repository before running repository commands.",
            format!("no Heddle repository was found at '{}'", path.display()),
            "the command cannot inspect or change repository state until initialization",
            "no repository objects, refs, metadata, or worktree files were changed",
        )
    }

    pub fn state_not_found(state_id: impl fmt::Display) -> Self {
        Self::safety_refusal(
            "state_not_found",
            format!("State not found: {state_id}"),
            "List recent states with `heddle log`, then choose an existing state id.",
            "the requested state id does not exist in this repository",
            "continuing with a guessed state could target the wrong history point",
            "repository state, refs, metadata, and worktree files were left unchanged",
        )
    }
}

impl fmt::Display for RecoveryDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}. Unsafe: {}. Would change: {}. Preserved: {}.",
            self.error, self.unsafe_condition, self.would_change, self.preserved
        )?;
        Ok(())
    }
}

impl Error for RecoveryDetails {}

/// Error type for repository/storage-adjacent operations.
#[derive(Debug, thiserror::Error)]
pub enum HeddleError {
    #[error("{0}")]
    Recovery(Box<RecoveryDetails>),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("No merge in progress")]
    NoMergeInProgress,
    #[error("state not found: {0}")]
    StateNotFound(StateId),
    #[error("invalid object: {0}")]
    InvalidObject(String),
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
    #[error(
        "repository at {path} predates format v{required} (found v{found}); recreate it or re-adopt its Git history with this Heddle version"
    )]
    RepositoryFormatMigrationRequired {
        path: std::path::PathBuf,
        found: u32,
        required: u32,
    },
    #[error(
        "{storage} uses format {found}, but this binary supports {supported}; upgrade Heddle before opening it"
    )]
    StorageFormatTooNew {
        storage: String,
        found: u32,
        supported: u32,
    },
    #[error(
        "{storage} predates required format {required} (found {found}); recreate the repository or re-adopt its Git history with this Heddle version"
    )]
    StorageFormatMigrationRequired {
        storage: String,
        found: u32,
        required: u32,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("repository lock unavailable: {0}")]
    Lock(#[from] crate::lock::LockError),
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
    #[error(
        "invalid {key}: '{value}' — valid values are {} (in {path})",
        valid_values.join(" or ")
    )]
    ConfigInvalidValue {
        path: std::path::PathBuf,
        key: String,
        value: String,
        valid_values: Vec<String>,
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
    pub fn recovery(details: RecoveryDetails) -> Self {
        HeddleError::Recovery(Box::new(details))
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

impl From<crate::object::SemanticIndexError> for HeddleError {
    fn from(e: crate::object::SemanticIndexError) -> Self {
        HeddleError::InvalidObject(e.to_string())
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
    use super::{HeddleError, RecoveryDetails};

    #[test]
    fn safety_refusal_formats_domain_details() {
        let details = RecoveryDetails::safety_refusal(
            "example",
            "error",
            "hint",
            "unsafe",
            "would change",
            "preserved",
        );

        assert_eq!(
            details.to_string(),
            "error. Unsafe: unsafe. Would change: would change. Preserved: preserved."
        );
    }

    #[test]
    fn recovery_error_displays_structured_error_copy() {
        let err = HeddleError::recovery(RecoveryDetails::serialization_error("bad marker"));

        assert!(err.to_string().contains("Repository state is corrupted"));
        assert!(!err.to_string().contains("heddle fsck --full"));
    }
}
