// SPDX-License-Identifier: Apache-2.0
//! Fail-closed resolution of state signatures through a key-binding registry.

use crypto::{verify_payload_signature, verify_state_signature_bytes};
use objects::object::{
    KeyBinding, KeyBindingRegistry, KeyRole, State, StateAttachmentBody, StateSignature,
};

use crate::{HeddleError, KeyBindingRegistryAnchor, RepoConfig, Repository, Result, TrustedKey};

/// Identity resolution result for a state's detached authorship signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorshipVerification {
    /// At least one active, authorized binding authenticated the state.
    Verified(String),
    /// The state was signed, but none of its signing keys occur in the registry.
    UnknownKey,
    /// A valid signature resolved to a binding revoked in the anchored registry.
    Revoked,
    /// The key is registered, but its signed role does not authorize this action.
    UnauthorizedRole { required: KeyRole, actual: KeyRole },
    /// The state, registry, binding authorization, or signature was invalid.
    Invalid,
}

impl Repository {
    /// Load the verifier-local registry anchor from repository configuration.
    ///
    /// This deliberately reloads the file so a long-lived repository handle
    /// never verifies against a stale trust decision.
    pub fn key_binding_registry_anchor(&self) -> Result<Option<KeyBindingRegistryAnchor>> {
        Ok(
            RepoConfig::load_for_repository(&self.heddle_dir().join("config.toml"))?
                .provenance
                .key_binding_registry,
        )
    }

    /// Verify one registry checkpoint against an out-of-band trusted authority.
    pub fn verify_key_binding_registry_checkpoint(
        &self,
        registry: &KeyBindingRegistry,
        trusted_authority: &TrustedKey,
    ) -> Result<()> {
        if registry_checkpoint_is_authorized(registry, trusted_authority) {
            Ok(())
        } else {
            Err(HeddleError::InvalidObject(
                "key-binding registry checkpoint is not signed by the trusted authority"
                    .to_string(),
            ))
        }
    }

    /// Resolve a signing key through the current authenticated registry.
    ///
    /// Signer-asserted object timestamps are not trusted for revocation. A key
    /// revoked in the anchored registry is rejected even when the signed object
    /// claims to predate revocation. The verifier's clock only bounds a
    /// future-dated authority-issued binding. Payload verification remains the
    /// caller's domain-specific responsibility.
    pub fn verify_known_actor_key(
        &self,
        algorithm: &str,
        public_key: &str,
        required_role: KeyRole,
        registry: &KeyBindingRegistry,
        trusted_authority: &TrustedKey,
    ) -> AuthorshipVerification {
        if !registry_is_authorized(registry, trusted_authority) {
            return AuthorshipVerification::Invalid;
        }
        let Some(binding) = find_binding_by_key(registry, algorithm, public_key) else {
            return AuthorshipVerification::UnknownKey;
        };
        if binding.valid_from > chrono::Utc::now() {
            return AuthorshipVerification::Invalid;
        }
        if binding.revoked_at.is_some() {
            return AuthorshipVerification::Revoked;
        }
        if binding.role != required_role {
            return AuthorshipVerification::UnauthorizedRole {
                required: required_role,
                actual: binding.role,
            };
        }
        AuthorshipVerification::Verified(binding.identity_ref.clone())
    }

    /// Verify integrity and resolve a state author through an offline registry.
    ///
    /// Unknown keys fail closed even when their state signature is
    /// cryptographically valid. The signer-controlled state timestamp is never
    /// used to move a signature behind a revocation boundary.
    pub fn verify_authored_by_known_actor(
        &self,
        state: &State,
        registry: &KeyBindingRegistry,
        trusted_authority: &TrustedKey,
    ) -> Result<AuthorshipVerification> {
        let stored_id = if state.accepts_stored_id(&state.state_id) {
            state.state_id
        } else {
            state.id()
        };
        let hash = state.hash_for_stored_id(&stored_id);
        let signatures: Vec<_> = self
            .list_state_attachments(&stored_id)?
            .into_iter()
            .filter_map(|attachment| match attachment.body {
                StateAttachmentBody::Signature(signature) => Some(signature),
                _ => None,
            })
            .collect();
        if signatures.is_empty() {
            return Ok(AuthorshipVerification::Invalid);
        }

        let mut verified_identity: Option<String> = None;
        let mut saw_unknown = false;
        let mut saw_revoked = false;
        let mut unauthorized_role = None;
        let mut saw_invalid = false;
        for signature in &signatures {
            if verify_state_signature_bytes(signature, &hash).is_err() {
                saw_invalid = true;
                continue;
            }
            match self.verify_known_actor_key(
                &signature.algorithm,
                &signature.public_key,
                KeyRole::Author,
                registry,
                trusted_authority,
            ) {
                AuthorshipVerification::Verified(identity) => match &verified_identity {
                    Some(prior) if *prior != identity => saw_invalid = true,
                    Some(_) => {}
                    None => verified_identity = Some(identity),
                },
                AuthorshipVerification::UnknownKey => saw_unknown = true,
                AuthorshipVerification::Revoked => saw_revoked = true,
                AuthorshipVerification::UnauthorizedRole { actual, .. } => {
                    unauthorized_role.get_or_insert(actual);
                }
                AuthorshipVerification::Invalid => saw_invalid = true,
            }
        }

        Ok(match verified_identity {
            Some(identity) if !saw_invalid => AuthorshipVerification::Verified(identity),
            Some(_) => AuthorshipVerification::Invalid,
            None if saw_revoked => AuthorshipVerification::Revoked,
            None if unauthorized_role.is_some() => AuthorshipVerification::UnauthorizedRole {
                required: KeyRole::Author,
                actual: unauthorized_role.expect("checked above"),
            },
            None if saw_invalid => AuthorshipVerification::Invalid,
            None if saw_unknown => AuthorshipVerification::UnknownKey,
            None => AuthorshipVerification::Invalid,
        })
    }
}

