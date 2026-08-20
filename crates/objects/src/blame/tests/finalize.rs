// SPDX-License-Identifier: Apache-2.0
use chrono::{TimeZone, Utc};

use crate::blame::{BlameSliceError, OriginRange, finalize_file_provenance};
use crate::object::{Attribution, ContentHash, Origin, Principal, StateId};

fn origin(name: &str, byte: u8) -> Origin {
    Origin {
        state_id: StateId::from_bytes([byte; 32]),
        attribution: Attribution::human(Principal::new(name, format!("{name}@example.com"))),
        created_at: Utc.timestamp_opt(0, 0).unwrap(),
        authored_at: None,
    }
}

#[test]
fn empty_file_rejects_nonzero_len() {
    let err = finalize_file_provenance(
        ContentHash::compute(b""),
        0,
        [OriginRange {
            target_start: 0,
            len: 1,
            origin: origin("alice", 1),
        }],
    )
    .unwrap_err();
    assert!(matches!(err, BlameSliceError::InvalidCoverage));
}

#[test]
fn empty_file_rejects_nonzero_start() {
    let err = finalize_file_provenance(
        ContentHash::compute(b""),
        0,
        [OriginRange {
            target_start: 3,
            len: 0,
            origin: origin("alice", 1),
        }],
    )
    .unwrap_err();
    assert!(matches!(err, BlameSliceError::InvalidCoverage));
}

#[test]
fn alice_bob_alice_reuses_two_origin_sets() {
    let alice = origin("alice", 1);
    let bob = origin("bob", 2);
    let provenance = finalize_file_provenance(
        ContentHash::compute(b"a\nb\na\n"),
        3,
        [
            OriginRange {
                target_start: 0,
                len: 1,
                origin: alice.clone(),
            },
            OriginRange {
                target_start: 1,
                len: 1,
                origin: bob,
            },
            OriginRange {
                target_start: 2,
                len: 1,
                origin: alice,
            },
        ],
    )
    .unwrap();
    assert_eq!(provenance.origins.len(), 2);
    assert_eq!(provenance.origin_sets.len(), 2);
    assert_eq!(
        provenance.spans[0].origin_set_index,
        provenance.spans[2].origin_set_index
    );
    assert_ne!(
        provenance.spans[0].origin_set_index,
        provenance.spans[1].origin_set_index
    );
}
