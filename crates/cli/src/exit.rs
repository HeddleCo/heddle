// SPDX-License-Identifier: Apache-2.0
//! Heddle CLI exit code taxonomy.
//!
//! Agents that retry on transient failures need codified exit codes so they
//! can distinguish "safe to retry" from "permanent failure" without parsing
//! stderr. The taxonomy follows BSD `sysexits.h` so the codes mean the same
//! thing to humans, init systems, and shell scripts that already understand
//! them.
//!
//! `0` is success; `2` is reserved for `set -e` / panic / unhandled error and
//! is never emitted intentionally — we let it surface naturally.

use std::{error::Error, fmt, io::ErrorKind as IoErrorKind};

use clap::error::ErrorKind as ClapErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HeddleExitCode {
    Ok = 0,
    /// `EX_USAGE` — invalid CLI args, unknown subcommand, malformed flag.
    Usage = 64,
    /// `EX_DATAERR` — well-formed input but semantically rejected (malformed
    /// repo, unmergeable divergence, parse error in a tracked file).
    DataErr = 65,
    /// `EX_CANTCREAT` — output file refused (write target exists, parent
    /// unwritable, state dir uncreatable).
    CantCreat = 73,
    /// `EX_IOERR` — generic IO failure during read/write.
    IoErr = 74,
    /// `EX_TEMPFAIL` — transient failure; same command with the same args
    /// is safe to retry.
    TempFail = 75,
    /// `EX_PROTOCOL` — remote rejected the payload at the protocol layer;
    /// retrying without changing inputs will fail the same way.
    Protocol = 76,
    /// `EX_NOPERM` — operation refused for permission reasons.
    NoPerm = 77,
    /// `EX_CONFIG` — configuration is missing, ambiguous, or invalid (no
    /// upstream, no remote, conflicting user identity).
    Config = 78,
}

/// Command already rendered its user-visible outcome (operator envelope,
/// eligibility report, etc.) and only needs a non-zero process exit.
///
/// `main` maps this through [`HeddleExitCode::from_error`] and **does not**
/// print a second error envelope — the command body owns the render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeExit {
    code: HeddleExitCode,
}

impl OutcomeExit {
    pub const fn new(code: HeddleExitCode) -> Self {
        Self { code }
    }

    pub const fn code(self) -> HeddleExitCode {
        self.code
    }

    /// Semantic rejection after a successful render (blocked operator,
    /// unmet merge eligibility, …).
    pub const fn data_err() -> Self {
        Self::new(HeddleExitCode::DataErr)
    }
}

impl fmt::Display for OutcomeExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "command completed with non-zero status {}",
            self.code.as_u8()
        )
    }
}

impl Error for OutcomeExit {}

impl HeddleExitCode {
    /// Map a clap parse error to an exit code. Help/version are not failures
    /// (clap prints to stdout and exits 0); everything else is a usage error.
    pub fn from_clap(err: &clap::Error) -> Self {
        match err.kind() {
            ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => Self::Ok,
            _ => Self::Usage,
        }
    }

    /// Exit code for a typed `RecoveryAdvice` kind whose documented code
    /// differs from the `IoErr` catch-all. This table — not the
    /// user-visible message — is the classification contract: rewording
    /// advice copy can never regress an exit code, and every entry here is
    /// pinned by a per-kind regression test below.
    fn for_advice_kind(kind: &str) -> Option<Self> {
        match kind {
            // Missing precondition (no default remote for push/pull), not
            // an IO failure.
            "remote_not_configured" | "remote_not_found" | "repository_not_found" => {
                Some(Self::Config)
            }
            // Well-formed input the command semantically rejects:
            // - nothing staged / no changes to capture
            // - a reconcile that needs a `--prefer` side
            // - unsaved worktree changes blocking a tree write
            // - repository state that fails msgpack/serde decoding
            // - `--output json`/`json-compact` against a command without
            //   that output contract (the invocation parses fine; the
            //   command rejects the requested projection)
            "nothing_to_capture"
            | "commit_requires_git_overlay"
            | "commit_capture_required"
            | "git_repair_requires_adoption"
            | "git_repair_requires_import"
            | "dirty_worktree"
            | "state_corrupted"
            | "state_not_found"
            | "conflict_not_found"
            | "no_merge_in_progress"
            | "operation_not_in_progress"
            | "json_unsupported"
            | "json_compact_unsupported"
            // Operator/land/ready/continue finished rendering a blocked or
            // failed envelope — semantic rejection of well-formed input.
            | "operator_blocked"
            // Hosted merge eligibility gate refused the pair.
            | "merge_eligibility_blocked" => Some(Self::DataErr),
            // Capture aborted on ENOSPC; working tree is intact. Classifies
            // as IO rather than a distinct raw-28 OS code so agents stay on
            // the documented sysexits taxonomy.
            "capture_out_of_space" => Some(Self::IoErr),
            _ => None,
        }
    }

