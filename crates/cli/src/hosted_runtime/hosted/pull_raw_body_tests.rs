// SPDX-License-Identifier: Apache-2.0

use wire::{MAX_RECEIVED_PACK_SIZE, ProtocolError};

use super::{admit_declared_received_len, receive_pull_raw_chunks};

#[test]
fn huge_declared_raw_body_is_rejected_before_any_reserve() {
    // Call the admit gate only: the pre-fix receive path still reserve()s the
    // declared usize, and exercising that with MAX+1 would abort the process.
    let error = admit_declared_received_len(
        MAX_RECEIVED_PACK_SIZE + 1,
        MAX_RECEIVED_PACK_SIZE,
        "pull raw body",
    )
    .expect_err("huge declared length must fail closed before reserve");
    assert!(
        error.to_string().contains("exceeds receive size limit"),
        "got {error}"
    );
}

#[test]
fn declared_length_under_ceiling_grows_from_chunks_only() {
    let mut buf = Vec::new();
    let declared = 8 * 1024 * 1024;
    let error = receive_pull_raw_chunks(
        &mut buf,
        declared,
        MAX_RECEIVED_PACK_SIZE,
        [b"abcd".as_slice()],
    )
    .expect_err("short body must fail after receive");
    assert!(
        matches!(error, ProtocolError::InvalidState(message) if message.contains("length changed")),
        "got {error}"
    );
    assert_eq!(buf, b"abcd");
    assert!(
        buf.capacity() < declared as usize,
        "must grow from received chunks, not reserve(declared); capacity={}",
        buf.capacity()
    );
}

#[test]
fn received_chunks_matching_declared_length_are_accepted() {
    let mut buf = Vec::new();
    receive_pull_raw_chunks(
        &mut buf,
        4,
        MAX_RECEIVED_PACK_SIZE,
        [b"ab".as_slice(), b"cd".as_slice()],
    )
    .unwrap();
    assert_eq!(buf, b"abcd");
    assert!(
        buf.capacity() < 64 * 1024,
        "small body must not reserve a large declared-independent slab, capacity={}",
        buf.capacity()
    );
}

#[test]
fn received_bytes_over_declared_length_are_rejected_before_append() {
    let mut buf = Vec::new();
    let error = receive_pull_raw_chunks(&mut buf, 3, MAX_RECEIVED_PACK_SIZE, [b"abcd".as_slice()])
        .unwrap_err();
    assert!(
        buf.is_empty(),
        "overflow chunk must not be appended, buf={buf:?}"
    );
    assert!(
        error
            .to_string()
            .contains("exceeded declared length during receive"),
        "got {error}"
    );
}

#[test]
fn admit_helper_is_the_shared_declared_length_gate() {
    admit_declared_received_len(0, MAX_RECEIVED_PACK_SIZE, "pull raw body").unwrap();
    admit_declared_received_len(
        MAX_RECEIVED_PACK_SIZE,
        MAX_RECEIVED_PACK_SIZE,
        "pull raw body",
    )
    .unwrap();
}
