//! Immutable source dependencies of an authored record. These are identities
//! to authorize, never authority. Current target locations are a separate view.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    AnnotationTag, CollaborationAnchor, CollaborationCodecError, CollaborationRevision,
    CollaborationScope, CollaborationSourceAnchor,
};
use crate::object::{
    ChangeId, ContentHash, StateId,
    source_target::SourceTargetBinding,
    thread_replication::metadata::{Review, ReviewCoverage},
};

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
    /// A Read attestation over the complete exact State requires that no
    /// currently admitted entry in that source is withheld. An empty Revision
    /// path checks only State visibility and must never represent this rule.
    WholeSource {
        scope: CollaborationScope,
        state_id: StateId,
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

/// Source predicates of a signed review, independent of its current display
/// location. The source and target scopes are explicit because a fork review
/// can compare a source Thread with its independently shared parent Thread.
pub fn authored_review_requirements(
    source_scope: &CollaborationScope,
    target_scope: &CollaborationScope,
    review: &Review,
) -> Result<Vec<AuthoredSourceRequirement>, CollaborationCodecError> {
    if source_scope.spool != target_scope.spool
        || source_scope.thread.is_none()
        || target_scope.thread.is_none()
    {
        return Err(CollaborationCodecError::Invalid(
            "review source and target require exact Threads in one Spool".into(),
        ));
    }
    let mut requirements = BTreeMap::new();
    let mut insert = |value: AuthoredSourceRequirement| {
        requirements.insert(value.id()?, value);
        Ok::<_, CollaborationCodecError>(())
    };
    insert(AuthoredSourceRequirement::Revision {
        scope: source_scope.clone(),
        revision: CollaborationRevision::State {
            state_id: review.source,
        },
        path: String::new(),
    })?;
    insert(AuthoredSourceRequirement::Revision {
        scope: target_scope.clone(),
        revision: CollaborationRevision::State {
            state_id: review.target,
        },
        path: String::new(),
    })?;
    match &review.coverage {
        Some(ReviewCoverage::WholeSource) => insert(AuthoredSourceRequirement::WholeSource {
            scope: source_scope.clone(),
            state_id: review.source,
        })?,
        Some(ReviewCoverage::Symbols(anchors)) => {
            if anchors.is_empty() || anchors.len() > 128 {
                return Err(CollaborationCodecError::Invalid(
                    "review symbol coverage exceeds bounds".into(),
                ));
            }
            for anchor in anchors {
                if anchor.file.is_empty()
                    || anchor.file.len() > 4096
                    || anchor.file.starts_with('/')
                    || anchor
                        .file
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == "..")
                {
                    return Err(CollaborationCodecError::Invalid(
                        "review symbol path must be relative and canonical".into(),
                    ));
                }
                insert(AuthoredSourceRequirement::Revision {
                    scope: source_scope.clone(),
                    revision: CollaborationRevision::State {
                        state_id: review.source,
                    },
                    path: anchor.file.clone(),
                })?;
            }
        }
        None => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::thread_replication::metadata::{ReviewKind, ReviewSymbolAnchor};

    fn review(coverage: Option<ReviewCoverage>) -> Review {
        Review {
            id: uuid::Uuid::from_u128(1),
            source: StateId::from_bytes([11; 32]),
            target: StateId::from_bytes([12; 32]),
            policy_version: ContentHash::from_bytes([13; 32]),
            kind: ReviewKind::Read,
            explanation: String::new(),
            revokes: None,
            expires_at_unix_seconds: None,
            coverage,
        }
    }

    #[test]
    fn complete_read_requires_whole_source_in_addition_to_both_exact_revisions() {
        let source = CollaborationScope {
            spool: uuid::Uuid::from_u128(1),
            thread: Some(ContentHash::from_bytes([1; 32])),
        };
        let target = CollaborationScope {
            thread: Some(ContentHash::from_bytes([2; 32])),
            ..source.clone()
        };
        let value = review(Some(ReviewCoverage::WholeSource));
        let requirements = authored_review_requirements(&source, &target, &value)
            .expect("exact review requirements");
        assert_eq!(requirements.len(), 3);
        assert!(
            requirements.contains(&AuthoredSourceRequirement::WholeSource {
                scope: source.clone(),
                state_id: value.source,
            })
        );
        assert!(requirements.contains(&AuthoredSourceRequirement::Revision {
            scope: target.clone(),
            revision: CollaborationRevision::State {
                state_id: value.target
            },
            path: String::new(),
        }));
        assert_ne!(
            AuthoredSourceRequirement::WholeSource {
                scope: source.clone(),
                state_id: value.source
            }
            .id()
            .expect("whole source key"),
            AuthoredSourceRequirement::Revision {
                scope: source,
                revision: CollaborationRevision::State {
                    state_id: value.source
                },
                path: String::new(),
            }
            .id()
            .expect("state key"),
        );
    }

    #[test]
    fn symbol_read_requires_each_original_path_and_rejects_unscoped_reviews() {
        let scope = CollaborationScope {
            spool: uuid::Uuid::from_u128(1),
            thread: Some(ContentHash::from_bytes([1; 32])),
        };
        let value = review(Some(ReviewCoverage::Symbols(vec![
            ReviewSymbolAnchor {
                file: "src/main.rs".into(),
                symbol: "run".into(),
            },
            ReviewSymbolAnchor {
                file: "src/main.rs".into(),
                symbol: "main".into(),
            },
            ReviewSymbolAnchor {
                file: "src/lib.rs".into(),
                symbol: "parse".into(),
            },
        ])));
        let requirements = authored_review_requirements(&scope, &scope, &value)
            .expect("exact symbol requirements");
        assert_eq!(
            requirements.len(),
            4,
            "two revisions and two distinct paths"
        );
        assert!(requirements.contains(&AuthoredSourceRequirement::Revision {
            scope: scope.clone(),
            revision: CollaborationRevision::State {
                state_id: value.source
            },
            path: "src/main.rs".into(),
        }));
        assert!(
            authored_review_requirements(
                &CollaborationScope {
                    thread: None,
                    ..scope.clone()
                },
                &scope,
                &value
            )
            .is_err()
        );
        assert!(
            authored_review_requirements(
                &scope,
                &CollaborationScope {
                    spool: uuid::Uuid::from_u128(2),
                    ..scope.clone()
                },
                &value
            )
            .is_err()
        );
    }
}
