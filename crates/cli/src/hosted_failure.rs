// SPDX-License-Identifier: Apache-2.0
//! CLI projection of transport-neutral hosted failure details.

use wire::{
    ProtocolError, RemoteCursorFailure, RemoteCursorReason, RemoteFailureCode, RemoteFailureDetail,
};

use crate::exit::HeddleExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedFailureDetail {
    Conflict {
        resource: String,
        expected_version: String,
        actual_version: String,
    },
    Cursor(RemoteCursorFailure),
    Stream {
        code: RemoteFailureCode,
        retry_after_secs: Option<i64>,
        cursor: Option<RemoteCursorFailure>,
    },
}

impl HostedFailureDetail {
    pub fn from_protocol_error(error: &ProtocolError) -> Option<Self> {
        let ProtocolError::RemoteFailure { details, .. } = error else {
            return None;
        };
        details.iter().find_map(|detail| match detail {
            RemoteFailureDetail::Conflict {
                resource,
                expected_version,
                actual_version,
            } => Some(Self::Conflict {
                resource: resource.clone(),
                expected_version: expected_version.clone(),
                actual_version: actual_version.clone(),
            }),
            RemoteFailureDetail::Cursor(cursor) => Some(Self::Cursor(cursor.clone())),
            RemoteFailureDetail::Stream {
                code,
                retry_after,
                cursor,
                ..
            } => Some(Self::Stream {
                code: *code,
                retry_after_secs: retry_after.map(|duration| duration.seconds),
                cursor: cursor.clone(),
            }),
            _ => None,
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "hosted_conflict",
            Self::Cursor(_) => "cursor_invalid",
            Self::Stream { .. } => "stream_failure",
        }
    }

    pub fn exit_code(&self) -> Option<HeddleExitCode> {
        match self {
            Self::Conflict { .. } => Some(HeddleExitCode::Protocol),
            Self::Cursor(_) => Some(HeddleExitCode::TempFail),
            Self::Stream {
                code,
                retry_after_secs,
                cursor,
            } if retry_after_secs.is_some()
                || cursor
                    .as_ref()
                    .is_some_and(|cursor| !cursor.restart_cursor.is_empty())
                || remote_code_is_retryable(*code) =>
            {
                Some(HeddleExitCode::TempFail)
            }
            Self::Stream { .. } => None,
        }
    }

    pub fn hint(&self) -> String {
        match self {
            Self::Conflict { resource, .. } => {
                let resource = if resource.is_empty() {
                    "the requested resource".to_string()
                } else {
                    format!("`{resource}`")
                };
                format!(
                    "The server rejected a conflict on {resource}. If you supplied --op-id, retry with a fresh operation id; for a ref conflict, fetch the latest state and retry."
                )
            }
            Self::Cursor(cursor) if cursor.restart_cursor.is_empty() => {
                "The pagination cursor is invalid or expired; restart the listing from the beginning (omit the cursor).".to_string()
            }
            Self::Cursor(cursor) => format!(
                "The pagination cursor is invalid or expired; restart from the `restart_cursor` value (`{}`).",
                cursor.restart_cursor
            ),
            Self::Stream {
                cursor: Some(cursor),
                ..
            } if !cursor.restart_cursor.is_empty() => format!(
                "The stream failed mid-flight; restart it from the `restart_cursor` value (`{}`).",
                cursor.restart_cursor
            ),
            Self::Stream {
                retry_after_secs: Some(seconds),
                ..
            } => format!(
                "The stream failed mid-flight but is resumable; retry after {seconds}s."
            ),
            Self::Stream { code, .. } if remote_code_is_retryable(*code) => {
                "The stream failed mid-flight; restart it from the beginning.".to_string()
            }
            Self::Stream { .. } => {
                "The stream failed mid-flight on a terminal error; do not blindly retry — inspect the underlying failure and fix the cause first.".to_string()
            }
        }
    }

    pub fn extra_json_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::Value;
        let mut fields = serde_json::Map::new();
        match self {
            Self::Conflict {
                resource,
                expected_version,
                actual_version,
            } => {
                fields.insert("conflict_resource".into(), Value::String(resource.clone()));
                if !expected_version.is_empty() {
                    fields.insert(
                        "conflict_expected_version".into(),
                        Value::String(expected_version.clone()),
                    );
                }
                if !actual_version.is_empty() {
                    fields.insert(
                        "conflict_actual_version".into(),
                        Value::String(actual_version.clone()),
                    );
                }
            }
            Self::Cursor(cursor) => {
                fields.insert(
                    "cursor_reason".into(),
                    Value::String(cursor_reason(cursor.reason).to_string()),
                );
                fields.insert(
                    "restart_cursor".into(),
                    Value::String(cursor.restart_cursor.clone()),
                );
            }
            Self::Stream {
                retry_after_secs,
                cursor,
                ..
            } => {
                let restart_cursor = cursor
                    .as_ref()
                    .map(|cursor| cursor.restart_cursor.clone())
                    .filter(|cursor| !cursor.is_empty());
                fields.insert(
                    "stream_resumable".into(),
                    Value::Bool(restart_cursor.is_some() || retry_after_secs.is_some()),
                );
                if let Some(cursor) = restart_cursor {
                    fields.insert("restart_cursor".into(), Value::String(cursor));
                }
                if let Some(seconds) = retry_after_secs {
                    fields.insert("retry_after_secs".into(), Value::Number((*seconds).into()));
                }
            }
        }
        fields
    }
}

