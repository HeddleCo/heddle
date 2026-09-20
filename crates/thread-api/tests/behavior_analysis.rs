#![cfg(feature = "semantic-analysis")]

use heddle_thread_api::{behavior::BehaviorAnalyzer, contract as api};
use prost::Message;

fn revision(oid: &str) -> api::RevisionRef {
    api::RevisionRef {
        spool: Some(api::SpoolRef {
            id: "review-spool".into(),
        }),
        revision: Some(api::revision_ref::Revision::GitCommitOid(oid.into())),
    }
}

#[test]
fn real_revision_comparison_round_trips_as_renderable_typed_facts() {
    let base = revision("92a9b0705370d2c04f8bcaf9b7141937085d0ad9");
    let source = revision("2a67bcaf7446def0bd01e4b9b79b2cbb9e203401");
    let analyzer = BehaviorAnalyzer::new(base.clone(), source.clone()).expect("exact comparison");
    let base_text = include_str!("../../semantic/tests/fixtures/behavior/capture-base.rs");
    let source_text = include_str!("../../semantic/tests/fixtures/behavior/capture-source.rs");
    let result = analyzer.compare(
        "crates/verbs/src/save.rs",
        Some(base_text),
        Some(source_text),
        &["capture".into()],
    );
    let change = result
        .changes
        .iter()
        .find(|c| {
            c.assignments
                .iter()
                .any(|a| a.target == "SavePlan.git_scope")
        })
        .expect("real change");
    let event = api::AnalysisEvent {
        payload: Some(api::analysis_event::Payload::BehaviorChange(change.clone())),
        ..Default::default()
    };
    let decoded =
        api::AnalysisEvent::decode(event.encode_to_vec().as_slice()).expect("protobuf round trip");
    assert_eq!(decoded, event);
    assert_eq!(change.base.as_ref(), Some(&base));
    assert_eq!(change.source.as_ref(), Some(&source));
    let expression = |id: &str| {
        change
            .expressions
            .iter()
            .find(|e| e.id == id)
            .expect("in-record expression")
    };
    let condition = change
        .expressions
        .iter()
        .find_map(|e| e.conditional.as_ref())
        .expect("conditional");
    assert_eq!(condition.branches.len(), 2);
    assert_eq!(
        condition.branches[0].label,
        api::behavior_branch::Label::True as i32
    );
    assert_eq!(
        condition.branches[1].label,
        api::behavior_branch::Label::False as i32
    );
    let origin = &change.correspondences[0].base_ids[0];
    assert!(
        change
            .correspondences
            .iter()
            .all(|c| c.base_ids == [origin.clone()])
    );
    assert_eq!(
        expression(origin).source.as_ref().expect("source").text,
        "GitScope::None"
    );
    for e in &change.expressions {
        let r = e.source.as_ref().expect("source reference");
        let text = if r.revision.as_ref() == Some(&base) {
            base_text
        } else {
            assert_eq!(r.revision.as_ref(), Some(&source));
            source_text
        };
        assert_eq!(&text[r.start_byte as usize..r.end_byte as usize], r.text);
        assert_eq!(r.blob_hash.len(), 32);
        assert_eq!(e.provenance, api::BehaviorProvenance::Syntax as i32);
    }
    assert!(
        change
            .bindings
            .iter()
            .all(|b| b.provenance == api::BehaviorProvenance::BindingResolution as i32)
    );
    assert!(
        change
            .correspondences
            .iter()
            .all(|c| c.provenance == api::BehaviorProvenance::StructuralComparison as i32)
    );
    assert!(!result.coverage.omitted.is_empty());
    let deps = change.dependencies.as_ref().expect("cache dependencies");
    assert_ne!(deps.base_extraction, deps.source_extraction);
    let moved = analyzer.compare(
        "crates/verbs/src/save.rs",
        Some(base_text),
        Some(&format!("// move\n{source_text}")),
        &["capture".into()],
    );
    let moved = moved
        .changes
        .iter()
        .find(|c| {
            c.assignments
                .iter()
                .any(|a| a.target == "SavePlan.git_scope")
        })
        .expect("moved map");
    assert_ne!(change.id, moved.id);
    assert_ne!(
        deps.comparison,
        moved
            .dependencies
            .as_ref()
            .expect("new dependencies")
            .comparison
    );
}

#[test]
fn unavailable_is_distinct_from_analyzed_empty_and_identity_binds_exact_revisions() {
    let base = revision(&"a".repeat(40));
    let source = revision(&"b".repeat(40));
    let analyzer = BehaviorAnalyzer::new(base.clone(), source.clone()).expect("exact revisions");
    let missing = analyzer.compare("save.rs", None, Some("fn f() {}"), &[]);
    assert_eq!(
        missing.coverage.omitted[0].support,
        api::BehaviorSupport::Unavailable as i32
    );
    let empty = analyzer.compare("save.rs", Some("fn f() {}"), Some("fn f() {}"), &[]);
    assert!(empty.changes.is_empty());
    assert!(empty.coverage.selection_exhausted);
    assert!(empty.coverage.omitted.is_empty());
    assert!(!empty.coverage.analyzed.is_empty());
    assert_ne!(
        analyzer.analysis,
        BehaviorAnalyzer::new(source, base)
            .expect("reverse")
            .analysis
    );
    assert!(BehaviorAnalyzer::new(revision("main"), revision(&"b".repeat(40))).is_err());
}