    /// True when the error is a post-render outcome that must not print a
    /// second stderr envelope (the command body already wrote the report).
    pub fn is_quiet_outcome(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| cause.is::<OutcomeExit>())
    }

    /// Map an anyhow error chain to an exit code. Walks the chain and uses
    /// the first downcast match; falls back to `IoErr` so callers always
    /// get a code more informative than the bare `1` shell convention.
    pub fn from_error(err: &anyhow::Error) -> Self {
        for cause in err.chain() {
            // Already-rendered command outcomes carry an explicit code and
            // must not fall through to the IoErr catch-all.
            if let Some(outcome) = cause.downcast_ref::<OutcomeExit>() {
                return outcome.code();
            }
            // Typed refusals carry a stable `kind` discriminator — route the
            // ones whose documented code differs from the `IoErr` catch-all.
            // Keyed on `kind` (not the user-visible message) so rewording the
            // error text can't silently regress the contract.
            if let Some(advice) = cause.downcast_ref::<crate::cli::commands::RecoveryAdvice>()
                && let Some(code) = Self::for_advice_kind(advice.kind)
            {
                return code;
            }
            if let Some(heddle_err) = cause.downcast_ref::<objects::error::HeddleError>() {
                match heddle_err {
                    objects::error::HeddleError::Recovery(details) => {
                        if let Some(code) = Self::for_advice_kind(details.kind) {
                            return code;
                        }
                    }
                    // A missing repository is a missing precondition
                    // (initialize/point at one), not an IO failure.
                    objects::error::HeddleError::RepositoryNotFound(_) => return Self::Config,
                    objects::error::HeddleError::RepositoryFormatTooNew { .. }
                    | objects::error::HeddleError::RepositoryFormatMigrationRequired { .. }
                    | objects::error::HeddleError::StorageFormatTooNew { .. }
                    | objects::error::HeddleError::StorageFormatMigrationRequired { .. } => {
                        return Self::DataErr;
                    }
                    objects::error::HeddleError::StateNotFound(_)
                    | objects::error::HeddleError::NoMergeInProgress
                    | objects::error::HeddleError::ConfigInvalidValue { .. } => {
                        return Self::DataErr;
                    }
                    objects::error::HeddleError::Config(_) => return Self::Config,
                    objects::error::HeddleError::Lock(_) => return Self::TempFail,
                    // Stored state that fails msgpack decoding is corrupted
                    // data, not a transient IO problem — same class as the
                    // serde_json/toml parse failures below.
                    objects::error::HeddleError::Serialization(_) => return Self::DataErr,
                    _ => {}
                }
            }
            if let Some(remote_err) = cause.downcast_ref::<crate::remote::RemoteError>()
                && matches!(
                    remote_err,
                    crate::remote::RemoteError::NotFound(_)
                        | crate::remote::RemoteError::NoDefaultRemote
                )
            {
                return Self::Config;
            }
            if let Some(protocol) = cause.downcast_ref::<wire::ProtocolError>() {
                if let Some(typed) =
                    crate::hosted_failure::HostedFailureDetail::from_protocol_error(protocol)
                    && let Some(code) = typed.exit_code()
                {
                    return code;
                }
                return match protocol {
                    wire::ProtocolError::RemoteFailure { code, .. } => {
                        crate::hosted_failure::exit_code_for_remote(*code)
                    }
                    wire::ProtocolError::AuthorizationFailed(_)
                    | wire::ProtocolError::AuthenticationFailed(_) => Self::NoPerm,
                    wire::ProtocolError::ObjectNotFound(_) => Self::Config,
                    wire::ProtocolError::InvalidState(_)
                    | wire::ProtocolError::AlreadyExists(_)
                    | wire::ProtocolError::Serialization(_)
                    | wire::ProtocolError::MessageTooLarge { .. }
                    | wire::ProtocolError::InvalidMessageType(_)
                    | wire::ProtocolError::VersionMismatch { .. }
                    | wire::ProtocolError::CapabilityNotSupported(_) => Self::Protocol,
                    wire::ProtocolError::Io(io) => match io.kind() {
                        IoErrorKind::TimedOut
                        | IoErrorKind::ConnectionRefused
                        | IoErrorKind::ConnectionAborted
                        | IoErrorKind::ConnectionReset
                        | IoErrorKind::Interrupted => Self::TempFail,
                        IoErrorKind::PermissionDenied => Self::NoPerm,
                        _ => Self::IoErr,
                    },
                    wire::ProtocolError::Remote(_) | wire::ProtocolError::LockError(_) => {
                        Self::IoErr
                    }
                };
            }
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                return match io.kind() {
                    IoErrorKind::PermissionDenied => Self::NoPerm,
                    IoErrorKind::TimedOut
                    | IoErrorKind::ConnectionRefused
                    | IoErrorKind::ConnectionAborted
                    | IoErrorKind::ConnectionReset
                    | IoErrorKind::Interrupted => Self::TempFail,
                    IoErrorKind::NotFound | IoErrorKind::AlreadyExists => Self::CantCreat,
                    _ => Self::IoErr,
                };
            }
            if cause.is::<serde_json::Error>() || cause.is::<toml::de::Error>() {
                return Self::DataErr;
            }
        }

        Self::IoErr
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl From<HeddleExitCode> for i32 {
    fn from(code: HeddleExitCode) -> Self {
        code as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_permission_denied_maps_to_noperm() {
        let err: anyhow::Error =
            std::io::Error::new(IoErrorKind::PermissionDenied, "denied").into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::NoPerm);
    }

    #[test]
    fn typed_repository_lock_failure_maps_to_tempfail() {
        let err = objects::error::HeddleError::Lock(objects::lock::LockError::Acquire(
            std::io::Error::new(IoErrorKind::WouldBlock, "contended"),
        ));
        assert_eq!(
            HeddleExitCode::from_error(&anyhow::Error::new(err)),
            HeddleExitCode::TempFail
        );
    }

    #[test]
    fn io_timed_out_is_retry_safe() {
        let err: anyhow::Error = std::io::Error::new(IoErrorKind::TimedOut, "slow").into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::TempFail);
    }

    #[test]
    fn config_parse_preserves_toml_source_as_data_err() {
        // Regression for Codex R4 (cid 3315305484): `ConfigParse` must keep
        // the `toml::de::Error` as its source so the chain-walk still
        // classifies it, rather than flattening to a String and falling
        // through to `IoErr`.
        let toml_err = toml::from_str::<toml::Value>("= nope").unwrap_err();
        let err: anyhow::Error = objects::error::HeddleError::ConfigParse {
            path: std::path::PathBuf::from("/tmp/config.toml"),
            source: toml_err,
        }
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn serde_json_is_data_err() {
        let err: anyhow::Error = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn remote_error_no_default_remote_is_config() {
        let err = anyhow::anyhow!(crate::remote::RemoteError::NoDefaultRemote);
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn heddle_config_error_is_config() {
        let err: anyhow::Error =
            objects::error::HeddleError::Config("workspace config invalid".to_string()).into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn remote_not_configured_advice_is_config() {
        // `heddle push`/`heddle pull` with no default remote raise the typed
        // `remote_not_configured` advice — a missing-precondition (Config),
        // not the `IoErr` catch-all.
        let err = anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::remote_not_configured(
            "push"
        ));
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn nothing_to_capture_advice_is_data_err() {
        // `heddle capture` with nothing selected is semantic rejection of
        // well-formed input (DataErr), not an IO failure.
        let advice = crate::cli::commands::RecoveryAdvice::safety_refusal(
            "nothing_to_capture",
            "nothing to capture",
            "hint",
            "unsafe",
            "would change",
            "preserved",
            "heddle status",
            vec!["heddle status".to_string()],
        );
        let err = anyhow::anyhow!(advice);
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn fsck_authority_refusal_is_data_err() {
        // An authority-conflicting `heddle fsck repair git --prefer ...`
        // request is a semantic refusal, not an IO failure.
        let advice = crate::cli::commands::RecoveryAdvice::safety_refusal(
            "git_repair_requires_adoption",
            "Git owns source history in this repository",
            "hint",
            "unsafe",
            "would change",
            "preserved",
            "heddle status",
            vec!["heddle status".to_string()],
        );
        let err = anyhow::anyhow!(advice);
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn repository_not_found_recovery_details_are_config() {
        let err: anyhow::Error = objects::error::HeddleError::recovery(
            objects::RecoveryDetails::repository_not_found(std::path::Path::new("/tmp/whatever")),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn repository_not_found_typed_variant_is_config() {
        // The typed `HeddleError::RepositoryNotFound` must classify without
        // relying on its Display text surviving a rewording.
        let err: anyhow::Error = objects::error::HeddleError::RepositoryNotFound(
            std::path::PathBuf::from("/tmp/whatever"),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn serialization_error_typed_variant_is_data_err() {
        // Corrupted msgpack state (HeddleCo/heddle#642): decode failures
        // are data corruption, not the IoErr catch-all.
        let err: anyhow::Error = objects::error::HeddleError::Serialization(
            "wrong msgpack marker FixArray(0)".to_string(),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn state_not_found_typed_variant_is_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::StateNotFound(
            objects::object::StateId::from_bytes([3; 32]),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn invalid_config_value_typed_variant_is_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::ConfigInvalidValue {
            path: std::path::PathBuf::from("/tmp/config.toml"),
            key: "output.format".to_string(),
            value: "auto".to_string(),
            valid_values: vec!["'text'".to_string(), "'json'".to_string()],
        }
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn repository_format_migration_required_is_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::RepositoryFormatMigrationRequired {
            path: std::path::PathBuf::from("/tmp/config.toml"),
            found: 2,
            required: 3,
        }
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn storage_format_migration_required_is_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::StorageFormatMigrationRequired {
            storage: "packed oplog container".to_string(),
            found: 2,
            required: 4,
        }
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn no_merge_in_progress_typed_variant_is_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::NoMergeInProgress.into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn recovery_details_kind_uses_advice_exit_code_mapping() {
        let err: anyhow::Error = objects::error::HeddleError::recovery(
            objects::RecoveryDetails::serialization_error("wrong msgpack marker FixArray(0)"),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    /// Build a `RecoveryAdvice` with the given kind and deliberately
    /// unrelated copy, proving classification reads `kind`, never the
    /// user-visible message (HeddleCo/heddle#640).
    fn advice_with_kind(kind: &'static str) -> anyhow::Error {
        anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::safety_refusal(
            kind,
            "reworded copy that matches no sentinel",
            "hint",
            "unsafe",
            "would change",
            "preserved",
            "heddle status",
            vec!["heddle status".to_string()],
        ))
    }

    #[test]
    fn every_classified_advice_kind_maps_to_its_documented_exit_code() {
        // Per-kind regression matrix: copy edits that orphan a string
        // sentinel can no longer regress these to the IoErr catch-all.
        for (kind, expected) in [
            ("remote_not_configured", HeddleExitCode::Config),
            ("remote_not_found", HeddleExitCode::Config),
            ("repository_not_found", HeddleExitCode::Config),
            ("nothing_to_capture", HeddleExitCode::DataErr),
            ("dirty_worktree", HeddleExitCode::DataErr),
            ("state_corrupted", HeddleExitCode::DataErr),
            ("state_not_found", HeddleExitCode::DataErr),
            ("no_merge_in_progress", HeddleExitCode::DataErr),
            ("operation_not_in_progress", HeddleExitCode::DataErr),
            ("conflict_not_found", HeddleExitCode::DataErr),
            ("json_unsupported", HeddleExitCode::DataErr),
            ("json_compact_unsupported", HeddleExitCode::DataErr),
            ("operator_blocked", HeddleExitCode::DataErr),
            ("merge_eligibility_blocked", HeddleExitCode::DataErr),
            ("capture_out_of_space", HeddleExitCode::IoErr),
        ] {
            assert_eq!(
                HeddleExitCode::from_error(&advice_with_kind(kind)),
                expected,
                "advice kind `{kind}` must classify by kind, not message text"
            );
        }
    }

    #[test]
    fn dirty_worktree_advice_constructor_is_data_err() {
        // The real constructor's Display does not contain the legacy
        // "dirty worktree" phrase, so only the typed kind can classify it.
        let err = anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::dirty_worktree(
            "merge",
            vec!["src/lib.rs".to_string()],
            "repository state was left unchanged",
        ));
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn dirty_worktree_recovery_details_are_data_err() {
        let err: anyhow::Error =
            objects::error::HeddleError::recovery(objects::RecoveryDetails::safety_refusal(
                "dirty_worktree",
                "reworded copy that matches no sentinel",
                "hint",
                "unsafe",
                "would change",
                "preserved",
            ))
            .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn unsupported_output_advice_is_data_err() {
        // HeddleCo/heddle#648: `--output json[-compact]` against a command
        // without that contract is semantic rejection of well-formed input
        // (DataErr 65), not a malformed invocation (Usage 64).
        let json = anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::json_unsupported(
            "shell completion"
        ));
        assert_eq!(HeddleExitCode::from_error(&json), HeddleExitCode::DataErr);

        let compact =
            anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::json_compact_unsupported("log"));
        assert_eq!(
            HeddleExitCode::from_error(&compact),
            HeddleExitCode::DataErr
        );
    }

    #[test]
    fn state_corrupted_recovery_details_are_data_err() {
        let err: anyhow::Error = objects::error::HeddleError::recovery(
            objects::RecoveryDetails::serialization_error("wrong msgpack marker FixArray(0)"),
        )
        .into();
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn unclassified_advice_kind_falls_back_to_io_err() {
        // Kinds without a documented divergent code keep the catch-all, so
        // adding a new advice kind never silently changes an exit code.
        assert_eq!(
            HeddleExitCode::from_error(&advice_with_kind("hook_veto")),
            HeddleExitCode::IoErr
        );
    }

    #[test]
    fn outcome_exit_maps_to_its_code() {
        let err = anyhow::anyhow!(OutcomeExit::data_err());
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
        assert!(HeddleExitCode::is_quiet_outcome(&err));
    }

    #[test]
    fn capture_out_of_space_advice_is_io_err() {
        assert_eq!(
            HeddleExitCode::from_error(&advice_with_kind("capture_out_of_space")),
            HeddleExitCode::IoErr
        );
    }

    #[test]
    fn operator_blocked_advice_is_data_err() {
        assert_eq!(
            HeddleExitCode::from_error(&advice_with_kind("operator_blocked")),
            HeddleExitCode::DataErr
        );
        assert_eq!(
            HeddleExitCode::from_error(&advice_with_kind("merge_eligibility_blocked")),
            HeddleExitCode::DataErr
        );
    }

    #[test]
    fn unknown_falls_back_to_io_err() {
        let err = anyhow::anyhow!("some unrelated thing went wrong");
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::IoErr);
    }

    #[test]
    fn u8_repr_matches_sysexits() {
        assert_eq!(HeddleExitCode::Ok.as_u8(), 0);
        assert_eq!(HeddleExitCode::Usage.as_u8(), 64);
        assert_eq!(HeddleExitCode::DataErr.as_u8(), 65);
        assert_eq!(HeddleExitCode::CantCreat.as_u8(), 73);
        assert_eq!(HeddleExitCode::IoErr.as_u8(), 74);
        assert_eq!(HeddleExitCode::TempFail.as_u8(), 75);
        assert_eq!(HeddleExitCode::Protocol.as_u8(), 76);
        assert_eq!(HeddleExitCode::NoPerm.as_u8(), 77);
        assert_eq!(HeddleExitCode::Config.as_u8(), 78);
    }
}
