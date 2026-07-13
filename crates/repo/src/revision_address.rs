// SPDX-License-Identifier: Apache-2.0
//! Explicit addresses for Heddle-native states and Git-backed revisions.

use std::{fmt, str::FromStr};

use objects::object::StateId;
use serde::{Deserialize, Serialize};

/// A durable address for a revision-like object.
///
/// `StateId` remains the identifier for Heddle-native captures. Git-overlay
/// checkpoints and external Git refs can point at a Git commit without
/// pretending that commit is itself a Heddle [`State`](objects::object::State).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RevisionAddress {
    Heddle(StateId),
    #[cfg(feature = "git-overlay")]
    GitCommit(String),
}

impl RevisionAddress {
    pub fn heddle(state_id: StateId) -> Self {
        Self::Heddle(state_id)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_commit(oid: impl Into<String>) -> Self {
        Self::GitCommit(oid.into())
    }

    pub fn storage_prefix(&self) -> &'static str {
        match self {
            Self::Heddle(_) => "heddle",
            #[cfg(feature = "git-overlay")]
            Self::GitCommit(_) => "git",
        }
    }
}

impl fmt::Display for RevisionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Heddle(state_id) => write!(f, "heddle:{}", state_id.to_string_full()),
            #[cfg(feature = "git-overlay")]
            Self::GitCommit(oid) => write!(f, "git:{oid}"),
        }
    }
}

impl FromStr for RevisionAddress {
    type Err = RevisionAddressParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(value) = input.strip_prefix("heddle:") {
            return StateId::parse(value)
                .map(Self::Heddle)
                .map_err(|error| RevisionAddressParseError::InvalidHeddle(error.to_string()));
        }
        #[cfg(feature = "git-overlay")]
        if let Some(value) = input.strip_prefix("git:") {
            let oid = value.trim();
            if oid.is_empty() {
                return Err(RevisionAddressParseError::EmptyGitCommit);
            }
            validate_git_commit_oid(oid)?;
            return Ok(Self::GitCommit(oid.to_ascii_lowercase()));
        }
        Err(RevisionAddressParseError::UnknownPrefix)
    }
}

#[cfg(feature = "git-overlay")]
fn validate_git_commit_oid(oid: &str) -> Result<(), RevisionAddressParseError> {
    match oid.len() {
        40 | 64 if oid.bytes().all(|byte| byte.is_ascii_hexdigit()) => Ok(()),
        40 | 64 => Err(RevisionAddressParseError::InvalidGitCommit(
            "commit oid must be lowercase or uppercase hex".to_string(),
        )),
        len => Err(RevisionAddressParseError::InvalidGitCommit(format!(
            "commit oid must be 40 or 64 hex characters, got {len}"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RevisionAddressParseError {
    #[error("revision address must start with `heddle:` or `git:`")]
    UnknownPrefix,
    #[error("git revision address is missing a commit oid")]
    EmptyGitCommit,
    #[cfg(feature = "git-overlay")]
    #[error("invalid Git commit oid: {0}")]
    InvalidGitCommit(String),
    #[error("invalid Heddle state id: {0}")]
    InvalidHeddle(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heddle_revision_address_round_trips() {
        let change = crate::test_state_id();
        let address = RevisionAddress::heddle(change);

        assert_eq!(
            address.to_string().parse::<RevisionAddress>().unwrap(),
            address
        );
        assert_eq!(address.storage_prefix(), "heddle");
    }

    #[cfg(feature = "git-overlay")]
    #[test]
    fn git_revision_address_round_trips() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let address = RevisionAddress::git_commit(oid);

        assert_eq!(
            address.to_string().parse::<RevisionAddress>().unwrap(),
            address
        );
        assert_eq!(address.storage_prefix(), "git");
    }

    #[cfg(feature = "git-overlay")]
    #[test]
    fn git_revision_address_normalizes_oid_case_on_parse() {
        let lowercase_sha1 = "0123456789abcdef0123456789abcdef01234567";
        let uppercase_sha1 = "0123456789ABCDEF0123456789ABCDEF01234567";
        let lowercase_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let uppercase_sha256 = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";

        let from_lower_sha1 = format!("git:{lowercase_sha1}")
            .parse::<RevisionAddress>()
            .expect("parse lowercase sha1");
        let from_upper_sha1 = format!("git:{uppercase_sha1}")
            .parse::<RevisionAddress>()
            .expect("parse uppercase sha1");
        let from_lower_sha256 = format!("git:{lowercase_sha256}")
            .parse::<RevisionAddress>()
            .expect("parse lowercase sha256");
        let from_upper_sha256 = format!("git:{uppercase_sha256}")
            .parse::<RevisionAddress>()
            .expect("parse uppercase sha256");

        assert_eq!(from_upper_sha1, from_lower_sha1);
        assert_eq!(from_upper_sha256, from_lower_sha256);
        assert_eq!(from_upper_sha1.to_string(), format!("git:{lowercase_sha1}"));
        assert_eq!(
            from_upper_sha256.to_string(),
            format!("git:{lowercase_sha256}")
        );
    }

    #[cfg(feature = "git-overlay")]
    #[test]
    fn git_revision_address_rejects_invalid_oids() {
        assert_eq!(
            "git:".parse::<RevisionAddress>().unwrap_err(),
            RevisionAddressParseError::EmptyGitCommit,
        );

        assert_eq!(
            "git:not-an-oid".parse::<RevisionAddress>().unwrap_err(),
            RevisionAddressParseError::InvalidGitCommit(
                "commit oid must be 40 or 64 hex characters, got 10".to_string(),
            ),
        );

        assert_eq!(
            "git:gggggggggggggggggggggggggggggggggggggggg"
                .parse::<RevisionAddress>()
                .unwrap_err(),
            RevisionAddressParseError::InvalidGitCommit(
                "commit oid must be lowercase or uppercase hex".to_string(),
            ),
        );
    }
}
