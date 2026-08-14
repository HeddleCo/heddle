// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashSet};

use objects::{
    error::Result,
    object::{ContentHash, KeyBindingRegistry, StateId},
    store::ObjectStore,
};
use repo::{AuthorshipVerification, Repository, TrustedKey};
use schemars::JsonSchema;
use serde::Serialize;

use super::MAX_REGISTRY_BLOB_BYTES;

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
        self.status.starts_with("Verified(")
    }
}

pub(super) enum RegistryDiscovery {
    Absent,
    Available {
        registry: Box<KeyBindingRegistry>,
        hash: ContentHash,
        authority: TrustedKey,
    },
    Invalid(String),
}

pub(super) fn discover_registry(repo: &Repository) -> Result<RegistryDiscovery> {
    let Some(anchor) = repo.key_binding_registry_anchor()? else {
        return Ok(RegistryDiscovery::Absent);
    };
    let expected_head = match ContentHash::from_hex(&anchor.registry_hash) {
        Ok(hash) => hash,
        Err(_) => {
            return Ok(RegistryDiscovery::Invalid(
                "trusted key-binding registry head is not a 32-byte hexadecimal digest".to_string(),
            ));
        }
    };
    let registries = registry_objects(repo)?;
    let mut expected_hash = expected_head;
    let mut expected_epoch = anchor.epoch;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(expected_hash) {
            return Ok(RegistryDiscovery::Invalid(
                "key-binding registry checkpoint chain contains a cycle".to_string(),
            ));
        }
        let Some(registry) = registries.get(&expected_hash) else {
            return Ok(RegistryDiscovery::Invalid(format!(
                "trusted key-binding registry checkpoint {} at epoch {expected_epoch} is missing",
                expected_hash.short()
            )));
        };
        if registry.epoch != expected_epoch {
            return Ok(RegistryDiscovery::Invalid(format!(
                "trusted key-binding registry checkpoint {} has epoch {}, expected {expected_epoch}",
                expected_hash.short(),
                registry.epoch
            )));
        }
        if let Err(error) = repo.verify_key_binding_registry_checkpoint(registry, &anchor.authority)
        {
            return Ok(RegistryDiscovery::Invalid(error.to_string()));
        }
        if expected_epoch == 0 {
            break;
        }
        expected_hash = registry
            .previous_registry
            .expect("validated non-genesis checkpoint has a predecessor");
        expected_epoch -= 1;
    }
    let registry = registries
        .get(&expected_head)
        .expect("validated registry head remains available")
        .clone();
    Ok(RegistryDiscovery::Available {
        registry: Box::new(registry),
        hash: expected_head,
        authority: anchor.authority,
    })
}

fn registry_objects(repo: &Repository) -> Result<BTreeMap<ContentHash, KeyBindingRegistry>> {
    let mut registries = BTreeMap::new();
    for hash in repo.store().list_blobs()? {
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
            continue;
        }
        let registry_hash = registry.content_hash().expect("decoded registry is valid");
        if let Some(prior) = registries.insert(registry_hash, registry.clone())
            && prior != registry
        {
            registries.remove(&registry_hash);
        }
    }
    Ok(registries)
}

pub(super) fn identity_error(result: &AuthorshipVerification) -> &'static str {
    match result {
        AuthorshipVerification::Verified(_) => "Verified",
        AuthorshipVerification::UnknownKey => "UnknownKey",
        AuthorshipVerification::Revoked => "Revoked",
        AuthorshipVerification::UnauthorizedRole { .. } => "UnauthorizedRole",
        AuthorshipVerification::Invalid => "Invalid",
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
