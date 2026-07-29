use super::{AuthorizationSignature, AuthorizationVerificationKey, RecoveryPolicy};

/// Owner-state transition operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum OwnerKeyTransitionKind {
    Unspecified = 0,
    Rotate = 1,
    Recover = 2,
    RecoveryPolicy = 3,
    ClaimDeferredHuman = 4,
}

/// Canonical body for an owner-state transition.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OwnerKeyTransition {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub owner_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub previous_state_hash: Vec<u8>,
    #[prost(uint64, tag = "4")]
    pub sequence: u64,
    #[prost(enumeration = "OwnerKeyTransitionKind", tag = "5")]
    pub kind: i32,
    #[prost(message, optional, tag = "6")]
    pub next_authority_key: Option<AuthorizationVerificationKey>,
    #[prost(message, optional, tag = "7")]
    pub next_recovery_policy: Option<RecoveryPolicy>,
    #[prost(int64, tag = "8")]
    pub valid_from_unix_seconds: i64,
    #[prost(int64, tag = "9")]
    pub previous_key_valid_until_unix_seconds: i64,
    #[prost(bytes = "vec", tag = "10")]
    pub nonce: Vec<u8>,
}

/// Signed owner-state transition.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SignedOwnerKeyTransition {
    #[prost(message, optional, tag = "1")]
    pub transition: Option<OwnerKeyTransition>,
    #[prost(message, repeated, tag = "2")]
    pub authorizations: Vec<AuthorizationSignature>,
    #[prost(message, optional, tag = "3")]
    pub next_authority_key_proof: Option<AuthorizationSignature>,
    #[prost(message, repeated, tag = "4")]
    pub next_recovery_key_proofs: Vec<AuthorizationSignature>,
}
