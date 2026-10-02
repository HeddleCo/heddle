// SPDX-License-Identifier: Apache-2.0
use super::{
    decode_state_frame, encode_state_frame, extract_state,
    state::{STATE_MAGIC_V3, encode_state_frame_hcs1},
};
use crate::object::{Agent, Attribution, ChangeId, ContentHash, Principal, State};

fn fixture() -> State {
    State::new(
        ContentHash::from_bytes([17; 32]),
        vec![],
        Attribution::with_agent(
            Principal::new("Author", "author@example.com"),
            Agent::new("anthropic", "opus"),
        ),
    )
    .with_change_id(ChangeId::from_bytes([18; 16]))
    .with_timestamp(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())
}

const LEGACY_MSGPACK: &str = concat!(
    "de0012a96368616e67655f6964dc001012121212121212121212121212121212a474726565dc00201111111111",
    "111111111111111111111111111111111111111111111111111111a7706172656e747390ab6174747269627574",
    "696f6e82a97072696e636970616c82a46e616d65c406417574686f72a5656d61696cc412617574686f72406578",
    "616d706c652e636f6da56167656e7485a870726f7669646572a9616e7468726f706963a56d6f64656ca46f7075",
    "73aa73657373696f6e5f6964c0aa7365676d656e745f6964c0a9706f6c6963795f6964c0a6696e74656e74c0aa",
    "636f6e666964656e6365c0aa637265617465645f6174b4323032332d31312d31345432323a31333a32305aac76",
    "6572696669636174696f6ec0a6737461747573a54472616674aa70726f76656e616e6365c0ab617574686f7265",
    "645f6174c0a9636f6d6d6974746572c0b2617574686f7265645f747a5f6f666673657400b3636f6d6d69747465",
    "725f747a5f6f666673657400ab7261775f6d657373616765c0a96769745f6c6f737379c2ad65787472615f6865",
    "616465727390a76c696e6561676590",
);
const LEGACY_HCS2: &str = concat!(
    "48435332010106417574686f7212617574686f72406578616d706c652e636f6d0109616e7468726f706963046f",
    "707573000000000012121212121212121212121212121212111111111111111111111111111111111111111111",
    "11111111111111111111110000010000000080c49fd50c0000000000000000000087b40a4822638eccf7bbae15",
    "495872e598cdfb6079be1cefebeeb82ad66731fa",
);
const LEGACY_HCS1: &str = concat!(
    "48435331010106417574686f7212617574686f72406578616d706c652e636f6d0109616e7468726f706963046f",
    "707573000000121212121212121212121212121212121111111111111111111111111111111111111111111111",
    "1111111111111111110000010000000080c49fd50c00000000000000000000938d9ad16cb5a877a70ec95591a4",
    "1564ba7870c7d68dbd59fc54be035bb9abe4",
);

#[test]
fn attribution_compatibility_fixture_golden() {
    let state = fixture();
    assert_eq!(
        state.id().to_string_full(),
        "hs-vys0jbem68fzx4tyewebpfqydqeesvz4hzf7ke8azm5qmz3kw7eg"
    );
    assert_eq!(
        state.pre_cursor_id().to_string_full(),
        "hs-kqww23try80bqstzf55awenbtp4wzkvhzk18nys45pzm06jpv8t0"
    );
    assert_eq!(
        hex::encode(state.encode_current_msgpack().unwrap()),
        LEGACY_MSGPACK
    );
    assert_eq!(
        hex::encode(encode_state_frame(std::slice::from_ref(&state)).unwrap()),
        LEGACY_HCS2
    );
    assert_eq!(
        hex::encode(encode_state_frame_hcs1(std::slice::from_ref(&state)).unwrap()),
        LEGACY_HCS1
    );
    let decoded = State::decode_current_msgpack(&hex::decode(LEGACY_MSGPACK).unwrap()).unwrap();
    assert!(decoded.attribution_evidence.is_none());
    for frame in [LEGACY_HCS1, LEGACY_HCS2] {
        let decoded = extract_state(&hex::decode(frame).unwrap(), state.pre_cursor_id()).unwrap();
        assert_eq!(decoded.state_id, state.pre_cursor_id());
        assert!(decoded.attribution_evidence.is_none());
    }
}

#[test]
fn attribution_hash6_commits_evidence_and_cannot_validate_as_legacy() {
    let legacy = fixture();
    let with = legacy
        .clone()
        .with_attribution_evidence(ContentHash::from_bytes([19; 32]));
    let changed = legacy
        .clone()
        .with_attribution_evidence(ContentHash::from_bytes([20; 32]));
    assert_ne!(legacy.id(), with.id());
    assert_ne!(changed.id(), with.id());
    assert!(!with.accepts_stored_id(&legacy.id()));
    assert!(!with.accepts_stored_id(&legacy.pre_cursor_id()));
    assert!(!legacy.accepts_stored_id(&with.id()));
    assert!(!changed.accepts_stored_id(&with.id()));
    assert_eq!(with.hash_for_stored_id(&with.id()), with.compute_hash());
    assert_ne!(
        with.hash_for_stored_id(&legacy.pre_cursor_id()),
        legacy.hash_for_stored_id(&legacy.pre_cursor_id())
    );
    let bytes = with.encode_current_msgpack().unwrap();
    let decoded = State::decode_current_msgpack(&bytes).unwrap();
    assert_eq!(decoded.attribution_evidence, with.attribution_evidence);
    assert_eq!(decoded.id(), with.id());
}

