use super::{
    AuthorizationSignature, AuthorizationVerificationKey, SignedOwnerKeyTransition, SignedOwnerRoot,
};

/// Principal category named by a capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum CapabilityPrincipalKind {
    Unspecified = 0,
    HumanDevice = 1,
    ServiceAccount = 2,
    Agent = 3,
    AnonymousKey = 4,
    AnyAnonymous = 5,
}

/// Literal spool action. No action implies another action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum SpoolCapabilityAction {
    Unspecified = 0,
    Read = 1,
    Write = 2,
    Merge = 3,
    Approve = 4,
    Admin = 5,
    Redact = 6,
    Grant = 7,
    Purge = 8,
}

/// Exact spool or complete-segment descendant selector.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SpoolSelector {
    #[prost(bytes = "vec", tag = "1")]
    pub root_spool_uuid: Vec<u8>,
    #[prost(string, repeated, tag = "2")]
    pub path_segments: Vec<String>,
    #[prost(bool, tag = "3")]
    pub include_descendants: bool,
}

/// Audited subject and optional subject verification key.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CapabilityPrincipal {
    #[prost(enumeration = "CapabilityPrincipalKind", tag = "1")]
    pub kind: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub principal_id: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub key: Option<AuthorizationVerificationKey>,
}

/// Actions granted for one selector.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SpoolCapabilityGrant {
    #[prost(message, optional, tag = "1")]
    pub spool: Option<SpoolSelector>,
    #[prost(enumeration = "SpoolCapabilityAction", repeated, tag = "2")]
    pub actions: Vec<i32>,
}

/// Owner-anchored capability body.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OwnerCapability {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub owner_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub issuer_state_hash: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub parent_capability_id: Vec<u8>,
    #[prost(message, optional, tag = "5")]
    pub subject: Option<CapabilityPrincipal>,
    #[prost(message, repeated, tag = "6")]
    pub grants: Vec<SpoolCapabilityGrant>,
    #[prost(int64, tag = "7")]
    pub not_before_unix_seconds: i64,
    #[prost(int64, tag = "8")]
    pub expires_at_unix_seconds: i64,
    #[prost(bytes = "vec", tag = "9")]
    pub nonce: Vec<u8>,
    #[prost(bytes = "vec", tag = "10")]
    pub capability_id: Vec<u8>,
}

/// Signed owner capability.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SignedOwnerCapability {
    #[prost(message, optional, tag = "1")]
    pub capability: Option<OwnerCapability>,
    #[prost(message, optional, tag = "2")]
    pub signature: Option<AuthorizationSignature>,
}

/// Portable owner-root, state, capability, and subject proof.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OwnerAuthorizationBundle {
    #[prost(message, optional, tag = "1")]
    pub owner_root: Option<SignedOwnerRoot>,
    #[prost(message, repeated, tag = "2")]
    pub owner_state_chain: Vec<SignedOwnerKeyTransition>,
    #[prost(message, repeated, tag = "3")]
    pub capability_chain: Vec<SignedOwnerCapability>,
    #[prost(bytes = "vec", tag = "4")]
    pub subject_biscuit: Vec<u8>,
}

/// Data-only submission request; no RPC consumes it before cutover.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SubmitOwnerAuthorizationRequest {
    #[prost(message, optional, tag = "1")]
    pub authorization: Option<OwnerAuthorizationBundle>,
    #[prost(string, tag = "2")]
    pub client_operation_id: String,
}

/// Unsigned submission receipt.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SubmitOwnerAuthorizationResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub capability_id: Vec<u8>,
    #[prost(int64, tag = "2")]
    pub expires_at_unix_seconds: i64,
}
