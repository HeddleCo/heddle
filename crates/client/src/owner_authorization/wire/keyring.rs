use super::{SignedOwnerCapability, SignedOwnerKeyTransition, SignedOwnerRoot};

/// Authentic local origin of an owner fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum CloneOwnerPinKind {
    Unspecified = 0,
    LocalCreation = 1,
    InvitationFingerprint = 2,
}

/// Client-local owner fingerprint pin.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloneOwnerPin {
    #[prost(enumeration = "CloneOwnerPinKind", tag = "1")]
    pub kind: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub expected_owner_id: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub first_seen_unix_seconds: i64,
}

/// Public bytes persisted by a clone for later offline verification.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloneAuthorizationKeyring {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub spool_uuid: Vec<u8>,
    #[prost(string, repeated, tag = "3")]
    pub canonical_spool_path_segments: Vec<String>,
    #[prost(message, optional, tag = "4")]
    pub pin: Option<CloneOwnerPin>,
    #[prost(message, optional, tag = "5")]
    pub owner_root: Option<SignedOwnerRoot>,
    #[prost(message, repeated, tag = "6")]
    pub accepted_transitions: Vec<SignedOwnerKeyTransition>,
    #[prost(bytes = "vec", tag = "7")]
    pub accepted_state_hash: Vec<u8>,
    #[prost(message, repeated, tag = "8")]
    pub public_access_capabilities: Vec<SignedOwnerCapability>,
}
