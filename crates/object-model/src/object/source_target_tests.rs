use super::*;

fn range(start: u32, end: u32) -> SourceLineRange {
    SourceLineRange {
        start,
        end,
        start_affinity: SourceAffinity::After,
        end_affinity: SourceAffinity::Before,
    }
}

#[test]
fn inherited_targets_select_fork_without_rewriting_core_or_explicit_thread() {
    let parent = CollaborationScope {
        spool: uuid::Uuid::from_u128(1),
        thread: Some(ContentHash::compute(b"parent")),
    };
    let fork = CollaborationScope {
        thread: Some(ContentHash::compute(b"fork")),
        ..parent.clone()
    };
    let file = SourceFileCore {
        scope: parent.clone(),
        revision: CollaborationRevision::GitCommit {
            oid: "a".repeat(40),
        },
        path: "src/lib.rs".into(),
    };
    let target = SourceTargetCore {
        file: file.id().expect("valid original file"),
        revision: file.revision.clone(),
        selector: SourceSelector::Lines {
            range: range(10, 20),
        },
    };
    let original = rmp_serde::to_vec_named(&target).expect("original bytes");
    let inherited = SourceTargetReference {
        target: target.id().expect("target identity"),
        binding: SourceTargetBinding::ViewedThread,
    };
    let explicit = SourceTargetReference {
        target: inherited.target,
        binding: SourceTargetBinding::NamedThread {
            scope: parent.clone(),
        },
    };
    assert_eq!(inherited.binding.scope(&fork).expect("fork"), &fork);
    assert_eq!(
        explicit.binding.scope(&fork).expect("named parent"),
        &parent
    );
    assert_eq!(inherited.target, explicit.target);
    assert_eq!(
        original,
        rmp_serde::to_vec_named(&target).expect("unchanged evidence")
    );
    assert!(
        SourceTargetBinding::ViewedThread
            .scope(&CollaborationScope {
                thread: None,
                ..parent
            })
            .is_err(),
        "a spool is not a current Thread binding"
    );
}

#[test]
fn shared_insert_map_projects_many_ranges_without_changing_their_origins() {
    let map = SourceLineEditMap::new(
        30_000,
        30_003,
        vec![SourceLineEdit {
            old_start: 1,
            old_end: 1,
            new_start: 1,
            new_end: 4,
        }],
    )
    .expect("one shared insertion");
    let before = rmp_serde::to_vec_named(&map).expect("map bytes");
    for start in 2..10_002 {
        let origin = range(start, start + 2);
        assert_eq!(
            map.project(origin).expect("project"),
            SourceRangeProjection::Resolved {
                range: range(start + 3, start + 5),
                changed: false,
            }
        );
        assert_eq!(origin.start, start);
    }
    assert_eq!(map.edits().len(), 1);
    assert_eq!(rmp_serde::to_vec_named(&map).expect("map bytes"), before);
}

