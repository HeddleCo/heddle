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

    /// Map an anyhow error chain to an exit code. Walks the chain and uses
    /// the first downcast match; falls back to `IoErr` so callers always
    /// get a code more informative than the bare `1` shell convention.
    pub fn from_error(err: &anyhow::Error) -> Self {
        for cause in err.chain() {
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

        // Heddle-specific string sentinels — match the user-visible phrasing
        // produced by RecoveryAdvice. Cheap to scan; only runs on the error
        // path. Keep these short and exact so they don't false-positive on
        // unrelated messages that happen to mention "no upstream".
        let msg = format!("{err:#}");
        if msg.contains("no upstream configured")
            || msg.contains("no remote configured")
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
    fn missing_repo_string_sentinel_is_config() {
        let err = anyhow::anyhow!("repository not found at /tmp/whatever");
        assert_eq!(HeddleExitCode::from_error(&err), HeddleExitCode::Config);
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
