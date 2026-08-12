// SPDX-License-Identifier: Apache-2.0

use super::{Blob, ConflictError, ConflictRange, ConflictRegion, ConflictSide, StateId};
use crate::object::StructuredConflict;

fn side(state: u8, body: &[u8], start: usize, end: usize) -> ConflictSide {
    ConflictSide::new(
        StateId::from_bytes([state; 32]),
        Some(Blob::from_slice(body).hash()),
        ConflictRange::new(start, end).unwrap(),
        body,
    )
    .unwrap()
}

fn sample_conflict() -> ConflictRegion {
    ConflictRegion::new(
        "src/lib.rs",
        Some("merge_target".into()),
        0,
        ConflictRange::new(8, 13).unwrap(),
        side(1, b"zero\nbase\ntail\n", 1, 2),
        side(2, b"zero\nours\ntail\n", 1, 2),
        side(3, b"zero\ntheirs\ntail\n", 1, 2),
    )
    .unwrap()
}

#[test]
fn three_way_conflict_roundtrip_is_lossless_and_verifiable() {
    let payload = StructuredConflict::new(vec![sample_conflict()]);
    let bytes = payload.encode().unwrap();
    let blob_id = Blob::from_slice(&bytes).hash();
    let decoded = StructuredConflict::decode(&bytes).unwrap();
    assert_eq!(payload, decoded);
    assert_eq!(blob_id, Blob::new(decoded.encode().unwrap()).hash());
    decoded.conflicts[0]
        .ours
        .verify_blob(b"zero\nours\ntail\n")
        .unwrap();
}

#[test]
fn stable_id_survives_unrelated_line_shifts_and_blob_rewrites() {
    let first = sample_conflict();
    let shifted = ConflictRegion::new(
        "src/lib.rs",
        Some("merge_target".into()),
        0,
        ConflictRange::new(20, 25).unwrap(),
        side(4, b"new\nzero\nbase\ntail\n", 2, 3),
        side(5, b"new\nzero\nours\ntail\n", 2, 3),
        side(6, b"new\nzero\ntheirs\ntail\n", 2, 3),
    )
    .unwrap();
    assert_eq!(first.id, shifted.id);
}

#[test]
fn tampered_hunk_fails_closed() {
    let conflict = sample_conflict();
    assert!(matches!(
        conflict.ours.verify_blob(b"zero\nother\ntail\n"),
        Err(ConflictError::BlobHashMismatch)
    ));
    let mut tampered = conflict;
    tampered.ours.hunk_hash = crate::object::ContentHash::compute(b"tampered");
    assert!(matches!(
        tampered.validate(),
        Err(ConflictError::IdMismatch { .. })
    ));
}

#[test]
fn legacy_format_is_rejected_at_the_version_gate() {
    let payload = StructuredConflict {
        format_version: 1,
        conflicts: vec![],
    };
    let bytes = payload.encode().unwrap();
    assert!(matches!(
        StructuredConflict::decode(&bytes),
        Err(ConflictError::UnsupportedVersion(1))
    ));
}
