mod encode;
mod objects;

pub(crate) use objects::{
    anonymous_body, capability_body, capability_without_id, deferred_bootstrap_body,
    owner_root_body, owner_root_without_id, registration_body, transition_body,
};
use sha2::{Digest, Sha256};

pub(crate) const OWNER_ROOT_DOMAIN: &[u8] = b"heddle-owner-root-v1";
pub(crate) const OWNER_TRANSITION_DOMAIN: &[u8] = b"heddle-owner-key-transition-v1";
pub(crate) const OWNER_CAPABILITY_DOMAIN: &[u8] = b"heddle-owner-capability-v1";
pub(crate) const ANONYMOUS_ID_DOMAIN: &[u8] = b"heddle-anonymous-v1";
pub(crate) const ANONYMOUS_CREDENTIAL_DOMAIN: &[u8] = b"heddle-anonymous-key-credential-v1";
pub(crate) const ANONYMOUS_REGISTRATION_DOMAIN: &[u8] = b"heddle-anonymous-registration-v1";
pub(crate) const DEFERRED_BOOTSTRAP_DOMAIN: &[u8] = b"heddle-owner-deferred-bootstrap-v1";

pub(crate) fn digest(domain: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(body);
    hasher.finalize().into()
}

pub(crate) fn key_id(
    key: &crate::owner_authorization::wire::AuthorizationVerificationKey,
) -> [u8; 32] {
    let mut body = Vec::with_capacity(4 + key.public_key.len());
    body.extend_from_slice(&key.algorithm.to_be_bytes());
    body.extend_from_slice(&key.public_key);
    digest(b"heddle-key-v1", &body)
}

pub(crate) fn nonce() -> Vec<u8> {
    let bytes: [u8; 32] = rand::random();
    bytes.to_vec()
}
