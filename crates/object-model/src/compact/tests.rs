// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    decode_blob_frame, decode_state_frame, decode_tree_frame, encode_blob_frame,
    encode_state_frame, encode_tree_frame,
};
use crate::object::{
    Agent, Attribution, ChangeId, ChangeLineage, ChangeLineageKind, ContentHash, Principal,
    SpoolId, State, StateId, Status, Tree, TreeEntry, Verification,
};

#[test]
fn tree_frame_round_trips_every_entry_kind_and_native_bytes() {
    let content = ContentHash::from_bytes([1; 32]);
    let nested = ContentHash::from_bytes([2; 32]);
    let spool_state = StateId::from_bytes([3; 32]);
    let sha1 = GitObjectId::from_raw(GitObjectFormat::Sha1, &[4; 20]).unwrap();
    let sha256 = GitObjectId::from_raw(GitObjectFormat::Sha256, &[5; 32]).unwrap();
    let trees = vec![
        Tree::from_entries(vec![
            TreeEntry::file("a", content, false).unwrap(),
            TreeEntry::file("b", content, true).unwrap(),
            TreeEntry::directory("c", nested).unwrap(),
            TreeEntry::symlink("d", content).unwrap(),
            TreeEntry::gitlink("e", sha1).unwrap(),
            TreeEntry::gitlink("f", sha256).unwrap(),
            TreeEntry::spoollink("g", SpoolId::parse("acme/child").unwrap(), spool_state).unwrap(),
        ]),
        Tree::from_entries(vec![TreeEntry::file("same", content, false).unwrap()]),
    ];

    let encoded = encode_tree_frame(&trees).unwrap();
    let decoded = decode_tree_frame(&encoded).unwrap();

    assert_eq!(decoded, trees);
    for (actual, expected) in decoded.iter().zip(&trees) {
        assert_eq!(actual.hash(), expected.hash());
        assert_eq!(
            rmp_serde::to_vec_named(actual).unwrap(),
            rmp_serde::to_vec_named(expected).unwrap()
        );
    }
}

#[test]
fn state_frame_round_trips_every_fidelity_field_and_recomputes_id() {
    let principal = Principal::new("Author", "author@example.com");
    let committer = Principal::new("Committer", "committer@example.com");
    let agent = Agent::new("openai", "gpt-test")
        .with_session("session", "segment")
        .with_policy("policy");
    let mut custom = BTreeMap::new();
    custom.insert(
        "nested".to_string(),
        serde_json::json!({"bytes": [1, 2, 3]}),
    );
    let mut first = State::new(
        ContentHash::from_bytes([10; 32]),
        vec![StateId::from_bytes([11; 32])],
        Attribution::with_agent(principal, agent),
    );
    first.change_id = ChangeId::from_bytes([12; 16]);
    first.intent = Some("intent".to_string());
    first.confidence = Some(f32::from_bits(0x7fc0_0123));
    first.created_at = Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap();
    first.authored_at = Some(Utc.timestamp_opt(1_699_999_900, 987_654_321).unwrap());
    first.verification = Some(Verification {
        tests_passed: Some(false),
        tests_failed: Some(7),
        coverage_pct: Some(92.5),
        coverage_delta: Some(f32::from_bits(0xffc0_0456)),
        lint_warnings: Some(3),
        custom,
    });
    first.status = Status::Published;
    first.provenance = Some(ContentHash::from_bytes([13; 32]));
    first.committer = Some(committer);
    first.authored_tz_offset = -7 * 3600;
    first.committer_tz_offset = 12 * 3600 + 45 * 60;
    first.raw_message = Some(b"raw\0message\xff".to_vec());
    first.git_lossy = true;
    first.extra_headers = vec![
        (b"x-custom".to_vec(), b"first\xff".to_vec()),
        (b"gpgsig".to_vec(), b"signed\n continuation".to_vec()),
    ];
    first.lineage = vec![ChangeLineage {
        kind: ChangeLineageKind::GitProjection,
        source_change: ChangeId::from_bytes([14; 16]),
        source_state: StateId::from_bytes([15; 32]),
    }];
    first.state_id = first.id();
    let mut second = State::new(
        ContentHash::from_bytes([20; 32]),
        vec![first.state_id],
        Attribution::human(Principal::new("Author", "author@example.com")),
    );
    second.created_at = Utc.timestamp_opt(1_700_000_001, 0).unwrap();
    second.state_id = second.id();
    let states = vec![first, second];

    let encoded = encode_state_frame(&states).unwrap();
    let decoded = decode_state_frame(&encoded).unwrap();

    for (actual, expected) in decoded.iter().zip(&states) {
        assert_eq!(actual.id(), expected.id());
        assert_eq!(
            rmp_serde::to_vec_named(actual).unwrap(),
            rmp_serde::to_vec_named(expected).unwrap()
        );
    }
    assert_eq!(decoded[0].confidence.unwrap().to_bits(), 0x7fc0_0123);
    assert_eq!(
        decoded[0]
            .verification
            .as_ref()
            .unwrap()
            .coverage_delta
            .unwrap()
            .to_bits(),
        0xffc0_0456
    );
}

#[test]
fn corrupt_frame_byte_rejects_every_contained_object() {
    let hash = ContentHash::from_bytes([21; 32]);
    let trees = vec![
        Tree::from_entries(vec![TreeEntry::file("a", hash, false).unwrap()]),
        Tree::from_entries(vec![TreeEntry::file("b", hash, true).unwrap()]),
    ];
    let mut encoded = encode_tree_frame(&trees).unwrap();
    let corrupt_at = encoded.len() / 2;
    encoded[corrupt_at] ^= 0x01;

    for _ in &trees {
        let error = decode_tree_frame(&encoded).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
    }
}

#[test]
fn blob_frame_round_trips_offsets_lengths_and_typed_hashes() {
    let bodies = [b"newest body".as_slice(), b"older body".as_slice(), &[]];
    let encoded = encode_blob_frame(&bodies).unwrap();
    let decoded = decode_blob_frame(&encoded).unwrap();

    assert_eq!(decoded.len(), bodies.len());
    for ((hash, actual), expected) in decoded.iter().zip(bodies) {
        assert_eq!(*actual, expected);
        assert_eq!(*hash, ContentHash::compute_typed("blob", expected));
    }
}

#[test]
fn corrupt_blob_frame_byte_rejects_every_contained_object() {
    let bodies = [b"first".as_slice(), b"second".as_slice()];
    let mut encoded = encode_blob_frame(&bodies).unwrap();
    let corrupt_at = encoded.len() / 2;
    encoded[corrupt_at] ^= 0x01;

    for _ in bodies {
        let error = decode_blob_frame(&encoded).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
    }
}
