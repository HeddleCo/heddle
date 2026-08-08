// SPDX-License-Identifier: Apache-2.0
//! Fail-closed resolution of state signatures through a key-binding registry.

use crypto::{verify_payload_signature, verify_state_signature_bytes};
use objects::object::{KeyBinding, KeyBindingRegistry, State, StateAttachmentBody, StateSignature};

use crate::{Repository, Result};

/// Identity resolution result for a state's detached authorship signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorshipVerification {
    /// At least one active, authorized binding authenticated the state.
    Verified(String),
    /// The state was signed, but none of its signing keys occur in the registry.
    UnknownKey,
    /// A valid state signature resolved to a binding revoked at signing time.
    Revoked,
    /// The state, registry, binding authorization, or signature was invalid.
    Invalid,
}

impl Repository {
    /// Verify integrity and resolve a state author through an offline registry.
    ///
    /// Unknown keys fail closed even when their state signature is
    /// cryptographically valid. Binding windows are evaluated at the state's
    /// signed whole-second `created_at`, the only trusted signing-time value
    /// in the current state-signature envelope. A subsecond boundary that
    /// shares the signed second is treated conservatively.
    pub fn verify_authored_by_known_actor(
        &self,
        state: &State,
        registry: &KeyBindingRegistry,
    ) -> Result<AuthorshipVerification> {
        if !registry_is_authorized(registry) {
            return Ok(AuthorshipVerification::Invalid);
        }

        let signatures: Vec<_> = self
            .list_state_attachments(&state.id())?
            .into_iter()
            .filter_map(|attachment| match attachment.body {
                StateAttachmentBody::Signature(signature) => Some(signature),
                _ => None,
            })
            .collect();
        if signatures.is_empty() {
            return Ok(AuthorshipVerification::Invalid);
        }

        let mut verified_identity: Option<&str> = None;
        let mut saw_unknown = false;
        let mut saw_revoked = false;
        let mut saw_invalid = false;
        for signature in &signatures {
            let Some(binding) = find_binding(registry, signature) else {
                saw_unknown = true;
                continue;
            };
            if verify_state_signature_bytes(signature, &state.compute_hash()).is_err() {
                saw_invalid = true;
                continue;
            }
            let signed_at = state.created_at.timestamp();
            if signed_second_precedes(signed_at, binding.valid_from) {
                saw_invalid = true;
                continue;
            }
            if binding
                .revoked_at
                .is_some_and(|revoked_at| signed_second_may_be_revoked(signed_at, revoked_at))
            {
                saw_revoked = true;
                continue;
            }
            match verified_identity {
                Some(identity) if identity != binding.identity_ref => saw_invalid = true,
                Some(_) => {}
                None => verified_identity = Some(&binding.identity_ref),
            }
        }

        Ok(match verified_identity {
            Some(identity) if !saw_invalid => {
                AuthorshipVerification::Verified(identity.to_string())
            }
            Some(_) => AuthorshipVerification::Invalid,
            None if saw_revoked => AuthorshipVerification::Revoked,
            None if saw_invalid => AuthorshipVerification::Invalid,
            None if saw_unknown => AuthorshipVerification::UnknownKey,
            None => AuthorshipVerification::Invalid,
        })
    }
}

fn registry_is_authorized(registry: &KeyBindingRegistry) -> bool {
    registry.validate().is_ok()
        && registry
            .bindings
            .iter()
            .all(|binding| binding_is_authorized(binding, registry))
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

fn signed_second_precedes(signed_at: i64, valid_from: chrono::DateTime<chrono::Utc>) -> bool {
    signed_at < valid_from.timestamp()
        || (signed_at == valid_from.timestamp() && valid_from.timestamp_subsec_nanos() != 0)
}

fn signed_second_may_be_revoked(signed_at: i64, revoked_at: chrono::DateTime<chrono::Utc>) -> bool {
    signed_at >= revoked_at.timestamp()
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

fn find_binding<'a>(
    registry: &'a KeyBindingRegistry,
    signature: &StateSignature,
) -> Option<&'a KeyBinding> {
    registry.bindings.iter().find(|binding| {
        key_matches(
            &binding.algorithm,
            &binding.public_key,
            &signature.algorithm,
            &signature.public_key,
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
