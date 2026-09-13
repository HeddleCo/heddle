use heddle_object_model::object::{
    AnnotationSourceReference, AnnotationTag, AuthoredSourceRequirement as R, CollaborationAnchor,
    CollaborationRevision, CollaborationScope, CollaborationSourceAnchor, ContentHash, StateId,
    authored_source_requirements,
    source_target::{SourceTargetBinding, SourceTargetReference},
};
use uuid::Uuid;

fn scope(thread: u8) -> CollaborationScope {
    CollaborationScope {
        spool: Uuid::from_u128(1),
        thread: Some(ContentHash::from_bytes([thread; 32])),
    }
}
fn source(binding: SourceTargetBinding) -> CollaborationSourceAnchor {
    CollaborationSourceAnchor {
        revision: CollaborationRevision::State {
            state_id: StateId::from_bytes([3; 32]),
        },
        path: "original.rs".into(),
        symbol_id: String::new(),
        start_line: Some(2),
        end_line: Some(4),
        target: Some(SourceTargetReference {
            target: ContentHash::from_bytes([4; 32]),
            binding,
        }),
    }
}
fn original() -> R {
    R::Revision {
        scope: scope(2),
        revision: CollaborationRevision::State {
            state_id: StateId::from_bytes([3; 32]),
        },
        path: "original.rs".into(),
    }
}

#[test]
fn many_lines_and_symbols_share_the_original_file_requirement() {
    let anchor = CollaborationAnchor::Source {
        source: source(SourceTargetBinding::ViewedThread),
    };
    let tags: Vec<_> = (0..128)
        .map(|index| {
            let mut target = source(SourceTargetBinding::ViewedThread);
            target.start_line = Some(index + 1);
            target.end_line = Some(index + 2);
            AnnotationTag::Source {
                target: AnnotationSourceReference {
                    scope: scope(2),
                    source: target,
                },
            }
        })
        .collect();
    let normalized =
        authored_source_requirements(&scope(2), &anchor, &tags).expect("valid references");
    assert_eq!(
        normalized,
        vec![original()],
        "referrer count and coordinates must not multiply file visibility predicates"
    );
    let reversed: Vec<_> = tags.into_iter().rev().collect();
    assert_eq!(
        normalized,
        authored_source_requirements(&scope(2), &anchor, &reversed).expect("reversed references")
    );
}

#[test]
fn named_and_pinned_bindings_keep_both_original_and_explicit_scope() {
    let named = CollaborationAnchor::Source {
        source: source(SourceTargetBinding::NamedThread { scope: scope(5) }),
    };
    let requirements =
        authored_source_requirements(&scope(2), &named, &[]).expect("named reference");
    assert_eq!(requirements.len(), 2);
    assert!(requirements.contains(&original()));
    assert!(
        requirements.contains(&R::Thread { scope: scope(5) }),
        "explicit named Thread must independently be readable"
    );
    let revision = CollaborationRevision::State {
        state_id: StateId::from_bytes([6; 32]),
    };
    let pinned = CollaborationAnchor::Source {
        source: source(SourceTargetBinding::PinnedRevision {
            scope: scope(5),
            revision: revision.clone(),
        }),
    };
    let requirements =
        authored_source_requirements(&scope(2), &pinned, &[]).expect("pinned reference");
    assert_eq!(requirements.len(), 2);
    assert!(requirements.contains(&original()));
    assert!(
        requirements.contains(&R::Revision {
            scope: scope(5),
            revision,
            path: String::new()
        }),
        "pinned revision must not degrade to Thread access"
    );
}

#[test]
fn exact_revision_scope_and_path_remain_distinct_and_git_does_not_disappear() {
    let anchor = CollaborationAnchor::Source {
        source: source(SourceTargetBinding::ViewedThread),
    };
    let mut sources = vec![AnnotationSourceReference {
        scope: scope(7),
        source: source(SourceTargetBinding::ViewedThread),
    }];
    let mut other_path = source(SourceTargetBinding::ViewedThread);
    other_path.path = "other.rs".into();
    sources.push(AnnotationSourceReference {
        scope: scope(2),
        source: other_path,
    });
    let mut git = source(SourceTargetBinding::ViewedThread);
    git.revision = CollaborationRevision::GitCommit {
        oid: "a".repeat(40),
    };
    sources.push(AnnotationSourceReference {
        scope: scope(2),
        source: git,
    });
    let tags = sources
        .into_iter()
        .map(|target| AnnotationTag::Source { target })
        .collect::<Vec<_>>();
    let requirements = authored_source_requirements(&scope(2), &anchor, &tags)
        .expect("mixed exact source references");
    assert_eq!(requirements.len(), 4);
    assert!(
        requirements.iter().any(|requirement| matches!(
            requirement,
            R::Revision {
                revision: CollaborationRevision::GitCommit { .. },
                ..
            }
        )),
        "unsupported current readers must deny Git rather than treating it as no dependency"
    );
    let keys = requirements
        .iter()
        .map(|r| r.id().expect("canonical index key"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 4);
}

#[test]
fn repository_only_records_have_no_source_dependencies_but_invalid_tags_fail() {
    assert!(
        authored_source_requirements(&scope(2), &CollaborationAnchor::Repository, &[])
            .expect("repository anchor")
            .is_empty()
    );
    let mut invalid = source(SourceTargetBinding::ViewedThread);
    invalid.path = "../outside.rs".into();
    assert!(
        authored_source_requirements(
            &scope(2),
            &CollaborationAnchor::Repository,
            &[AnnotationTag::Source {
                target: AnnotationSourceReference {
                    scope: scope(2),
                    source: invalid
                }
            }]
        )
        .is_err()
    );
}
