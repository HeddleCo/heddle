//! Frozen shared heddle-api alpha.47 vectors, including the older signed corpus.
use heddle_object_model::object::{
    CollaborationMention as Mention, CollaborationOperationBodyV1 as Body,
    CollaborationOperationEnvelope, ContextRevision,
    thread_replication::{ThreadOperation, ThreadOperationBody},
};
use serde_json::Value;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/collaboration-alpha47.json"))
        .expect("shared vectors")
}
fn operation(name: &str) -> ThreadOperation {
    let bytes = hex::decode(
        fixtures()["vectors"][name]["canonical_hex"]
            .as_str()
            .expect("hex"),
    )
    .expect("canonical bytes");
    ThreadOperation::decode(&bytes).expect("native alpha.47 decode")
}
fn discussion(name: &str) -> CollaborationOperationEnvelope {
    let ThreadOperationBody::Discussion(bytes) = operation(name).body else {
        panic!("discussion vector");
    };
    CollaborationOperationEnvelope::decode(&bytes)
        .expect("discussion")
        .operation
}

#[test]
fn every_alpha47_vector_decodes_and_reencodes_byte_identically() {
    let f = fixtures();
    let vectors = f["vectors"].as_object().expect("vectors");
    assert_eq!(vectors.len(), 7);
    for (name, vector) in vectors {
        let canonical = hex::decode(vector["canonical_hex"].as_str().expect("hex")).expect("bytes");
        let mut op = ThreadOperation::decode(&canonical).unwrap_or_else(|e| panic!("{name}: {e}"));
        match &mut op.body {
            ThreadOperationBody::Discussion(bytes) => {
                let decoded = CollaborationOperationEnvelope::decode(bytes).expect("inner decode");
                assert_eq!(
                    decoded.operation.encode().expect("inner encode"),
                    *bytes,
                    "{name}"
                );
            }
            ThreadOperationBody::Context(bytes) => {
                assert_eq!(
                    ContextRevision::decode(bytes)
                        .expect("context")
                        .encode()
                        .expect("encode"),
                    *bytes,
                    "{name}"
                );
            }
            _ => panic!("collaboration vector"),
        }
        assert_eq!(op.encode().expect("outer encode"), canonical, "{name}");
        assert_eq!(
            op.id().expect("ID").to_hex(),
            vector["operation_id_hex"].as_str().expect("ID")
        );
        println!("PASS alpha.47 {name}: native decode and byte-identical re-encode");
    }
    println!("GATE alpha.47 native codec: 7 accepted, 0 rejected");
}

#[test]
fn existing_shared_signed_corpus_keeps_exact_bytes() {
    let mut count = 0;
    for line in include_str!("fixtures/collaboration-v2-rust.txt").lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let bytes = hex::decode(fields[2]).expect("fixture");
        let op = ThreadOperation::decode(&bytes).expect("existing decode");
        assert_eq!(op.encode().expect("encode"), bytes, "{}", fields[0]);
        assert_eq!(op.id().expect("ID").to_hex(), fields[4]);
        count += 1;
    }
    assert_eq!(count, 16);
    println!("GATE existing collaboration corpus: 16 byte-identical records");
}

fn rejects(op: CollaborationOperationEnvelope) {
    assert!(
        op.encode().is_err(),
        "encoder must reject invalid authored record"
    );
    let bytes = rmp_serde::to_vec_named(&op).expect("malformed input serialization");
    assert!(
        CollaborationOperationEnvelope::decode(&bytes).is_err(),
        "decoder must reject invalid record"
    );
}

#[test]
fn malformed_mentions_and_bounds_fail_on_encode_and_decode() {
    let original = discussion("source_state_open");
    let Mention::Source { target } =
        original.metadata.as_ref().expect("metadata").mentions[0].clone()
    else {
        panic!("source mention");
    };
    for path in [
        "",
        "../secret",
        "/absolute",
        "a//b",
        "a/./b",
        "a\\b",
        "C:/a",
        "a\nb",
        &"x".repeat(4097),
    ] {
        let mut op = original.clone();
        let mut target = target.clone();
        target.source.path = path.into();
        op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Source { target }];
        rejects(op);
    }
    for (start, end) in [
        (Some(0), Some(1)),
        (Some(2), Some(1)),
        (Some(1), None),
        (None, Some(1)),
    ] {
        let mut op = original.clone();
        let mut target = target.clone();
        target.source.start_line = start;
        target.source.end_line = end;
        op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Source { target }];
        rejects(op);
    }
    for oid in ["a".repeat(39), "A".repeat(40), "a".repeat(65)] {
        let mut op = original.clone();
        let mut invalid = target.clone();
        invalid.source.revision =
            heddle_object_model::object::CollaborationRevision::GitCommit { oid };
        op.metadata.as_mut().expect("metadata").mentions =
            vec![Mention::Source { target: invalid }];
        rejects(op);
    }
    let mut op = original.clone();
    op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Principal {
        principal_id: uuid::Uuid::nil(),
    }];
    rejects(op);
    let mut op = original.clone();
    let mut invalid = target.clone();
    invalid.scope.spool = uuid::Uuid::nil();
    op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Source { target: invalid }];
    rejects(op);
    let mut op = original.clone();
    let mut invalid = target.clone();
    invalid.source.symbol_id = "x".repeat(4097);
    op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Source { target: invalid }];
    rejects(op);
    let mut op = original.clone();
    op.metadata.as_mut().expect("metadata").mentions = vec![
        Mention::Source {
            target: target.clone()
        };
        129
    ];
    rejects(op);
    let mut op = original;
    let mut large = target;
    large.source.path = "x".repeat(4096);
    large.source.symbol_id = "y".repeat(4096);
    op.metadata.as_mut().expect("metadata").mentions = vec![Mention::Source { target: large }; 32];
    rejects(op);
}

#[test]
fn invalid_tags_and_parentless_edits_are_rejected() {
    for name in ["open_tags", "edit_tags"] {
        let mut op = discussion(name);
        let (Body::Open { tags, .. } | Body::EditTags { tags }) = &mut op.body else {
            panic!("tags")
        };
        *tags = vec!["x".into(); 129];
        rejects(op);
        let mut op = discussion(name);
        let (Body::Open { tags, .. } | Body::EditTags { tags }) = &mut op.body else {
            panic!("tags")
        };
        *tags = vec!["x".repeat(513).into()];
        rejects(op);
    }
    let mut op = discussion("clear_tags");
    op.parents.clear();
    rejects(op);
    let canonical = operation("principal_open").encode().expect("encode");
    for length in [0, 1, canonical.len() / 2, canonical.len() - 1] {
        assert!(ThreadOperation::decode(&canonical[..length]).is_err());
    }
}
