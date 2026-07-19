// SPDX-License-Identifier: Apache-2.0
//! Surfacing for the hosted server's typed gRPC error vocabulary (AX H4).
//!
//! The hosted weft server attaches machine-readable error details to failing
//! gRPC responses as `google.rpc.Status` details (the standard
//! `grpc-status-details-bin` trailer) — see weft
//! `crates/weft-server/src/server/typed_error.rs`. The vocabulary is defined in
//! `api/proto/heddle/api/v1alpha1/errors.proto` and both sides consume it via
//! the `heddle-api` crate.
//!
//! This module decodes the three conflict/pagination/stream details the CLI
//! acts on — [`ConflictDetail`], [`CursorFailure`], [`StreamFailure`] — off a
//! [`tonic::Status`] and projects them into:
//!
//! - the JSON/text **error envelope** (`cli::commands::error_envelope`): a
//!   stable `kind`, a recovery `hint`, and structured `extra_json_fields` an
//!   agent can branch on (the conflicting resource, the `restart_cursor`,
//!   whether a stream is resumable), and
//! - the **exit-code taxonomy** (`exit`): a cursor/stream failure is a
//!   safe-retry (`TempFail` = 75, restart the pagination/stream); a conflict
//!   needs a changed input (a fresh op-id or a fetch+retry) so it stays a
//!   protocol-layer rejection (`Protocol` = 76).
//!
//! Decoding is dependency-light: rather than pull in `tonic-types`, we decode
//! the `google.rpc.Status` envelope with a local prost mirror ([`RpcStatus`])
//! and match the detail `Any` by its `type.googleapis.com/heddle.api.v1alpha1.*`
//! type URL.

use api::heddle::api::v1alpha1::{
    ConflictDetail, CursorFailure, StreamFailure, cursor_failure::Reason as CursorReason,
};

use crate::exit::HeddleExitCode;

/// Local prost mirror of `google.rpc.Status` (the shape tonic packs into the
/// `grpc-status-details-bin` trailer). We only need to reach `details`; the
/// duplicated `code`/`message` are ignored (the outer [`tonic::Status`] owns
/// the authoritative pair).
#[derive(Clone, PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: prost::alloc::string::String,
    #[prost(message, repeated, tag = "3")]
    details: prost::alloc::vec::Vec<prost_types::Any>,
}

fn heddle_type_url(message_name: &str) -> String {
    format!("type.googleapis.com/heddle.api.v1alpha1.{message_name}")
}

fn decode_first_detail<T: prost::Message + Default>(
    status: &tonic::Status,
    message_name: &str,
) -> Option<T> {
    // Fully-qualified so no `use prost::Message` import is needed (rustc
    // versions disagree on whether the bare `RpcStatus::decode` path requires
    // the trait in scope; the qualified form compiles cleanly on all of them).
    let rpc = <RpcStatus as prost::Message>::decode(status.details()).ok()?;
    let expected = heddle_type_url(message_name);
    rpc.details
        .into_iter()
        .find(|any| any.type_url == expected)
        .and_then(|any| <T as prost::Message>::decode(any.value.as_slice()).ok())
}

/// A hosted typed error the CLI knows how to render + classify. Built from a
/// [`tonic::Status`] via [`HostedTypedError::from_status`]; the first matching
/// detail wins (a status carries at most one of these in practice).
#[derive(Debug, Clone, PartialEq)]
pub enum HostedTypedError {
    /// An operation-id reuse or ref compare-and-set conflict.
    Conflict(ConflictDetail),
    /// An invalid/stale/expired pagination or stream cursor.
    Cursor(CursorFailure),
    /// A streaming RPC failed mid-flight.
    Stream(StreamFailure),
}

