// SPDX-License-Identifier: Apache-2.0
//! Offline verification of the state authorship and review-signature chain.

mod content;
mod result;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use crypto::verify_payload_signature;
use objects::{
    error::Result,
    object::{
        ContentHash, KeyBindingRegistry, ReviewSignature, ReviewSignaturesBlob, SignatureStatus,
        State, StateAttachmentBody, StateAttachmentKind, StateId, signing_payload,
    },
    store::ObjectStore,
};
use repo::{AuthorshipVerification, Repository};
use schemars::JsonSchema;
use serde::Serialize;

use self::{
    content::verify_tree_content,
    result::{discover_registry, failed, identity_error, integrity_only, legacy},
};

const MAX_REGISTRY_BLOB_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ProvenanceReport {
    pub clean: bool,
    pub registry_status: String,
    pub registry_hash: Option<String>,
    pub states: Vec<StateProvenanceVerification>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct StateProvenanceVerification {
    pub state_id: String,
    pub status: String,
    pub identity: Option<String>,
    pub failed_link: Option<String>,
    pub detail: String,
    pub reviews_verified: usize,
    pub reviewer_identities: Vec<String>,
}

impl StateProvenanceVerification {
    pub fn display_status(&self) -> String {
        self.status.clone()
    }

    pub fn is_clean(&self) -> bool {
        self.status == "IntegrityOnly" || self.status.starts_with("Verified(")
    }
}

enum RegistryDiscovery {
    Absent,
    Available {
        registry: KeyBindingRegistry,
        hash: ContentHash,
    },
    Invalid(String),
}

pub fn verify_repository_provenance(repo: &Repository) -> Result<ProvenanceReport> {
    let registry = discover_registry(repo)?;
    let (registry_status, registry_hash) = match &registry {
        RegistryDiscovery::Absent => ("absent", None),
        RegistryDiscovery::Available { hash, .. } => ("available", Some(hash.to_string())),
        RegistryDiscovery::Invalid(_) => ("invalid", None),
    };
    let mut state_ids = repo.store().list_states()?;
    state_ids.sort();
    let mut states = Vec::with_capacity(state_ids.len());
    for state_id in state_ids {
        match repo.store().get_state(&state_id) {
            Ok(Some(state)) => states.push(verify_state(repo, &state, &registry)?),
            Ok(None) => states.push(failed(state_id, "content", "state object is missing")),
            Err(error) => states.push(failed(
                state_id,
                "content",
                &format!("state object failed content binding: {error}"),
            )),
        }
    }
    Ok(ProvenanceReport {
        clean: states.iter().all(StateProvenanceVerification::is_clean),
        registry_status: registry_status.to_string(),
        registry_hash,
        states,
    })
}

fn verify_state(
    repo: &Repository,
    state: &State,
    registry: &RegistryDiscovery,
) -> Result<StateProvenanceVerification> {
    let attachments = repo.list_state_attachments(&state.state_id)?;
    let signature_attributions: Vec<_> = attachments
        .iter()
        .filter_map(|attachment| match &attachment.body {
            StateAttachmentBody::Signature(_) => Some(&attachment.attribution),
            _ => None,
        })
        .collect();
    let signature_attribution_matches = signature_attributions
        .iter()
        .all(|attribution| **attribution == state.attribution);
    if state.id() != state.state_id {
        return Ok(failed(
            state.state_id,
            "content",
            "state content hash does not match its stored state id",
        ));
    }
    if let Some(detail) = verify_tree_content(repo, state.tree)? {
        return Ok(failed(state.state_id, "content", &detail));
    }

    let verification = match repo.verify_state_signature(&state.state_id)? {
        SignatureStatus::Unsigned => match registry {
            RegistryDiscovery::Available { registry, .. } => {
                verify_reviews(repo, state, Some(registry), None)
            }
            RegistryDiscovery::Absent => verify_reviews(repo, state, None, None),
            RegistryDiscovery::Invalid(detail) => {
                if repo
                    .latest_state_attachment(
                        &state.state_id,
                        StateAttachmentKind::ReviewSignatures,
                    )?
                    .is_some()
                {
                    failed(state.state_id, "review", detail)
                } else {
                    integrity_only(state.state_id, "state has no authorship signature")
                }
            }
        },
        SignatureStatus::Legacy => legacy(state.state_id),
        SignatureStatus::Invalid => failed(
            state.state_id,
            "content",
            "domain-tagged authorship signature did not verify",
        ),
        SignatureStatus::Valid => {
            if !signature_attribution_matches {
                failed(
                    state.state_id,
                    "identity",
                    "signed state attribution does not match its signature evidence",
                )
            } else {
                match registry {
                    RegistryDiscovery::Absent => verify_reviews(repo, state, None, None),
                    RegistryDiscovery::Invalid(detail) => {
                        failed(state.state_id, "identity", detail)
                    }
                    RegistryDiscovery::Available { registry, .. } => {
                        match repo.verify_authored_by_known_actor(state, registry)? {
                            AuthorshipVerification::Verified(identity) => {
                                verify_reviews(repo, state, Some(registry), Some(identity))
                            }
                            other => failed(
                                state.state_id,
                                "identity",
                                &format!(
                                    "authorship key resolution returned {}",
                                    identity_error(&other)
                                ),
                            ),
                        }
                    }
                }
            }
        }
    };
    Ok(verification)
}

fn verify_reviews(
    repo: &Repository,
    state: &State,
    registry: Option<&KeyBindingRegistry>,
    author_identity: Option<String>,
) -> StateProvenanceVerification {
    match verify_review_chain(repo, state, registry) {
        Ok((count, reviewer_identities)) => match author_identity {
            Some(identity) => StateProvenanceVerification {
                state_id: state.state_id.to_string_full(),
                status: format!("Verified({identity})"),
                identity: Some(identity),
                failed_link: None,
                detail: format!("authorship and {count} review signature(s) verified offline"),
                reviews_verified: count,
                reviewer_identities,
            },
            None => {
                let mut result = integrity_only(
                    state.state_id,
                    if registry.is_some() && count > 0 {
                        "state is unsigned; review-signature identities verified offline"
                    } else if registry.is_some() {
                        "state has no authorship signature"
                    } else {
                        "content and signatures verify, but no key-binding registry is present"
                    },
                );
                result.reviews_verified = count;
                result.reviewer_identities = reviewer_identities;
                result
            }
        },
        Err(detail) => failed(state.state_id, "review", &detail),
    }
}

fn verify_review_chain(
    repo: &Repository,
    state: &State,
    registry: Option<&KeyBindingRegistry>,
) -> std::result::Result<(usize, Vec<String>), String> {
    let attachment = repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::ReviewSignatures)
        .map_err(|error| error.to_string())?;
    let Some(attachment) = attachment else {
        return Ok((0, Vec::new()));
    };
    let StateAttachmentBody::ReviewSignatures(hash) = attachment.body else {
        return Err("review attachment has the wrong body kind".to_string());
    };
    let blob = repo
        .store()
        .get_blob(&hash)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("review-signatures blob {} is missing", hash.short()))?;
    if blob.hash() != hash {
        return Err(format!(
            "review-signatures blob {} failed content binding",
            hash.short()
        ));
    }
    let reviews =
        ReviewSignaturesBlob::decode(blob.content()).map_err(|error| error.to_string())?;
    let mut identities = Vec::with_capacity(reviews.signatures.len());
    for review in &reviews.signatures {
        verify_review_signature(state.state_id, review)?;
        if let Some(registry) = registry {
            let signed_at = DateTime::<Utc>::from_timestamp(review.signed_at, 0)
                .ok_or_else(|| "review signature has an invalid signed_at".to_string())?;
            match repo.verify_known_actor_key(
                &review.algorithm,
                &review.public_key,
                signed_at,
                registry,
            ) {
                AuthorshipVerification::Verified(identity) => identities.push(identity),
                other => {
                    return Err(format!(
                        "reviewer key resolution returned {}",
                        identity_error(&other)
                    ));
                }
            }
        }
    }
    Ok((reviews.signatures.len(), identities))
}

fn verify_review_signature(
    state_id: StateId,
    review: &ReviewSignature,
) -> std::result::Result<(), String> {
    review.validate().map_err(|error| error.to_string())?;
    let public_key = hex::decode(&review.public_key)
        .map_err(|error| format!("review public key is not hexadecimal: {error}"))?;
    let signature = hex::decode(&review.signature)
        .map_err(|error| format!("review signature is not hexadecimal: {error}"))?;
    let payload = signing_payload(
        state_id,
        review.kind,
        &review.scope,
        review.signed_at,
        review.justification.as_deref(),
    );
    verify_payload_signature(&payload, &review.algorithm, &public_key, &signature)
        .map_err(|error| format!("review signature did not verify: {error}"))
}
