// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle prove` planning helpers (no hosted network / FS I/O).
//!
//! Status labels and host/repo validation are pure so the CLI can keep
//! protobuf transport, file writes, and recovery advice locally.

/// Identity-proof status kinds aligned with hosted `ProofStatus` wire values
/// (0 unspecified, 1 pending, 2 verified, 3 failed) without generated API types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofStatusKind {
    Unspecified,
    Pending,
    Verified,
    Failed,
}

impl ProofStatusKind {
    /// Map a raw proto `i32` status to a kind (unknown values → unspecified).
    pub fn from_i32(status: i32) -> Self {
        match status {
            1 => Self::Pending,
            2 => Self::Verified,
            3 => Self::Failed,
            _ => Self::Unspecified,
        }
    }

    /// Stable human/machine status token.
    pub fn label(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Unspecified => "unspecified",
        }
    }
}

/// Status label string for a proof status kind.
pub fn proof_status_label(kind: ProofStatusKind) -> &'static str {
    kind.label()
}

/// Failure when start-form host/repo positionals are incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRepoPlanError {
    /// Host and/or repo missing when no subcommand is present.
    MissingHostOrRepo,
}

impl HostRepoPlanError {
    /// Stable CLI error message (matches historical prove start-form copy).
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingHostOrRepo => {
                "a host and repo are required (e.g. `heddle prove github.com owner/repo`); \
                 for other actions use `heddle prove submit <challenge_id>` or `heddle prove list`"
            }
        }
    }
}

impl std::fmt::Display for HostRepoPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HostRepoPlanError {}

/// Validate start-form positionals: both `host` and `repo` are required when
/// no subcommand is given.
pub fn require_host_repo<'a>(
    host: Option<&'a str>,
    repo: Option<&'a str>,
) -> Result<(&'a str, &'a str), HostRepoPlanError> {
    match (host, repo) {
        (Some(h), Some(r)) => Ok((h, r)),
        _ => Err(HostRepoPlanError::MissingHostOrRepo),
    }
}

/// Non-negative seconds from an optional protobuf-style `seconds` field.
pub fn timestamp_secs_u64(seconds: Option<i64>) -> u64 {
    seconds.map(|s| s.max(0) as u64).unwrap_or(0)
}

/// RFC3339 (or raw seconds) label for verified_at display; empty when zero.
pub fn format_unix_secs_label(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

/// Optional follow-up line after `prove submit` based on status.
pub fn proof_submit_followup(kind: ProofStatusKind, challenge_id: &str) -> Option<String> {
    match kind {
        ProofStatusKind::Verified => Some("Your control of the repo is verified.".to_string()),
        ProofStatusKind::Pending => Some(format!(
            "The marker was not found yet. Push the file, then retry: heddle prove submit {challenge_id}"
        )),
        ProofStatusKind::Failed => Some(format!(
            "Verification failed. Check the marker line + path, then retry: heddle prove submit {challenge_id}"
        )),
        ProofStatusKind::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_covers_every_variant() {
        assert_eq!(proof_status_label(ProofStatusKind::Verified), "verified");
        assert_eq!(proof_status_label(ProofStatusKind::Pending), "pending");
        assert_eq!(proof_status_label(ProofStatusKind::Failed), "failed");
        assert_eq!(
            proof_status_label(ProofStatusKind::Unspecified),
            "unspecified"
        );
        assert_eq!(ProofStatusKind::from_i32(0), ProofStatusKind::Unspecified);
        assert_eq!(ProofStatusKind::from_i32(1), ProofStatusKind::Pending);
        assert_eq!(ProofStatusKind::from_i32(2), ProofStatusKind::Verified);
        assert_eq!(ProofStatusKind::from_i32(3), ProofStatusKind::Failed);
        assert_eq!(ProofStatusKind::from_i32(99), ProofStatusKind::Unspecified);
    }

    #[test]
    fn host_repo_guard() {
        assert_eq!(
            require_host_repo(Some("github.com"), Some("owner/repo")).unwrap(),
            ("github.com", "owner/repo")
        );
        assert_eq!(
            require_host_repo(None, Some("owner/repo")),
            Err(HostRepoPlanError::MissingHostOrRepo)
        );
        assert_eq!(
            require_host_repo(Some("github.com"), None),
            Err(HostRepoPlanError::MissingHostOrRepo)
        );
        assert!(
            HostRepoPlanError::MissingHostOrRepo
                .message()
                .contains("host and repo")
        );
    }

    #[test]
    fn timestamp_and_submit_followup() {
        assert_eq!(timestamp_secs_u64(None), 0);
        assert_eq!(timestamp_secs_u64(Some(-1)), 0);
        assert_eq!(format_unix_secs_label(0), "");
        assert!(!format_unix_secs_label(1_700_000_000).is_empty());

        assert_eq!(
            proof_submit_followup(ProofStatusKind::Verified, "c1").as_deref(),
            Some("Your control of the repo is verified.")
        );
        assert!(
            proof_submit_followup(ProofStatusKind::Pending, "c1")
                .unwrap()
                .contains("heddle prove submit c1")
        );
        assert!(
            proof_submit_followup(ProofStatusKind::Failed, "c2")
                .unwrap()
                .contains("heddle prove submit c2")
        );
        assert_eq!(
            proof_submit_followup(ProofStatusKind::Unspecified, "c"),
            None
        );
    }
}
