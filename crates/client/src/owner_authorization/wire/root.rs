/// Supported owner-authorization public-key algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum AuthorizationKeyAlgorithm {
    Unspecified = 0,
    Ed25519 = 1,
}

/// Public verification key carried on the wire.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AuthorizationVerificationKey {
    #[prost(enumeration = "AuthorizationKeyAlgorithm", tag = "1")]
    pub algorithm: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub public_key: Vec<u8>,
}

/// Detached Ed25519 signature and stable signer identifier.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AuthorizationSignature {
    #[prost(bytes = "vec", tag = "1")]
    pub signer_key_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub signature: Vec<u8>,
}

/// Signed provenance of a dedicated recovery guardian.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum RecoveryGuardianKind {
    Unspecified = 0,
    Paper = 1,
    Social = 2,
    Weft = 3,
}

/// Recovery guardian public key plus its signed provenance.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RecoveryGuardian {
    #[prost(enumeration = "RecoveryGuardianKind", tag = "1")]
    pub kind: i32,
    #[prost(message, optional, tag = "2")]
    pub key: Option<AuthorizationVerificationKey>,
}

/// Precommitted threshold recovery policy.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RecoveryPolicy {
    #[prost(uint32, tag = "1")]
    pub threshold: u32,
    #[prost(message, repeated, tag = "2")]
    pub guardians: Vec<RecoveryGuardian>,
}

/// Stable owner-root body.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OwnerRoot {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub owner_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub account_uuid: Vec<u8>,
    #[prost(message, optional, tag = "4")]
    pub authority_key: Option<AuthorizationVerificationKey>,
    #[prost(message, optional, tag = "5")]
    pub recovery_policy: Option<RecoveryPolicy>,
    #[prost(bool, tag = "6")]
    pub claimable_deferred_human: bool,
    #[prost(bytes = "vec", tag = "7")]
    pub nonce: Vec<u8>,
    #[prost(int64, tag = "8")]
    pub claimable_until_unix_seconds: i64,
}

/// Owner root with proof of possession from every declared key.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SignedOwnerRoot {
    #[prost(message, optional, tag = "1")]
    pub root: Option<OwnerRoot>,
    #[prost(message, optional, tag = "2")]
    pub authority_proof: Option<AuthorizationSignature>,
    #[prost(message, repeated, tag = "3")]
    pub recovery_key_proofs: Vec<AuthorizationSignature>,
}
