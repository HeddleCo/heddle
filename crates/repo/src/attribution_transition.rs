// SPDX-License-Identifier: Apache-2.0
//! Recheck reported edit transitions against immutable snapshot content.
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
};

use objects::{
    error::{HeddleError, Result},
    object::{
        AttributionEvidenceV1, AttributionFileChange, AttributionOperationResolution, Blob,
        ContentHash, StateId, Tree, TreeEntryTarget,
    },
    store::ObjectSource,
};

/// Resolve an ordinary file without treating links or unavailable trees as
/// absence. Prepared snapshots supply new subtrees before they enter the store.
pub fn attribution_path_blob(
    source: &(impl ObjectSource + ?Sized),
    root: &Tree,
    pending_trees: &HashMap<ContentHash, &Tree>,
    path: &str,
) -> Result<Option<ContentHash>> {
    AttributionFileChange {
        path: path.to_string(),
        before: None,
        after: None,
    }
    .validate()
    .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    let mut tree = Cow::Borrowed(root);
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let Some(entry) = tree.get(part) else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return match entry.target() {
                TreeEntryTarget::Blob { hash, .. } => Ok(Some(*hash)),
                _ => Err(invalid("attribution path is not an ordinary file")),
            };
        }
        let TreeEntryTarget::Tree { hash } = entry.target() else {
            return Err(invalid("attribution path traverses a non-directory"));
        };
        let hash = *hash;
        let next =
            match pending_trees.get(&hash) {
                Some(tree) => Cow::Borrowed(*tree),
                None => Cow::Owned(source.get_tree(&hash)?.ok_or_else(|| {
                    HeddleError::MissingObject {
                        object_type: "attribution transition tree".into(),
                        id: hash.to_hex(),
                    }
                })?),
            };
        if next.hash() != hash {
            return Err(invalid("attribution transition tree hash mismatch"));
        }
        tree = next;
    }
    Err(invalid("attribution path is empty"))
}

pub(crate) fn validate_snapshot_attribution(
    source: &(impl ObjectSource + ?Sized),
    evidence: &Blob,
    first_parent: Option<StateId>,
    actual: &Tree,
    pending_trees: &HashMap<ContentHash, &Tree>,
) -> Result<()> {
    let evidence = AttributionEvidenceV1::from_blob(evidence)
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    if !evidence
        .operations
        .iter()
        .any(|operation| operation.resolution == AttributionOperationResolution::ContentBound)
    {
        return Ok(());
    }
    let parent_tree = first_parent
        .map(|id| {
            let state = source
                .get_state(&id)?
                .ok_or(HeddleError::StateNotFound(id))?;
            source
                .get_tree(&state.tree)?
                .ok_or_else(|| HeddleError::MissingObject {
                    object_type: "attribution first-parent tree".into(),
                    id: state.tree.to_hex(),
                })
        })
        .transpose()?;
    validate_attribution_transitions(
        source,
        &evidence,
        parent_tree.as_ref(),
        actual,
        pending_trees,
    )
}

fn validate_attribution_transitions(
    source: &(impl ObjectSource + ?Sized),
    evidence: &AttributionEvidenceV1,
    parent: Option<&Tree>,
    actual: &Tree,
    pending_trees: &HashMap<ContentHash, &Tree>,
) -> Result<()> {
    let mut paths =
        BTreeMap::<&str, Vec<(&AttributionFileChange, AttributionOperationResolution)>>::new();
    for operation in &evidence.operations {
        for change in &operation.changes {
            paths
                .entry(&change.path)
                .or_default()
                .push((change, operation.resolution));
        }
    }
    for (path, changes) in paths {
        if !changes
            .iter()
            .any(|(_, resolution)| *resolution == AttributionOperationResolution::ContentBound)
        {
            continue;
        }
        if changes
            .iter()
            .any(|(_, resolution)| *resolution != AttributionOperationResolution::ContentBound)
        {
            return Err(invalid(
                "content-bound attribution overlaps an unresolved operation",
            ));
        }
        let mut previous = match parent {
            Some(parent) => attribution_path_blob(source, parent, &HashMap::new(), path)?,
            None => None,
        };
        for (change, _) in changes {
            if change.before != previous {
                return Err(invalid(
                    "attribution operation chain differs from first-parent content",
                ));
            }
            previous = change.after;
        }
        if previous != attribution_path_blob(source, actual, pending_trees, path)? {
            return Err(invalid(
                "attribution operation chain differs from captured content",
            ));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.to_string())
}

#[cfg(test)]
#[path = "attribution_transition_tests.rs"]
mod tests;
