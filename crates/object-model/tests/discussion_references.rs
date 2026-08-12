// SPDX-License-Identifier: Apache-2.0

use heddle_object_model::object::{
    DiscussionReference, DiscussionReferenceKind, DiscussionTurn, Principal, StateId,
};
use serde::Serialize;

fn sample_principal() -> Principal {
    Principal::new("Alice", "alice@example.com")
}

#[test]
fn references_round_trip_with_utf8_byte_offsets() {
    let body = "é @user @agent @spool @thread @state @file @line @symbol".to_string();
    let state = StateId::from_bytes([9; 32]);
    let reference = |kind, id: &str, token: &str, at| {
        let start = body.find(token).unwrap();
        DiscussionReference {
            kind,
            id: id.to_string(),
            at,
            start: u32::try_from(start).unwrap(),
            end: u32::try_from(start + token.len()).unwrap(),
        }
    };
    let references = vec![
        reference(DiscussionReferenceKind::User, "user-1", "@user", None),
        reference(DiscussionReferenceKind::Agent, "agent-1", "@agent", None),
        reference(DiscussionReferenceKind::Spool, "spool-1", "@spool", None),
        reference(DiscussionReferenceKind::Thread, "thread-1", "@thread", None),
        reference(DiscussionReferenceKind::State, "state-1", "@state", None),
        reference(
            DiscussionReferenceKind::File,
            "src/lib.rs",
            "@file",
            Some(state),
        ),
        reference(
            DiscussionReferenceKind::Line,
            "src/lib.rs#88",
            "@line",
            Some(state),
        ),
        reference(
            DiscussionReferenceKind::Symbol,
            "src/lib.rs#run",
            "@symbol",
            Some(state),
        ),
    ];
    let turn = DiscussionTurn {
        author: sample_principal(),
        body,
        posted_at: 1_700_000_001,
        references,
    };

    let bytes = rmp_serde::to_vec_named(&turn).unwrap();
    let decoded: DiscussionTurn = rmp_serde::from_slice(&bytes).unwrap();

    assert_eq!(decoded, turn);
    assert_eq!(decoded.references[2].at, None);
    assert_eq!(decoded.references[6].at, Some(state));
    assert_eq!(decoded.references[0].start, 3);
    for (reference, token) in decoded.references.iter().zip([
        "@user", "@agent", "@spool", "@thread", "@state", "@file", "@line", "@symbol",
    ]) {
        let span = reference.start as usize..reference.end as usize;
        assert_eq!(decoded.body.get(span), Some(token));
    }
}

#[test]
fn old_turn_shape_decodes_with_empty_references() {
    #[derive(Serialize)]
    struct OldDiscussionTurn {
        author: Principal,
        body: String,
        posted_at: i64,
    }

    let old_turn = OldDiscussionTurn {
        author: sample_principal(),
        body: "stored before discussion references".into(),
        posted_at: 1_700_000_000,
    };
    let bytes = rmp_serde::to_vec_named(&old_turn).unwrap();
    let decoded: DiscussionTurn = rmp_serde::from_slice(&bytes).unwrap();

    assert!(decoded.references.is_empty());
    assert_eq!(decoded.body, old_turn.body);
}
