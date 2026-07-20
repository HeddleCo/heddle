// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub code: StatusCode,
    pub message: String,
    pub progress: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusCode {
    Progress,
    Success,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    General,
    InvalidArgument,
    NotFound,
    PermissionDenied,
    Network,
    Protocol,
    Server,
}

/// Stable outcome vocabulary for a failed hosted call. This mirrors the
/// governed API contract without depending on generated protobuf types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteFailureCode {
    Unspecified,
    Cancelled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDuration {
    pub seconds: i64,
    pub nanos: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTimestamp {
    pub seconds: i64,
    pub nanos: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteCursorReason {
    Unspecified,
    Stale,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCursorFailure {
    pub reason: RemoteCursorReason,
    pub expired_at: Option<RemoteTimestamp>,
    pub restart_cursor: String,
}

/// Machine-readable hosted failure details retained across the transport seam.
/// Unknown details stay lossless so a newer server does not collapse them to
/// prose when talking to an older client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteFailureDetail {
    Retry {
        retry_after: Option<RemoteDuration>,
    },
    Conflict {
        resource: String,
        expected_version: String,
        actual_version: String,
    },
    Cursor(RemoteCursorFailure),
    CapabilityRequirement {
        capabilities: Vec<String>,
    },
    PolicyDenial {
        policy_id: String,
        rule: String,
        human_verification_can_override: bool,
    },
    Stream {
        code: RemoteFailureCode,
        message: String,
        retry_after: Option<RemoteDuration>,
        cursor: Option<RemoteCursorFailure>,
    },
    Unknown {
        type_url: String,
        value: Vec<u8>,
    },
}
