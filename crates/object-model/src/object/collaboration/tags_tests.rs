use uuid::Uuid;

use super::*;
use crate::object::{CollaborationRevision, ContentHash};

fn source(path: &str, start: u32, end: u32) -> AnnotationSourceReference {
    AnnotationSourceReference {
        scope: CollaborationScope {
            spool: Uuid::from_u128(1),
            thread: Some(ContentHash::from_bytes([2; 32])),
        },
        source: CollaborationSourceAnchor {
            revision: CollaborationRevision::GitCommit {
                oid: "a".repeat(40),
            },
            path: path.into(),
            symbol_id: String::new(),
            start_line: Some(start),
            end_line: Some(end),
        },
    }
}
fn query(predicate: AnnotationTagPredicate) -> AnnotationQuery {
    AnnotationQuery {
        all: vec![predicate],
        ..Default::default()
    }
}
#[test]
fn structured_tags_preserve_exact_types_and_reference_coordinates() {
    let tags = vec![
        "freeform".into(),
        AnnotationTag::Symbol {
            name: "authorize".into(),
            target: Some(source("src/auth.rs", 3, 9)),
        },
        AnnotationTag::Property {
            key: "confidence".into(),
            value: AnnotationValue::Decimal(AnnotationDecimal {
                coefficient: 9,
                scale: 1,
            }),
        },
        AnnotationTag::Property {
            key: "requires_review".into(),
            value: AnnotationValue::Boolean(false),
        },
        AnnotationTag::Property {
            key: "large".into(),
            value: AnnotationValue::Integer(i64::MAX),
        },
    ];
    validate_annotation_tags(&tags).expect("bounded structured tags");
    let encoded = rmp_serde::to_vec_named(&tags).expect("encode");
    let restored: Vec<AnnotationTag> = rmp_serde::from_slice(&encoded).expect("decode");
    assert_eq!(restored, tags);
    assert!(
        query(AnnotationTagPredicate::SymbolName {
            name: "authorize".into()
        })
        .matches(&restored)
    );
    assert!(
        !query(AnnotationTagPredicate::Exact {
            tag: "authorize".into()
        })
        .matches(&restored)
    );
}
#[test]
fn property_filters_are_typed_exact_and_explicit_about_missing_values() {
    let tags = vec![AnnotationTag::Property {
        key: "confidence".into(),
        value: AnnotationValue::Decimal(AnnotationDecimal {
            coefficient: 91,
            scale: 2,
        }),
    }];
    let p = |key: &str, comparison, value| AnnotationTagPredicate::Property {
        key: key.into(),
        comparison,
        value,
    };
    let q = query(p(
        "confidence",
        AnnotationComparison::Greater,
        Some(AnnotationValue::Decimal(AnnotationDecimal {
            coefficient: 9,
            scale: 1,
        })),
    ));
    q.validate().expect("numeric query");
    assert!(q.matches(&tags));
    assert!(
        !query(p(
            "confidence",
            AnnotationComparison::Greater,
            Some(AnnotationValue::Integer(0))
        ))
        .matches(&tags)
    );
    assert!(
        !query(p(
            "missing",
            AnnotationComparison::Equal,
            Some(AnnotationValue::Boolean(false))
        ))
        .matches(&tags)
    );
    assert!(query(p("confidence", AnnotationComparison::Exists, None)).matches(&tags));
    assert!(
        query(p(
            "confidence",
            AnnotationComparison::Less,
            Some(AnnotationValue::Text("x".into()))
        ))
        .validate()
        .is_err()
    );
    let mut q = AnnotationQuery {
        any: vec![
            p("missing", AnnotationComparison::Exists, None),
            p("confidence", AnnotationComparison::Exists, None),
        ],
        ..Default::default()
    };
    assert!(q.matches(&tags));
    q.none
        .push(p("confidence", AnnotationComparison::Exists, None));
    assert!(!q.matches(&tags));
    let extremes = [
        AnnotationDecimal {
            coefficient: i64::MIN,
            scale: 0,
        },
        AnnotationDecimal {
            coefficient: i64::MAX,
            scale: 9,
        },
    ];
    assert!(extremes[0].scaled() < extremes[1].scaled());
}
#[test]
fn source_filters_never_mix_tags_revisions_or_path_components() {
    let tags = vec![
        AnnotationTag::Source {
            target: source("src/auth.rs", 30, 40),
        },
        AnnotationTag::Source {
            target: source("other.rs", 3, 9),
        },
    ];
    assert!(
        !query(AnnotationTagPredicate::LinesOverlap {
            target: source("src/auth.rs", 3, 9)
        })
        .matches(&tags)
    );
    assert!(
        query(AnnotationTagPredicate::LinesOverlap {
            target: source("src/auth.rs", 40, 45)
        })
        .matches(&tags)
    );
    let mut different = source("src/auth.rs", 30, 40);
    different.source.revision = CollaborationRevision::GitCommit {
        oid: "b".repeat(40),
    };
    assert!(!query(AnnotationTagPredicate::LinesOverlap { target: different }).matches(&tags));
    assert!(query(AnnotationTagPredicate::FilePrefix { path: "src".into() }).matches(&tags));
    assert!(!query(AnnotationTagPredicate::FilePrefix { path: "sr".into() }).matches(&tags));
}
#[test]
fn invalid_properties_coordinates_and_query_bounds_are_rejected() {
    for value in [
        AnnotationDecimal {
            coefficient: 90,
            scale: 2,
        },
        AnnotationDecimal {
            coefficient: 0,
            scale: 1,
        },
        AnnotationDecimal {
            coefficient: 1,
            scale: 10,
        },
    ] {
        assert!(value.validate().is_err());
    }
    let tag = AnnotationTag::Property {
        key: "severity".into(),
        value: AnnotationValue::Text("high".into()),
    };
    assert!(validate_annotation_tags(&[tag.clone(), tag]).is_err());
    for path in [
        "/absolute",
        "../parent",
        "src/../escape",
        "src//x",
        "src\\x",
        "C:/x",
    ] {
        assert!(source(path, 1, 2).validate().is_err(), "{path}");
    }
    assert!(source("src/x", 0, 2).validate().is_err());
    assert!(source("src/x", 3, 2).validate().is_err());
    let mut half = source("src/x", 1, 2);
    half.source.end_line = None;
    assert!(half.validate().is_err());
    let q = AnnotationQuery {
        all: vec![AnnotationTagPredicate::SymbolName { name: "x".into() }; 65],
        ..Default::default()
    };
    assert!(q.validate().is_err());
}
