//! Immutable source dependencies of an authored record. These are identities
//! to authorize, never authority. Current target locations are a separate view.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    AnnotationTag, CollaborationAnchor, CollaborationCodecError, CollaborationRevision,
    CollaborationScope, CollaborationSourceAnchor,
};
use crate::object::{ChangeId, ContentHash, source_target::SourceTargetBinding};

/// A distinct source permission required to emit an authored anchor or tag.
/// Revision requirements include the original path, but not line coordinates:
/// entry visibility is a property of the file, independent of its referrers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum AuthoredSourceRequirement {
    /// An explicitly named Thread must itself be readable.
    Thread { scope: CollaborationScope },
    /// An exact authored revision and, if nonempty, its file must be readable.
    Revision {
        scope: CollaborationScope,
        revision: CollaborationRevision,
        path: String,
    },
    /// A logical identity needs a separately authorized revision selection.
    /// Absence of such a selection must not become an empty permission set.
    Change {
        scope: CollaborationScope,
        change_id: ChangeId,
    },
}

impl AuthoredSourceRequirement {
    /// Stable derived index key. This does not alter any signed record bytes.
    pub fn id(&self) -> Result<ContentHash, CollaborationCodecError> {
        let bytes = rmp_serde::to_vec_named(self)
            .map_err(|error| CollaborationCodecError::Encoding(error.to_string()))?;
        Ok(ContentHash::compute_typed(
            "heddle-authored-source-requirement-v1",
            &bytes,
        ))
    }
}

/// Normalize the source dependencies of a signed anchor and its tags.
///
/// Repeated references to a file share a single predicate. ViewedThread targets
/// retain their original source predicate; movement/deletion of the derived
/// current target cannot silently remove readable history. Named and pinned
/// bindings add their explicit Thread/revision, independently of that original.
///
/// This function covers source references only. Record audience, extracted
/// discussion visibility, and other entity authorization remain separate gates.
pub fn authored_source_requirements(
    scope: &CollaborationScope,
    anchor: &CollaborationAnchor,
    tags: &[AnnotationTag],
) -> Result<Vec<AuthoredSourceRequirement>, CollaborationCodecError> {
    super::operation::validate_anchor(anchor)?;
    super::validate_annotation_tags(tags)?;
    let mut requirements = BTreeMap::new();
    let mut insert = |value: AuthoredSourceRequirement| {
        requirements.insert(value.id()?, value);
        Ok::<_, CollaborationCodecError>(())
    };
    match anchor {
        CollaborationAnchor::Repository => {}
        CollaborationAnchor::Source { source } => source_requirements(scope, source, &mut insert)?,
        CollaborationAnchor::State { state_id } => insert(AuthoredSourceRequirement::Revision {
            scope: scope.clone(),
            revision: CollaborationRevision::State {
                state_id: *state_id,
            },
            path: String::new(),
        })?,
        CollaborationAnchor::Path { state_id, path }
        | CollaborationAnchor::Symbol { state_id, path, .. } => {
            insert(AuthoredSourceRequirement::Revision {
                scope: scope.clone(),
                revision: CollaborationRevision::State {
                    state_id: *state_id,
                },
                path: path.clone(),
            })?;
        }
        CollaborationAnchor::Change { change_id } => insert(AuthoredSourceRequirement::Change {
            scope: scope.clone(),
            change_id: *change_id,
        })?,
    }
    for tag in tags {
        if let AnnotationTag::Source { target }
        | AnnotationTag::Symbol {
            target: Some(target),
            ..
        } = tag
        {
            source_requirements(&target.scope, &target.source, &mut insert)?;
        }
    }
    Ok(requirements.into_values().collect())
}

fn source_requirements(
    scope: &CollaborationScope,
    source: &CollaborationSourceAnchor,
    insert: &mut impl FnMut(AuthoredSourceRequirement) -> Result<(), CollaborationCodecError>,
) -> Result<(), CollaborationCodecError> {
    insert(AuthoredSourceRequirement::Revision {
        scope: scope.clone(),
        revision: source.revision.clone(),
        path: source.path.clone(),
    })?;
    match source.target.as_ref().map(|target| &target.binding) {
        None | Some(SourceTargetBinding::ViewedThread) => Ok(()),
        Some(SourceTargetBinding::NamedThread { scope }) => {
            insert(AuthoredSourceRequirement::Thread {
                scope: scope.clone(),
            })
        }
        Some(SourceTargetBinding::PinnedRevision { scope, revision }) => {
            insert(AuthoredSourceRequirement::Revision {
                scope: scope.clone(),
                revision: revision.clone(),
                path: String::new(),
            })
        }
    }
}