pub fn exit_code_for_remote(code: RemoteFailureCode) -> HeddleExitCode {
    match code {
        RemoteFailureCode::Unavailable
        | RemoteFailureCode::DeadlineExceeded
        | RemoteFailureCode::ResourceExhausted
        | RemoteFailureCode::Aborted
        | RemoteFailureCode::Cancelled => HeddleExitCode::TempFail,
        RemoteFailureCode::InvalidArgument
        | RemoteFailureCode::FailedPrecondition
        | RemoteFailureCode::OutOfRange
        | RemoteFailureCode::AlreadyExists => HeddleExitCode::Protocol,
        RemoteFailureCode::PermissionDenied | RemoteFailureCode::Unauthenticated => {
            HeddleExitCode::NoPerm
        }
        RemoteFailureCode::NotFound => HeddleExitCode::Config,
        _ => HeddleExitCode::IoErr,
    }
}

fn remote_code_is_retryable(code: RemoteFailureCode) -> bool {
    matches!(
        code,
        RemoteFailureCode::Unavailable
            | RemoteFailureCode::DeadlineExceeded
            | RemoteFailureCode::ResourceExhausted
            | RemoteFailureCode::Aborted
            | RemoteFailureCode::Internal
            | RemoteFailureCode::Unknown
            | RemoteFailureCode::Cancelled
    )
}

fn cursor_reason(reason: RemoteCursorReason) -> &'static str {
    match reason {
        RemoteCursorReason::Stale => "stale",
        RemoteCursorReason::Expired => "expired",
        RemoteCursorReason::Unspecified => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use wire::{RemoteDuration, RemoteFailureDetail};

    use super::*;

    fn failure(code: RemoteFailureCode, detail: RemoteFailureDetail) -> ProtocolError {
        ProtocolError::RemoteFailure {
            code,
            message: "hosted call failed".to_string(),
            details: vec![detail],
        }
    }

    #[test]
    fn conflict_detail_preserves_machine_fields_and_protocol_exit() {
        let error = failure(
            RemoteFailureCode::AlreadyExists,
            RemoteFailureDetail::Conflict {
                resource: "refs/heads/main".to_string(),
                expected_version: "old".to_string(),
                actual_version: "new".to_string(),
            },
        );
        let typed = HostedFailureDetail::from_protocol_error(&error).expect("typed detail");
        assert_eq!(typed.kind(), "hosted_conflict");
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::Protocol));
        assert_eq!(
            typed.extra_json_fields()["conflict_resource"],
            "refs/heads/main"
        );
    }

    #[test]
    fn cursor_and_retryable_stream_are_safe_to_restart() {
        let cursor = RemoteCursorFailure {
            reason: RemoteCursorReason::Stale,
            expired_at: None,
            restart_cursor: "cursor-2".to_string(),
        };
        let cursor_error = failure(
            RemoteFailureCode::InvalidArgument,
            RemoteFailureDetail::Cursor(cursor.clone()),
        );
        let typed = HostedFailureDetail::from_protocol_error(&cursor_error).expect("cursor");
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::TempFail));
        assert_eq!(typed.extra_json_fields()["restart_cursor"], "cursor-2");

        let stream_error = failure(
            RemoteFailureCode::Unavailable,
            RemoteFailureDetail::Stream {
                code: RemoteFailureCode::Unavailable,
                message: "interrupted".to_string(),
                retry_after: Some(RemoteDuration {
                    seconds: 3,
                    nanos: 0,
                }),
                cursor: None,
            },
        );
        let typed = HostedFailureDetail::from_protocol_error(&stream_error).expect("stream");
        assert_eq!(typed.exit_code(), Some(HeddleExitCode::TempFail));
        assert_eq!(typed.extra_json_fields()["retry_after_secs"], 3);
    }

    #[test]
    fn terminal_stream_does_not_claim_safe_retry() {
        let error = failure(
            RemoteFailureCode::PermissionDenied,
            RemoteFailureDetail::Stream {
                code: RemoteFailureCode::PermissionDenied,
                message: "denied".to_string(),
                retry_after: None,
                cursor: None,
            },
        );
        let typed = HostedFailureDetail::from_protocol_error(&error).expect("stream");
        assert_eq!(typed.exit_code(), None);
        assert!(typed.hint().contains("do not blindly retry"));
    }
}
