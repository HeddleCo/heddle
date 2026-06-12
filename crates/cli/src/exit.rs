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

use std::io::ErrorKind as IoErrorKind;

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
            "remote_not_configured" => Some(Self::Config),
            // Well-formed input the command semantically rejects:
            // - nothing staged / no changes to capture
            // - a reconcile that needs a `--prefer` side
            // - unsaved worktree changes blocking a tree write
            // - repository state that fails msgpack/serde decoding
            // - `--output json`/`json-compact` against a command without
            //   that output contract (the invocation parses fine; the
            //   command rejects the requested projection)
            "nothing_to_commit"
            | "reconcile_direction_required"
            | "dirty_worktree"
            | "state_corrupted"
            | "conflict_not_found"
            | "json_unsupported"
            | "json_compact_unsupported" => Some(Self::DataErr),
            _ => None,
        }
    }

    /// Map an anyhow error chain to an exit code. Walks the chain and uses
    /// the first downcast match; falls back to `IoErr` so callers always
    /// get a code more informative than the bare `1` shell convention.
    pub fn from_error(err: &anyhow::Error) -> Self {
        for cause in err.chain() {
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
                    // A missing repository is a missing precondition
                    // (initialize/point at one), not an IO failure.
                    objects::error::HeddleError::RepositoryNotFound(_) => return Self::Config,
                    objects::error::HeddleError::RepositoryFormatTooNew { .. } => {
                        return Self::DataErr;
                    }
                    // Stored state that fails msgpack decoding is corrupted
                    // data, not a transient IO problem — same class as the
                    // serde_json/toml parse failures below.
                    objects::error::HeddleError::Serialization(_) => return Self::DataErr,
                    _ => {}
                }
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
            if let Some(status) = cause.downcast_ref::<tonic::Status>() {
                use tonic::Code;
                return match status.code() {
                    Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted => {
                        Self::TempFail
                    }
                    Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
                        Self::Protocol
                    }
                    Code::PermissionDenied | Code::Unauthenticated => Self::NoPerm,
                    Code::NotFound => Self::Config,
                    _ => Self::IoErr,
                };
            }
            if cause.is::<serde_json::Error>() || cause.is::<toml::de::Error>() {
                return Self::DataErr;
            }
        }

        // Legacy string sentinels — LAST RESORT for raw-string error paths
        // that carry no typed `RecoveryAdvice` or `HeddleError` (e.g. the
        // stringified `RemoteError::NotFound` display from `resolve_remote`,
        // or upstream messages flattened through `anyhow::Error::msg`).
        // Any error that has a typed kind MUST be classified above via
        // `for_advice_kind` / the `HeddleError` match — never add a sentinel
        // here for a message a typed constructor produces. Keep these short
        // and exact so they don't false-positive on unrelated messages.
        let msg = format!("{err:#}");
        if msg.contains("no upstream configured")
            || msg.contains("no remote configured")
            || msg.contains("no default remote configured")
            || msg.contains("workspace config invalid")
            || msg.contains("repository not found")
        {
            return Self::Config;
        }
        if msg.contains("dirty worktree") {
            return Self::DataErr;
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
    fn no_upstream_string_sentinel_is_config() {
        let err = anyhow::anyhow!("push refused: no upstream configured for branch 'main'");
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
    }

    #[test]
    fn no_default_remote_string_sentinel_is_config() {
        // `heddle pull` against a repo with no default remote surfaces the
        // raw `RemoteError::NotFound` display via `anyhow::Error::msg`, so it
        // is only matchable as a string. The persona-flagged divergence
        // (HeddleCo/heddle#252) was this returning the `IoErr` catch-all.
        let err = anyhow::anyhow!("remote not found: (no default remote configured)");
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
    fn nothing_to_commit_advice_is_data_err() {
        // `heddle commit` with nothing staged is semantic rejection of
        // well-formed input (DataErr), not an IO failure.
        let advice = crate::cli::commands::RecoveryAdvice::safety_refusal(
            "nothing_to_commit",
            "nothing to commit",
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
    fn reconcile_direction_required_advice_is_data_err() {
        // `heddle bridge git reconcile` without a `--prefer` side requires
        // manual resolution — the reconcile contract's documented DataErr.
        let advice = crate::cli::commands::RecoveryAdvice::safety_refusal(
            "reconcile_direction_required",
            "Refusing to reconcile 'main': choose a local side before applying",
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
    fn missing_repo_string_sentinel_is_config() {
        let err = anyhow::anyhow!("repository not found at /tmp/whatever");
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
            ("nothing_to_commit", HeddleExitCode::DataErr),
            ("reconcile_direction_required", HeddleExitCode::DataErr),
            ("dirty_worktree", HeddleExitCode::DataErr),
            ("state_corrupted", HeddleExitCode::DataErr),
            ("conflict_not_found", HeddleExitCode::DataErr),
            ("json_unsupported", HeddleExitCode::DataErr),
            ("json_compact_unsupported", HeddleExitCode::DataErr),
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
        // "dirty worktree" sentinel, so only the typed kind can classify it.
        let err = anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::dirty_worktree(
            "merge",
            vec!["src/lib.rs".to_string()],
            "repository state was left unchanged",
        ));
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::DataErr);
    }

    #[test]
    fn dirty_worktree_string_sentinel_is_data_err() {
        // Raw-string path (e.g. repository_worktree_apply's refusal) that
        // carries no typed advice still classifies via the legacy sentinel.
        let err = anyhow::anyhow!(
            "dirty worktree would be overwritten by full rematerialize (switch)"
        );
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

        let compact = anyhow::anyhow!(
            crate::cli::commands::RecoveryAdvice::json_compact_unsupported("log")
        );
        assert_eq!(
            HeddleExitCode::from_error(&compact),
            HeddleExitCode::DataErr
        );
    }

    #[test]
    fn state_corrupted_advice_is_data_err() {
        let err = anyhow::anyhow!(crate::cli::commands::RecoveryAdvice::serialization_error(
            "wrong msgpack marker FixArray(0)"
        ));
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
