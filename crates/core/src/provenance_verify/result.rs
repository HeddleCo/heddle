// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use objects::{
    error::Result,
    object::{ContentHash, KeyBindingRegistry, StateAttachmentBody, StateId},
    store::ObjectStore,
};
use repo::{AuthorshipVerification, Repository};

use super::{MAX_REGISTRY_BLOB_BYTES, RegistryDiscovery, StateProvenanceVerification};

pub(super) fn discover_registry(repo: &Repository) -> Result<RegistryDiscovery> {
    let source_and_metadata_blobs = referenced_content_hashes(repo)?;
    let mut candidates = Vec::new();
    for hash in repo.store().list_blobs()? {
        if source_and_metadata_blobs.contains(&hash) {
            continue;
        }
        let Ok(Some(blob)) = repo.store().get_blob(&hash) else {
            continue;
        };
        if blob.size() > MAX_REGISTRY_BLOB_BYTES {
            continue;
        }
        let Ok(registry) = KeyBindingRegistry::decode(blob.content()) else {
            continue;
        };
        if blob.hash() != hash {
            return Ok(RegistryDiscovery::Invalid(format!(
                "key-binding registry blob {} failed content binding",
                hash.short()
            )));
        }
        if !candidates
            .iter()
            .any(|(candidate_hash, _)| *candidate_hash == hash)
        {
            candidates.push((hash, registry));
        }
    }
    match candidates.len() {
        0 => Ok(RegistryDiscovery::Absent),
        1 => {
            let (hash, registry) = candidates.pop().expect("one candidate exists");
            Ok(RegistryDiscovery::Available { registry, hash })
        }
        count => Ok(RegistryDiscovery::Invalid(format!(
            "found {count} distinct key-binding registries; offline trust root is ambiguous"
        ))),
    }
}

fn referenced_content_hashes(repo: &Repository) -> Result<HashSet<ContentHash>> {
    let mut referenced = HashSet::new();
    let mut trees = Vec::new();
    for state_id in repo.store().list_states()? {
        let Ok(Some(state)) = repo.store().get_state(&state_id) else {
            continue;
        };
        trees.push(state.tree);
        if let Some(provenance) = state.provenance {
            trees.push(provenance);
        }
        for attachment in repo.list_state_attachments(&state_id)? {
            match attachment.body {
                StateAttachmentBody::Context(root) | StateAttachmentBody::SemanticIndex(root) => {
                    trees.push(root)
                }
                StateAttachmentBody::RiskSignals(hash)
                | StateAttachmentBody::ReviewSignatures(hash)
                | StateAttachmentBody::Discussions(hash)
                | StateAttachmentBody::StructuredConflicts(hash) => {
                    referenced.insert(hash);
                }
                StateAttachmentBody::Signature(_) => {}
            }
        }
    }
    while let Some(hash) = trees.pop() {
        if !referenced.insert(hash) {
            continue;
        }
        let Ok(Some(tree)) = repo.store().get_tree(&hash) else {
            continue;
        };
        for entry in tree.entries() {
            if let Some(child) = entry.tree_hash() {
                trees.push(child);
            } else if let Some(blob) = entry.blob_hash() {
                referenced.insert(blob);
            }
        }
    }
    Ok(referenced)
}

pub(super) fn identity_error(result: &AuthorshipVerification) -> &'static str {
    match result {
        AuthorshipVerification::Verified(_) => "Verified",
        AuthorshipVerification::UnknownKey => "UnknownKey",
        AuthorshipVerification::Revoked => "Revoked",
        AuthorshipVerification::Invalid => "Invalid",
    }
}

pub(super) fn integrity_only(state_id: StateId, detail: &str) -> StateProvenanceVerification {
    StateProvenanceVerification {
        state_id: state_id.to_string_full(),
        status: "IntegrityOnly".to_string(),
        identity: None,
        failed_link: None,
        detail: detail.to_string(),
        reviews_verified: 0,
        reviewer_identities: Vec::new(),
    }
}

pub(super) fn legacy(state_id: StateId) -> StateProvenanceVerification {
    StateProvenanceVerification {
        state_id: state_id.to_string_full(),
        status: "Legacy".to_string(),
        identity: None,
        failed_link: Some("content".to_string()),
        detail: "only an untagged legacy authorship signature verifies".to_string(),
        reviews_verified: 0,
        reviewer_identities: Vec::new(),
    }
}

pub(super) fn failed(state_id: StateId, link: &str, detail: &str) -> StateProvenanceVerification {
    StateProvenanceVerification {
        state_id: state_id.to_string_full(),
        status: format!("FAILED({link})"),
        identity: None,
        failed_link: Some(link.to_string()),
        detail: detail.to_string(),
        reviews_verified: 0,
        reviewer_identities: Vec::new(),
    }
}
