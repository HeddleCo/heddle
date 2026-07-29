use super::{AuthorizationSignature, OwnerAuthorizationBundle, SignedOwnerRoot};

/// Passkey creation proof for a new human account.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NewPasskeyOwnerRootApproval {
    #[prost(bytes = "vec", tag = "1")]
    pub client_data_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub attestation_object: Vec<u8>,
}

/// Passkey assertion proof for an existing human account.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExistingPasskeyOwnerRootApproval {
    #[prost(bytes = "vec", tag = "1")]
    pub credential_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub client_data_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub authenticator_data: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub signature: Vec<u8>,
}

/// WebAuthn approval bound to the exact owner-root challenge.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WebAuthnOwnerRootApproval {
    #[prost(string, tag = "1")]
    pub challenge_id: String,
    #[prost(oneof = "web_authn_owner_root_approval::Proof", tags = "2, 3")]
    pub proof: Option<web_authn_owner_root_approval::Proof>,
}

/// WebAuthn proof variants.
pub mod web_authn_owner_root_approval {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Proof {
        #[prost(message, tag = "2")]
        NewPasskey(super::NewPasskeyOwnerRootApproval),
        #[prost(message, tag = "3")]
        ExistingPasskey(super::ExistingPasskeyOwnerRootApproval),
    }
}

/// Agent-authorized deferred-human bootstrap proof.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DeferredOwnerRootApproval {
    #[prost(message, optional, tag = "1")]
    pub provisioning_authority: Option<OwnerAuthorizationBundle>,
    #[prost(message, optional, tag = "2")]
    pub origin_key_request_signature: Option<AuthorizationSignature>,
}

/// Owner-root bootstrap request.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BootstrapOwnerRootRequest {
    #[prost(message, optional, tag = "1")]
    pub owner_root: Option<SignedOwnerRoot>,
    #[prost(oneof = "bootstrap_owner_root_request::Approval", tags = "2, 3")]
    pub approval: Option<bootstrap_owner_root_request::Approval>,
    #[prost(string, tag = "4")]
    pub client_operation_id: String,
}

/// Bootstrap approval variants.
pub mod bootstrap_owner_root_request {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    #[allow(clippy::large_enum_variant)]
    pub enum Approval {
        #[prost(message, tag = "2")]
        Human(super::WebAuthnOwnerRootApproval),
        #[prost(message, tag = "3")]
        DeferredHuman(super::DeferredOwnerRootApproval),
    }
}

/// Unsigned bootstrap acknowledgement.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BootstrapOwnerRootResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub owner_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub accepted_root_hash: Vec<u8>,
}