#[test]
fn boundary_affinity_deletion_and_replacement_are_explicit() {
    let insert = SourceLineEditMap::new(
        10,
        12,
        vec![SourceLineEdit {
            old_start: 3,
            old_end: 3,
            new_start: 3,
            new_end: 5,
        }],
    )
    .expect("insert");
    assert_eq!(
        insert
            .project(range(3, 5))
            .expect("exclude start insertion"),
        SourceRangeProjection::Resolved {
            range: range(5, 7),
            changed: false
        }
    );
    let growing_start = SourceLineRange {
        start_affinity: SourceAffinity::Before,
        ..range(3, 5)
    };
    assert_eq!(
        insert
            .project(growing_start)
            .expect("include start insertion"),
        SourceRangeProjection::Resolved {
            range: SourceLineRange {
                end: 7,
                ..growing_start
            },
            changed: true
        }
    );
    assert_eq!(
        insert.project(range(1, 3)).expect("exclude end insertion"),
        SourceRangeProjection::Resolved {
            range: range(1, 3),
            changed: false
        }
    );
    let growing_end = SourceLineRange {
        end_affinity: SourceAffinity::After,
        ..range(1, 3)
    };
    assert_eq!(
        insert.project(growing_end).expect("include end insertion"),
        SourceRangeProjection::Resolved {
            range: SourceLineRange {
                end: 5,
                ..growing_end
            },
            changed: true
        }
    );

    let deleted = SourceLineEditMap::new(
        10,
        7,
        vec![SourceLineEdit {
            old_start: 3,
            old_end: 6,
            new_start: 3,
            new_end: 3,
        }],
    )
    .expect("delete");
    assert_eq!(
        deleted.project(range(4, 5)).expect("deleted interior"),
        SourceRangeProjection::Deleted
    );
    assert_eq!(
        deleted.project(range(3, 6)).expect("deleted boundaries"),
        SourceRangeProjection::Deleted
    );
    assert_eq!(
        deleted
            .project(range(2, 5))
            .expect("partly deleted endpoint"),
        SourceRangeProjection::Ambiguous
    );
    assert_eq!(
        deleted
            .project(range(6, 8))
            .expect("after deleted interval"),
        SourceRangeProjection::Resolved {
            range: range(3, 5),
            changed: false
        }
    );

    let replaced = SourceLineEditMap::new(
        10,
        9,
        vec![SourceLineEdit {
            old_start: 3,
            old_end: 6,
            new_start: 3,
            new_end: 5,
        }],
    )
    .expect("replace");
    assert_eq!(
        replaced.project(range(4, 5)).expect("unproven interior"),
        SourceRangeProjection::Ambiguous
    );
    assert_eq!(
        replaced.project(range(3, 6)).expect("preserved boundaries"),
        SourceRangeProjection::Resolved {
            range: range(3, 5),
            changed: true
        }
    );
}

#[test]
fn corrupt_maps_are_rejected_during_decode_and_boundaries_do_not_overflow() {
    let invalid_map = serde_json::json!({
        "old_lines": 10, "new_lines": 12,
        "edits": [{ "old_start": 3, "old_end": 3, "new_start": 4, "new_end": 6 }]
    });
    assert!(
        serde_json::from_value::<SourceLineEditMap>(invalid_map).is_err(),
        "unchanged gaps cannot acquire an unexplained displacement"
    );
    assert!(
        SourceLineEditMap::new(10, 11, vec![]).is_err(),
        "unrecorded edits fail"
    );
    assert!(
        SourceLineEditMap::new(
            10,
            12,
            vec![
                SourceLineEdit {
                    old_start: 3,
                    old_end: 3,
                    new_start: 3,
                    new_end: 4
                },
                SourceLineEdit {
                    old_start: 3,
                    old_end: 3,
                    new_start: 4,
                    new_end: 5
                },
            ]
        )
        .is_err(),
        "adjacent insertions must be coalesced"
    );
    let maximum = SourceLineEditMap::new(
        u32::MAX - 1,
        u32::MAX,
        vec![SourceLineEdit {
            old_start: 0,
            old_end: 0,
            new_start: 0,
            new_end: 1,
        }],
    )
    .expect("maximum valid dimensions");
    assert_eq!(
        maximum
            .project(range(u32::MAX - 2, u32::MAX - 1))
            .expect("no overflow"),
        SourceRangeProjection::Resolved {
            range: range(u32::MAX - 1, u32::MAX),
            changed: false
        }
    );
    assert!(
        maximum.project(range(0, u32::MAX)).is_err(),
        "original dimensions bound reads"
    );
}

#[test]
fn independent_branch_maps_project_the_same_core_differently() {
    let origin = range(4, 6);
    let parent = SourceLineEditMap::new(10, 10, vec![]).expect("unchanged parent");
    let fork = SourceLineEditMap::new(
        10,
        12,
        vec![SourceLineEdit {
            old_start: 1,
            old_end: 1,
            new_start: 1,
            new_end: 3,
        }],
    )
    .expect("fork insertion");
    assert_eq!(
        parent.project(origin).expect("parent"),
        SourceRangeProjection::Resolved {
            range: origin,
            changed: false,
        }
    );
    assert_eq!(
        fork.project(origin).expect("fork"),
        SourceRangeProjection::Resolved {
            range: range(6, 8),
            changed: false,
        }
    );
}