impl HostedTypedError {
    /// Decode the first conflict/cursor/stream detail carried on `status`, if
    /// any. Returns `None` for a plain status (string-only path unaffected).
    pub fn from_status(status: &tonic::Status) -> Option<Self> {
        if let Some(detail) = decode_first_detail::<ConflictDetail>(status, "ConflictDetail") {
            return Some(Self::Conflict(detail));
        }
        if let Some(detail) = decode_first_detail::<CursorFailure>(status, "CursorFailure") {
            return Some(Self::Cursor(detail));
        }
        if let Some(detail) = decode_first_detail::<StreamFailure>(status, "StreamFailure") {
            return Some(Self::Stream(detail));
        }
        None
    }

    /// Stable envelope `kind` discriminator (keyed on, never the message text).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Conflict(_) => "hosted_conflict",
            Self::Cursor(_) => "cursor_invalid",
            Self::Stream(_) => "stream_failure",
        }
    }

    /// Exit-code override for this typed failure, or `None` to fall through to
    /// the bare gRPC-code mapping.
    ///
    /// A cursor failure is a safe restart (`TempFail`); a conflict needs a
    /// changed input (`Protocol`). A `StreamFailure`, however, wraps EVERY
    /// mid-flight stream error — including a `PermissionDenied` / `NotFound` /
    /// `InvalidArgument` surfaced from inside the stream — so it is only a
    /// safe-retry when it is *genuinely* retryable (carries retry advice or a
    /// restart cursor, or its underlying gRPC code is transient). Otherwise we
    /// return `None` so the caller uses the bare code (PermissionDenied→NoPerm,
    /// NotFound→NoInput, InvalidArgument→Protocol) rather than telling an agent
    /// to hot-loop a permission/validation gate as "safe to retry".
    pub fn exit_code(&self) -> Option<HeddleExitCode> {
        match self {
            Self::Conflict(_) => Some(HeddleExitCode::Protocol),
            Self::Cursor(_) => Some(HeddleExitCode::TempFail),
            Self::Stream(detail) if stream_is_retryable(detail) => Some(HeddleExitCode::TempFail),
            Self::Stream(_) => None,
        }
    }

    /// Human-readable, one-line recovery hint.
    pub fn hint(&self) -> String {
        match self {
            Self::Conflict(detail) => {
                let resource = if detail.resource.is_empty() {
                    "the requested resource".to_string()
                } else {
                    format!("`{}`", detail.resource)
                };
                format!(
                    "The server rejected a conflict on {resource}. If you supplied --op-id, retry \
                     with a fresh operation id; for a ref conflict, fetch the latest state and retry."
                )
            }
            Self::Cursor(detail) => {
                if detail.restart_cursor.is_empty() {
                    "The pagination cursor is invalid or expired; restart the listing from the \
                     beginning (omit the cursor)."
                        .to_string()
                } else {
                    format!(
                        "The pagination cursor is invalid or expired; restart from the \
                         `restart_cursor` value (`{}`).",
                        detail.restart_cursor
                    )
                }
            }
            Self::Stream(detail) => {
                if let Some(cursor) = detail
                    .cursor
                    .as_ref()
                    .filter(|c| !c.restart_cursor.is_empty())
                {
                    format!(
                        "The stream failed mid-flight; restart it from the `restart_cursor` value \
                         (`{}`).",
                        cursor.restart_cursor
                    )
                } else if let Some(secs) = stream_retry_after_secs(detail) {
                    format!(
                        "The stream failed mid-flight but is resumable; retry after {secs}s."
                    )
                } else if stream_is_retryable(detail) {
                    "The stream failed mid-flight; restart it from the beginning.".to_string()
                } else {
                    "The stream failed mid-flight on a terminal error; do not blindly retry — \
                     inspect the underlying status and fix the cause first."
                        .to_string()
                }
            }
        }
    }

    /// Structured fields to merge into the JSON envelope so an agent can branch
    /// without parsing the hint prose.
    pub fn extra_json_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::Value;
        let mut fields = serde_json::Map::new();
        match self {
            Self::Conflict(detail) => {
                fields.insert("conflict_resource".into(), Value::String(detail.resource.clone()));
                if !detail.expected_version.is_empty() {
                    fields.insert(
                        "conflict_expected_version".into(),
                        Value::String(detail.expected_version.clone()),
                    );
                }
                if !detail.actual_version.is_empty() {
                    fields.insert(
                        "conflict_actual_version".into(),
                        Value::String(detail.actual_version.clone()),
                    );
                }
            }
            Self::Cursor(detail) => {
                fields.insert(
                    "cursor_reason".into(),
                    Value::String(cursor_reason_str(detail.reason).to_string()),
                );
                fields.insert(
                    "restart_cursor".into(),
                    Value::String(detail.restart_cursor.clone()),
                );
            }
            Self::Stream(detail) => {
                fields.insert("stream_grpc_code".into(), Value::Number(detail.grpc_code.into()));
                let restart_cursor = detail
                    .cursor
                    .as_ref()
                    .map(|c| c.restart_cursor.clone())
                    .filter(|c| !c.is_empty());
                let retry_secs = stream_retry_after_secs(detail);
                // "resumable" = the server told us how to continue (a resume
                // cursor or a retry delay); otherwise the client must restart.
                let resumable = restart_cursor.is_some() || retry_secs.is_some();
                fields.insert("stream_resumable".into(), Value::Bool(resumable));
                if let Some(cursor) = restart_cursor {
                    fields.insert("restart_cursor".into(), Value::String(cursor));
                }
                if let Some(secs) = retry_secs {
                    fields.insert("retry_after_secs".into(), Value::Number(secs.into()));
                }
            }
        }
        fields
    }
}

