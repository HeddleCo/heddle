use super::{AuthorizationSignature, AuthorizationVerificationKey};

/// Self-authenticated anonymous pseudonym.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AnonymousKeyCredential {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub anonymous_id: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub key: Option<AuthorizationVerificationKey>,
    #[prost(int64, tag = "4")]
    pub issued_at_unix_seconds: i64,
    #[prost(int64, tag = "5")]
    pub expires_at_unix_seconds: i64,
    #[prost(bytes = "vec", tag = "6")]
    pub nonce: Vec<u8>,
    #[prost(message, optional, tag = "7")]
    pub self_signature: Option<AuthorizationSignature>,
}

/// Anonymous-key registration request.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterAnonymousKeyRequest {
    #[prost(message, optional, tag = "1")]
    pub credential: Option<AnonymousKeyCredential>,
    #[prost(string, optional, tag = "2")]
    pub turnstile_token: Option<String>,
    #[prost(string, tag = "3")]
    pub prior_continuity_token: String,
    #[prost(message, optional, tag = "4")]
    pub continuity_proof: Option<AuthorizationSignature>,
    #[prost(string, tag = "5")]
    pub client_operation_id: String,
}

/// Unsigned continuity receipt carrying no authorization.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterAnonymousKeyResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub anonymous_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub continuity_token: String,
    #[prost(int64, tag = "3")]
    pub continuity_expires_at_unix_seconds: i64,
}
