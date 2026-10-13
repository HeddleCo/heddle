#![cfg(feature = "replication")]
use heddle_thread_api::{
    collaboration,
    contract::{EntityRef, SignedRecord},
};
use prost::Message;
use serde_json::Value;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/collaboration-alpha47.json"))
        .expect("shared api fixtures")
}
#[test]
fn shared_signed_records_and_typed_mentions_preserve_api_bytes() {
    for (name, vector) in fixtures()["vectors"].as_object().expect("vectors") {
        let bytes = hex::decode(vector["record_wire_hex"].as_str().expect("wire")).expect("hex");
        let signed = SignedRecord::decode(bytes.as_slice()).expect("signed record");
        let operation = collaboration::verify(&signed).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            operation.encode().expect("encode"),
            signed.canonical_record,
            "{name}"
        );
        assert_eq!(signed.encode_to_vec(), bytes, "{name}");
        for mention in vector["mentions"].as_array().expect("mentions") {
            let bytes =
                hex::decode(mention["entity_wire_hex"].as_str().expect("entity")).expect("hex");
            let entity = EntityRef::decode(bytes.as_slice()).expect("entity wire");
            let native = collaboration::mention(&entity).expect("native mention");
            assert_eq!(collaboration::mention_ref(&native), entity, "{name}");
            assert_eq!(
                rmp_serde::to_vec_named(&native).expect("native encode"),
                hex::decode(mention["canonical_hex"].as_str().expect("canonical")).expect("hex"),
                "{name}"
            );
            assert_eq!(
                api::v2::collaboration::canonical_mention(&entity).expect("api encode"),
                rmp_serde::to_vec_named(&native).expect("encode")
            );
        }
        println!("PASS alpha.47 {name}: signature, protobuf and typed mention parity");
    }
}
#[test]
fn source_conversion_refuses_missing_revision_mismatched_scope_and_invalid_lines() {
    use heddle_thread_api::contract::{
        RevisionRef, SourceMention, SpoolRef, ThreadId, ThreadRef, entity_ref::Entity,
    };
    let source = SourceMention {
        revision: Some(RevisionRef {
            spool: Some(SpoolRef {
                id: uuid::Uuid::from_u128(1).to_string(),
            }),
            revision: Some(
                heddle_thread_api::contract::revision_ref::Revision::GitCommitOid("a".repeat(40)),
            ),
        }),
        path: "src/main.rs".into(),
        ..Default::default()
    };
    let convert = |source| {
        collaboration::mention(&EntityRef {
            entity: Some(Entity::Source(source)),
        })
    };
    convert(source.clone()).expect("valid source");
    let mut invalid = source.clone();
    invalid.revision = None;
    assert!(convert(invalid).is_err());
    let mut invalid = source.clone();
    invalid.start_line = Some(1);
    assert!(convert(invalid).is_err());
    let mut invalid = source.clone();
    invalid.path = "../private".into();
    assert!(convert(invalid).is_err());
    let mut invalid = source;
    invalid.thread = Some(ThreadRef {
        spool: Some(SpoolRef {
            id: uuid::Uuid::from_u128(2).to_string(),
        }),
        id: Some(ThreadId { value: vec![2; 32] }),
    });
    assert!(convert(invalid).is_err());
}