fn stream_retry_after_secs(detail: &StreamFailure) -> Option<i64> {
    detail
        .retry
        .as_ref()
        .and_then(|r| r.retry_after.as_ref())
        .map(|d| d.seconds)
        .filter(|secs| *secs > 0)
}

/// Is a `StreamFailure` genuinely safe to retry by restarting? True when it
/// carries retry advice or a restart cursor, or its underlying gRPC code is a
/// transient/retryable class. A stream that merely wraps a terminal
/// PermissionDenied / NotFound / InvalidArgument is NOT safe-retry.
fn stream_is_retryable(detail: &StreamFailure) -> bool {
    use tonic::Code;
    if detail.retry.is_some() {
        return true;
    }
    if detail
        .cursor
        .as_ref()
        .is_some_and(|c| !c.restart_cursor.is_empty())
    {
        return true;
    }
    matches!(
        Code::from_i32(detail.grpc_code),
        Code::Unavailable
            | Code::DeadlineExceeded
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::Internal
            | Code::Unknown
            | Code::Cancelled
    )
}

fn cursor_reason_str(reason: i32) -> &'static str {
    match CursorReason::try_from(reason) {
        Ok(CursorReason::Stale) => "stale",
        Ok(CursorReason::Expired) => "expired",
        _ => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::heddle::api::v1alpha1::RetryAdvice;
    use prost::Message as _;
    use tonic::{Code, Status};

    /// Build a `tonic::Status` carrying `detail` as a `google.rpc.Status`
    /// detail, exactly as the weft server's `typed_error` builders do.
    fn status_with_detail<T: prost::Message>(
        code: Code,
        message: &str,
        message_name: &str,
        detail: &T,
    ) -> Status {
        let any = prost_types::Any {
            type_url: heddle_type_url(message_name),
            value: detail.encode_to_vec(),
        };
        let rpc = RpcStatus {
            code: code as i32,
            message: message.to_string(),
            details: vec![any],
        };
        Status::with_details(code, message, rpc.encode_to_vec().into())
    }

    #[test]
    fn conflict_detail_decodes_and_classifies_as_protocol() {
        let status = status_with_detail(
            Code::AlreadyExists,
            "thread 'refs/threads/main' already exists",
            "ConflictDetail",
            &ConflictDetail {
                resource: "thread 'refs/threads/main' already exists".to_string(),
                expected_version: String::new(),
                actual_version: String::new(),
            },
        );
        let typed = HostedTypedError::from_status(&status).expect("conflict detail decodes");
        assert_eq!(typed.kind(), "hosted_conflict");
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::Protocol));
        assert!(typed.hint().contains("conflict"));
        let fields = typed.extra_json_fields();
        assert!(fields["conflict_resource"].as_str().unwrap().contains("refs/threads/main"));
    }

    #[test]
    fn cursor_failure_decodes_and_is_safe_retry() {
        let status = status_with_detail(
            Code::InvalidArgument,
            "invalid cursor",
            "CursorFailure",
            &CursorFailure {
                reason: CursorReason::Stale as i32,
                expired_at: None,
                restart_cursor: String::new(),
            },
        );
        let typed = HostedTypedError::from_status(&status).expect("cursor detail decodes");
        assert_eq!(typed.kind(), "cursor_invalid");
        // A cursor restart is safe-retry — 75, NOT the InvalidArgument→Protocol
        // default the bare status code would otherwise map to.
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::TempFail));
        let fields = typed.extra_json_fields();
        assert_eq!(fields["cursor_reason"], "stale");
        assert_eq!(fields["restart_cursor"], "");
    }

    #[test]
    fn stream_failure_resume_vs_restart() {
        // No retry, no cursor → restart.
        let restart = status_with_detail(
            Code::Unavailable,
            "pull stream aborted",
            "StreamFailure",
            &StreamFailure {
                grpc_code: Code::Unavailable as i32,
                message: "pull stream aborted".to_string(),
                retry: None,
                cursor: None,
            },
        );
        let typed = HostedTypedError::from_status(&restart).expect("stream detail decodes");
        assert_eq!(typed.kind(), "stream_failure");
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::TempFail));
        assert!(typed.hint().contains("restart"));
        assert_eq!(typed.extra_json_fields()["stream_resumable"], serde_json::Value::Bool(false));

        // Retry advice present → resumable after N seconds.
        let resumable = status_with_detail(
            Code::Unavailable,
            "pull stream aborted",
            "StreamFailure",
            &StreamFailure {
                grpc_code: Code::Unavailable as i32,
                message: "pull stream aborted".to_string(),
                retry: Some(RetryAdvice {
                    retry_after: Some(prost_types::Duration { seconds: 3, nanos: 0 }),
                }),
                cursor: None,
            },
        );
        let typed = HostedTypedError::from_status(&resumable).expect("stream detail decodes");
        assert!(typed.hint().contains("resumable"));
        let fields = typed.extra_json_fields();
        assert_eq!(fields["stream_resumable"], serde_json::Value::Bool(true));
        assert_eq!(fields["retry_after_secs"], serde_json::Value::Number(3.into()));
    }

    #[test]
    fn stream_failure_wrapping_terminal_error_is_not_safe_retry() {
        // A StreamFailure wraps EVERY mid-stream Err, including a terminal
        // PermissionDenied surfaced from inside the stream. It must NOT be
        // reclassified as safe-retry (75) — exit_code() returns None so the
        // caller falls through to the bare code (PermissionDenied→NoPerm), and
        // the hint must not tell an agent to restart a policy gate.
        let denied = status_with_detail(
            Code::PermissionDenied,
            "access denied",
            "StreamFailure",
            &StreamFailure {
                grpc_code: Code::PermissionDenied as i32,
                message: "access denied".to_string(),
                retry: None,
                cursor: None,
            },
        );
        let typed = HostedTypedError::from_status(&denied).expect("stream detail decodes");
        assert_eq!(typed.kind(), "stream_failure");
        assert_eq!(typed.exit_code(), None); // fall through to bare PermissionDenied→NoPerm
        assert!(!typed.hint().contains("restart"));
        assert!(typed.hint().contains("do not blindly retry"));
    }

    #[test]
    fn plain_status_has_no_typed_error() {
        let status = Status::failed_precondition("no details here");
        assert!(HostedTypedError::from_status(&status).is_none());
    }
}
