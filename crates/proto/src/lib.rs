// SPDX-License-Identifier: Apache-2.0
//! Shared protocol/auth transport types.

mod auth_context;
#[cfg(test)]
mod auth_tests;
mod auth_token;
mod capabilities;
mod message_auth;
mod message_delta;
mod message_hosted;
mod message_objects;
mod message_pushpull;
mod message_refs;
mod message_status;
mod native_pack;
mod object_availability;
mod object_graph;
mod object_transfer;

pub use auth_context::AuthContext;
pub use auth_token::{AuthToken, TokenScope};
pub use capabilities::{
    CAPABILITY_CHUNKED_TRANSFER, CAPABILITY_PACK_TRANSFER, CAPABILITY_PARTIAL_FETCH,
    CAPABILITY_RESUMABLE_TRANSFER, Capabilities, CapabilitySet,
};
pub use message_auth::{AuthMethod, Permission};
pub use message_delta::{DeltaData, RequestDelta};
pub use message_hosted::{
    CreateHostedGrant, CreateHostedRepository, CreateNamespace, DeleteHostedGrant,
    DeleteHostedRepository, DeleteNamespace, HarnessIdentity, HostedGrantCreated,
    HostedGrantDeleted, HostedGrantInfo, HostedGrantUpdated, HostedGrantsList, HostedNamespaceInfo,
    HostedRepositoryInfo, ListHostedGrants, ListHostedNamespaces, ListHostedRepositories,
    NamespaceCreated, NamespaceDeleted, NamespaceUpdated, NamespacesList, ProgressCheckpoint,
    RepositoriesList, RepositoryCreated, RepositoryDeleted, RepositoryUpdated, SessionDiffSummary,
    SessionReportEnvelope, TranscriptAttachmentRef, UpdateHostedGrant, UpdateHostedRepository,
    UpdateNamespace, UsageTotals, WorktreeChangeBaseline,
};
pub use message_objects::{HaveObjects, ObjectData, ObjectRequest, SendObjects, WantObjects};
pub use message_pushpull::{
    PARTIAL_FETCH_DISABLED, PARTIAL_FETCH_ENABLED, PARTIAL_FETCH_REQUIRED, PullComplete, PullReady,
    PullRequest, PushComplete, PushReady, PushRequest, TRANSPORT_MODE_NATIVE_PACK,
};
pub use message_refs::{HeadInfo, ListRefs, RefEntry, RefFilter, RefUpdated, RefsList, UpdateRef};
pub use message_status::{Error, ErrorCode, Status, StatusCode};
pub use native_pack::{
    NativePackBundle, PackChunkState, build_native_pack, install_received_pack,
    is_native_packable_object_type, native_pack_excluded_object_types, next_pack_chunk,
    receive_pack_chunk,
};
pub use object_availability::{ObjectAvailabilityPlan, has_object, plan_object_availability};
pub use object_graph::{
    ObjectId, ObjectInfo, ObjectType, PlannedObject, StateClosureOptions, enumerate_state_closure,
    enumerate_state_closure_plan, enumerate_state_closure_plan_with_options,
    enumerate_state_closure_with_options, is_ancestor,
};
pub use object_transfer::{
    chunk_bounds, chunk_count, chunk_offset, load_object_data, load_requested_object,
    store_received_object,
};

/// Default port for Heddle protocol.
pub const DEFAULT_PORT: u16 = 8421;

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum message size (64 MB).
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Error type for protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("message too large: {size} bytes (max {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("invalid message type: {0}")]
    InvalidMessageType(u8),

    #[error("protocol version mismatch: server={server}, client={client}")]
    VersionMismatch { server: u32, client: u32 },

    #[error("capability not supported: {0}")]
    CapabilityNotSupported(String),

    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("authorization failed: {0}")]
    AuthorizationFailed(String),

    #[error("object not found: {0}")]
    ObjectNotFound(String),

    #[error("invalid state: {0}")]
    InvalidState(String),

    #[error("remote error: {0}")]
    Remote(String),

    #[error("remote failure ({code:?}): {message}")]
    RemoteFailure {
        code: ErrorCode,
        message: String,
        details: Option<String>,
    },

    #[error("lock error: {0}")]
    LockError(String),
}

impl From<rmp_serde::encode::Error> for ProtocolError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        ProtocolError::Serialization(e.to_string())
    }
}

impl From<rmp_serde::decode::Error> for ProtocolError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        ProtocolError::Serialization(e.to_string())
    }
}

impl From<objects::error::HeddleError> for ProtocolError {
    fn from(e: objects::error::HeddleError) -> Self {
        ProtocolError::Remote(e.to_string())
    }
}

impl ProtocolError {
    pub fn client_message(&self) -> String {
        match self {
            ProtocolError::Io(_) => "network error".to_string(),
            ProtocolError::Serialization(_) => "protocol error".to_string(),
            ProtocolError::MessageTooLarge { .. } => "message too large".to_string(),
            ProtocolError::InvalidMessageType(_) => "protocol error".to_string(),
            ProtocolError::VersionMismatch { .. } => "protocol version mismatch".to_string(),
            ProtocolError::CapabilityNotSupported(_) => "capability not supported".to_string(),
            ProtocolError::AuthenticationFailed(_) => "permission denied".to_string(),
            ProtocolError::AuthorizationFailed(_) => "permission denied".to_string(),
            ProtocolError::ObjectNotFound(_) => "object not found".to_string(),
            ProtocolError::InvalidState(_) => "invalid request state".to_string(),
            ProtocolError::Remote(_) => "internal server error".to_string(),
            ProtocolError::RemoteFailure { message, .. } => message.clone(),
            ProtocolError::LockError(_) => "internal server error".to_string(),
        }
    }

    pub fn error_code(&self) -> ErrorCode {
        match self {
            ProtocolError::Io(_) => ErrorCode::Network,
            ProtocolError::Serialization(_) => ErrorCode::Protocol,
            ProtocolError::MessageTooLarge { .. } => ErrorCode::Protocol,
            ProtocolError::InvalidMessageType(_) => ErrorCode::Protocol,
            ProtocolError::VersionMismatch { .. } => ErrorCode::Protocol,
            ProtocolError::CapabilityNotSupported(_) => ErrorCode::Protocol,
            ProtocolError::AuthenticationFailed(_) => ErrorCode::PermissionDenied,
            ProtocolError::AuthorizationFailed(_) => ErrorCode::PermissionDenied,
            ProtocolError::ObjectNotFound(_) => ErrorCode::NotFound,
            ProtocolError::InvalidState(_) => ErrorCode::InvalidArgument,
            ProtocolError::Remote(_) => ErrorCode::Server,
            ProtocolError::RemoteFailure { code, .. } => *code,
            ProtocolError::LockError(_) => ErrorCode::Server,
        }
    }

    pub fn to_wire_error(&self, details: Option<String>) -> Error {
        Error {
            code: self.error_code(),
            message: self.client_message(),
            details,
        }
    }
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