#[test]
fn attribution_hcs3_mixed_batch_roundtrips_all_native_bytes_and_legacy_ids() {
    let legacy = fixture();
    let mut cursor = fixture();
    cursor.attribution.agent.as_mut().unwrap().thought_level = Some("high".into());
    cursor.attribution.agent.as_mut().unwrap().parent = Some("child".into());
    let with = cursor.with_attribution_evidence(ContentHash::from_bytes([19; 32]));
    let states = [legacy.clone(), with.clone()];
    let bytes = encode_state_frame(&states).unwrap();
    assert!(bytes.starts_with(STATE_MAGIC_V3));
    let decoded = decode_state_frame(&bytes).unwrap();
    for (actual, expected) in decoded.iter().zip(&states) {
        assert_eq!(
            actual.encode_current_msgpack().unwrap(),
            expected.encode_current_msgpack().unwrap()
        );
        assert_eq!(actual.id(), expected.id());
    }
    assert_eq!(
        extract_state(&bytes, legacy.pre_cursor_id())
            .unwrap()
            .state_id,
        legacy.pre_cursor_id()
    );
    assert_eq!(
        extract_state(&bytes, with.id())
            .unwrap()
            .attribution_evidence,
        with.attribution_evidence
    );
    // The mixed frame legitimately contains the legacy state with that old id.
    // Isolate the evidence-bearing object to prove it cannot impersonate it.
    let evidence_only = encode_state_frame(std::slice::from_ref(&with)).unwrap();
    assert!(extract_state(&evidence_only, with.pre_cursor_id()).is_err());
    assert!(encode_state_frame_hcs1(&[with]).is_err());
}

#[test]
fn attribution_only_state_is_agent_authored_without_inventing_a_model() {
    let mut state = fixture();
    state.attribution.agent = None;
    assert!(!state.is_agent_authored());
    let state = state.with_attribution_evidence(ContentHash::from_bytes([19; 32]));
    assert!(state.is_agent_authored());
    let decoded = decode_state_frame(&encode_state_frame(&[state.clone()]).unwrap()).unwrap();
    assert!(decoded[0].attribution.agent.is_none());
    assert_eq!(decoded[0].id(), state.id());
}

fn frame_with_checksum(mut bytes: Vec<u8>) -> Vec<u8> {
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

#[test]
fn attribution_hcs3_rejects_invalid_tags_absent_columns_and_downgrade() {
    let state = fixture().with_attribution_evidence(ContentHash::from_bytes([19; 32]));
    let bytes = encode_state_frame(&[state]).unwrap();
    let mut invalid_tag = bytes[..bytes.len() - 32].to_vec();
    let offset = invalid_tag.len() - 33;
    invalid_tag[offset] = 2;
    assert!(decode_state_frame(&frame_with_checksum(invalid_tag)).is_err());
    let mut missing = bytes[..bytes.len() - 65].to_vec();
    assert!(decode_state_frame(&frame_with_checksum(missing.clone())).is_err());
    missing.push(0);
    assert!(decode_state_frame(&frame_with_checksum(missing)).is_err());
    let mut downgrade = bytes[..bytes.len() - 32].to_vec();
    downgrade[3] = b'2';
    assert!(decode_state_frame(&frame_with_checksum(downgrade)).is_err());
}

#[test]
fn attribution_hash6_transcript_is_explicit_and_domain_separated() {
    // Independent transcript for fixture(): fixed framing is the durable
    // format contract, not serde output or a hash of the previous StateId.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&[18; 16]); // ChangeId
    legacy.extend_from_slice(&[17; 32]); // tree
    legacy.extend_from_slice(&0u32.to_le_bytes()); // parents
    legacy.extend_from_slice(b"Author\0author@example.com\0");
    legacy.push(1); // legacy Agent present
    legacy.extend_from_slice(b"anthropic\0opus\0");
    legacy.extend_from_slice(&[0; 5]); // session, segment, policy, effort, parent
    legacy.extend_from_slice(&[0; 2]); // intent and confidence
    legacy.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    legacy.extend_from_slice(&[0; 3]); // verification, provenance, draft status
    legacy.push(0); // committer
    legacy.extend_from_slice(&[0; 8]); // author and committer timezone offsets
    legacy.extend_from_slice(&[0; 2]); // authored time and raw message
    legacy.extend_from_slice(&[0; 8]); // extra-header and lineage counts
    assert_eq!(
        fixture().compute_hash(),
        ContentHash::compute_typed("state", &legacy)
    );

    let reference = ContentHash::from_bytes([19; 32]);
    let mut v6 = b"heddle-state-v6\0".to_vec();
    v6.extend_from_slice(&legacy);
    v6.extend_from_slice(reference.as_bytes());
    assert_eq!(
        fixture()
            .with_attribution_evidence(reference)
            .compute_hash(),
        ContentHash::compute_typed("state-v6", &v6)
    );
    assert_ne!(
        ContentHash::compute_typed("state", &v6),
        ContentHash::compute_typed("state-v6", &v6)
    );
}