fn registry_is_authorized(registry: &KeyBindingRegistry, trusted_authority: &TrustedKey) -> bool {
    registry_checkpoint_is_authorized(registry, trusted_authority)
        && registry
            .bindings
            .iter()
            .all(|binding| binding_is_authorized(binding, registry))
}

fn registry_checkpoint_is_authorized(
    registry: &KeyBindingRegistry,
    trusted_authority: &TrustedKey,
) -> bool {
    let signature = &registry.authority_signature;
    registry.validate().is_ok()
        && key_matches(
            &signature.algorithm,
            &signature.public_key,
            &trusted_authority.algorithm,
            &trusted_authority.public_key,
        )
        && registry
            .canonical_checkpoint_signing_payload()
            .is_ok_and(|payload| signature_verifies(&payload, signature))
}

fn binding_is_authorized(binding: &KeyBinding, registry: &KeyBindingRegistry) -> bool {
    let added_by = &binding.added_by_sig;
    let is_self_signed = key_matches(
        &binding.algorithm,
        &binding.public_key,
        &added_by.algorithm,
        &added_by.public_key,
    );
    if is_self_signed {
        if binding.delegated_from.is_some() {
            return false;
        }
    } else {
        let Some(delegated_from) = binding.delegated_from else {
            return false;
        };
        let Some(root) = registry.bindings.iter().find(|candidate| {
            candidate.content_hash().ok() == Some(delegated_from)
                && key_matches(
                    &candidate.algorithm,
                    &candidate.public_key,
                    &added_by.algorithm,
                    &added_by.public_key,
                )
        }) else {
            return false;
        };
        if root.delegated_from.is_some()
            || root.identity_ref != binding.identity_ref
            || !binding_active_at(root, binding.valid_from)
            || !binding_self_signature_verifies(root)
        {
            return false;
        }
    }
    signature_verifies(&binding.canonical_signing_payload(), added_by)
}

fn binding_self_signature_verifies(binding: &KeyBinding) -> bool {
    key_matches(
        &binding.algorithm,
        &binding.public_key,
        &binding.added_by_sig.algorithm,
        &binding.added_by_sig.public_key,
    ) && signature_verifies(&binding.canonical_signing_payload(), &binding.added_by_sig)
}

fn binding_active_at(binding: &KeyBinding, at: chrono::DateTime<chrono::Utc>) -> bool {
    binding.valid_from <= at && binding.revoked_at.is_none_or(|revoked| at < revoked)
}

fn signature_verifies(payload: &[u8], signature: &StateSignature) -> bool {
    let Ok(public_key) = hex::decode(&signature.public_key) else {
        return false;
    };
    let Ok(signature_bytes) = hex::decode(&signature.signature) else {
        return false;
    };
    verify_payload_signature(payload, &signature.algorithm, &public_key, &signature_bytes).is_ok()
}

fn find_binding_by_key<'a>(
    registry: &'a KeyBindingRegistry,
    algorithm: &str,
    public_key: &str,
) -> Option<&'a KeyBinding> {
    registry.bindings.iter().find(|binding| {
        key_matches(
            &binding.algorithm,
            &binding.public_key,
            algorithm,
            public_key,
        )
    })
}

fn key_matches(
    left_algorithm: &str,
    left_public_key: &str,
    right_algorithm: &str,
    right_public_key: &str,
) -> bool {
    left_algorithm.eq_ignore_ascii_case(right_algorithm)
        && left_public_key.eq_ignore_ascii_case(right_public_key)
}

#[cfg(test)]
#[path = "repository_key_binding_tests.rs"]
mod tests;
