// SPDX-License-Identifier: Apache-2.0
//! Snapshot-time persistence for context annotation anchor travel.

#![cfg(feature = "tree-sitter-symbols")]

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use objects::{
    object::{
        Annotation, AnnotationAnchorStatus, AnnotationScope, AnnotationStatus, ContentHash,
        ContextBlob, ContextTarget, EntryType, State, StateAttachmentBody, Tree,
    },
    store::ObjectStore,
};

use crate::{
    HeddleError, Repository, Result,
    context_anchor_travel::{ContextFileTravel, context_file_travel},
};

impl Repository {
    pub(crate) fn compute_and_persist_context_anchor_travel(
        &self,
        parent_state: &State,
        new_tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
    ) -> Result<Option<ContentHash>> {
        let Some(parent_context_hash) = self
            .latest_state_attachment(&parent_state.id(), crate::StateAttachmentKind::Context)?
            .and_then(|attachment| match attachment.body {
                StateAttachmentBody::Context(hash) => Some(hash),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let entries = self.list_context_entries(&parent_context_hash, None)?;
        if !entries.iter().any(|entry| {
            matches!(entry.target, ContextTarget::File { .. })
                && entry.blob.annotations.iter().any(is_live_durable_anchor)
        }) {
            return Ok(Some(parent_context_hash));
        }

        let parent_tree = self
            .store()
            .get_tree(&parent_state.tree)?
            .ok_or_else(|| missing_context_travel_object("tree", parent_state.tree))?;
        let old_files = self.collect_context_travel_file_bytes(&parent_tree, None)?;
        let new_files = self.collect_context_travel_file_bytes(new_tree, source_blobs)?;
        let mut grouped: BTreeMap<String, (ContextTarget, Vec<Annotation>)> = BTreeMap::new();
        let mut changed = false;

        for entry in entries {
            let decision = match &entry.target {
                ContextTarget::File { path } => {
                    Some(context_file_travel(path.as_str(), &old_files, &new_files))
                }
                ContextTarget::State { .. } => None,
            };
            for mut annotation in entry.blob.annotations {
                let mut destination = entry.target.clone();
                if is_live_durable_anchor(&annotation)
                    && let Some(decision) = decision.as_ref()
                {
                    let decision = carry_forward_ambiguity(decision, &annotation, &new_files);
                    let next_status = match &decision {
                        ContextFileTravel::Present => AnnotationAnchorStatus::Resolved,
                        ContextFileTravel::Moved(path) => {
                            destination = ContextTarget::file(path.clone()).map_err(|error| {
                                HeddleError::InvalidObject(format!(
                                    "invalid context anchor destination: {error}"
                                ))
                            })?;
                            AnnotationAnchorStatus::Resolved
                        }
                        ContextFileTravel::Ambiguous(candidate_paths) => {
                            AnnotationAnchorStatus::Ambiguous {
                                candidate_paths: candidate_paths.clone(),
                            }
                        }
                        ContextFileTravel::Orphaned => AnnotationAnchorStatus::Orphaned,
                    };
                    if annotation.anchor_status != next_status {
                        annotation.anchor_status = next_status;
                        changed = true;
                    }
                    if destination != entry.target {
                        changed = true;
                    }
                }
                let key = destination.storage_path().to_string_lossy().into_owned();
                grouped
                    .entry(key)
                    .or_insert_with(|| (destination, Vec::new()))
                    .1
                    .push(annotation);
            }
        }

        if !changed {
            return Ok(Some(parent_context_hash));
        }

        let mut root = None;
        for (_, (target, annotations)) in grouped {
            let next =
                self.set_context_blob(root.as_ref(), &target, &ContextBlob::new(annotations))?;
            root = Some(next);
        }
        Ok(root)
    }

    fn collect_context_travel_file_bytes(
        &self,
        tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut files = HashMap::new();
        self.collect_context_travel_file_bytes_inner(
            tree,
            PathBuf::new(),
            source_blobs,
            &mut files,
        )?;
        Ok(files)
    }

    fn collect_context_travel_file_bytes_inner(
        &self,
        tree: &Tree,
        prefix: PathBuf,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        files: &mut HashMap<String, Vec<u8>>,
    ) -> Result<()> {
        for entry in tree.entries() {
            let path = prefix.join(entry.name());
            match entry.entry_type() {
                EntryType::Blob => {
                    let Some(path) = path.to_str() else {
                        continue;
                    };
                    let Some(hash) = entry.blob_hash() else {
                        continue;
                    };
                    let bytes = match source_blobs.and_then(|blobs| blobs.get(&hash).copied()) {
                        Some(bytes) => bytes.to_vec(),
                        None => self
                            .store()
                            .get_blob(&hash)?
                            .ok_or_else(|| missing_context_travel_object("blob", hash))?
                            .content()
                            .to_vec(),
                    };
                    files.insert(path.to_string(), bytes);
                }
                EntryType::Tree => {
                    let Some(hash) = entry.tree_hash() else {
                        continue;
                    };
                    let subtree = self
                        .store()
                        .get_tree(&hash)?
                        .ok_or_else(|| missing_context_travel_object("tree", hash))?;
                    self.collect_context_travel_file_bytes_inner(
                        &subtree,
                        path,
                        source_blobs,
                        files,
                    )?;
                }
                EntryType::Symlink | EntryType::Gitlink | EntryType::Spoollink => {}
            }
        }
        Ok(())
    }
}

fn is_live_durable_anchor(annotation: &Annotation) -> bool {
    annotation.status == AnnotationStatus::Active
        && matches!(
            annotation.scope,
            AnnotationScope::File | AnnotationScope::Symbol { .. }
        )
}

fn carry_forward_ambiguity(
    decision: &ContextFileTravel,
    annotation: &Annotation,
    new_files: &HashMap<String, Vec<u8>>,
) -> ContextFileTravel {
    if !matches!(decision, ContextFileTravel::Orphaned) {
        return decision.clone();
    }
    let AnnotationAnchorStatus::Ambiguous { candidate_paths } = &annotation.anchor_status else {
        return ContextFileTravel::Orphaned;
    };
    let surviving: Vec<String> = candidate_paths
        .iter()
        .filter(|path| new_files.contains_key(*path))
        .cloned()
        .collect();
    match surviving.as_slice() {
        [] => ContextFileTravel::Orphaned,
        [path] => ContextFileTravel::Moved(path.clone()),
        _ => ContextFileTravel::Ambiguous(surviving),
    }
}

fn missing_context_travel_object(object_type: &str, hash: ContentHash) -> HeddleError {
    HeddleError::MissingObject {
        object_type: object_type.to_string(),
        id: hash.to_hex(),
    }
}

#[cfg(test)]
mod tests {
    use objects::object::AnnotationKind;

    use super::*;

    #[test]
    fn persisted_ambiguity_stays_ambiguous_until_one_candidate_survives() {
        let mut annotation = Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Invariant,
            "context".to_string(),
            vec![],
            "test@example.com".to_string(),
            1_700_000_000,
            None,
            None,
        );
        annotation.anchor_status = AnnotationAnchorStatus::Ambiguous {
            candidate_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
        };

        let both = HashMap::from([
            ("src/a.rs".to_string(), b"a".to_vec()),
            ("src/b.rs".to_string(), b"b".to_vec()),
        ]);
        assert_eq!(
            carry_forward_ambiguity(&ContextFileTravel::Orphaned, &annotation, &both),
            ContextFileTravel::Ambiguous(vec!["src/a.rs".to_string(), "src/b.rs".to_string(),])
        );

        let one = HashMap::from([("src/b.rs".to_string(), b"b".to_vec())]);
        assert_eq!(
            carry_forward_ambiguity(&ContextFileTravel::Orphaned, &annotation, &one),
            ContextFileTravel::Moved("src/b.rs".to_string())
        );
    }
}
