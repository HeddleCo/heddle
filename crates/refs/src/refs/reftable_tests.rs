// SPDX-License-Identifier: Apache-2.0
//! Red-commit tests for the reftable spike (HeddleCo/heddle#21).
//!
//! These pin the contract of [`super::reftable_model::ReftableModel`]:
//! roundtrip, sorted iteration, presence/absence lookup, removal, and the
//! cold-lookup helper that binary-searches against serialized bytes without
//! parsing the whole payload.

use objects::object::ChangeId;

use super::reftable_model::{FOOTER_LEN, HEADER_LEN, MAGIC, ReftableModel};

fn cid(n: u8) -> ChangeId {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    ChangeId::from_bytes(bytes)
}

#[test]
fn roundtrip_empty() {
    let model = ReftableModel::new();
    let bytes = model.to_bytes();
    assert!(
        bytes.len() >= HEADER_LEN + FOOTER_LEN,
        "empty reftable must still carry header + footer"
    );
    let restored = ReftableModel::from_bytes(&bytes).expect("decode empty reftable");
    assert!(restored.is_empty());
}

#[test]
fn roundtrip_with_threads_and_markers() {
    let mut model = ReftableModel::new();
    model.set_thread("main", cid(1));
    model.set_thread("alpha", cid(2));
    model.set_thread("zeta", cid(3));
    model.set_marker("v1", cid(10));
    model.set_marker("v2", cid(11));

    let bytes = model.to_bytes();
    let restored = ReftableModel::from_bytes(&bytes).expect("decode populated reftable");

    assert_eq!(restored.thread_count(), 3);
    assert_eq!(restored.marker_count(), 2);
    assert_eq!(restored.get_thread("main"), Some(cid(1)));
    assert_eq!(restored.get_thread("alpha"), Some(cid(2)));
    assert_eq!(restored.get_thread("zeta"), Some(cid(3)));
    assert_eq!(restored.get_marker("v1"), Some(cid(10)));
    assert_eq!(restored.get_marker("v2"), Some(cid(11)));
}

#[test]
fn get_returns_inserted_thread() {
    let mut model = ReftableModel::new();
    model.set_thread("feature/x", cid(7));
    assert_eq!(model.get_thread("feature/x"), Some(cid(7)));
}

#[test]
fn get_returns_inserted_marker() {
    let mut model = ReftableModel::new();
    model.set_marker("release/1.0", cid(8));
    assert_eq!(model.get_marker("release/1.0"), Some(cid(8)));
}

#[test]
fn get_missing_returns_none() {
    let model = ReftableModel::new();
    assert_eq!(model.get_thread("nope"), None);
    assert_eq!(model.get_marker("nope"), None);
}

#[test]
fn set_thread_overwrites_existing_value() {
    let mut model = ReftableModel::new();
    model.set_thread("main", cid(1));
    model.set_thread("main", cid(2));
    assert_eq!(model.get_thread("main"), Some(cid(2)));
    assert_eq!(model.thread_count(), 1);
}

#[test]
fn list_threads_returns_sorted_names() {
    let mut model = ReftableModel::new();
    model.set_thread("zeta", cid(1));
    model.set_thread("alpha", cid(2));
    model.set_thread("main", cid(3));
    assert_eq!(
        model.list_threads(),
        vec!["alpha".to_string(), "main".to_string(), "zeta".to_string()]
    );
}

#[test]
fn list_markers_returns_sorted_names() {
    let mut model = ReftableModel::new();
    model.set_marker("v2", cid(1));
    model.set_marker("v1", cid(2));
    assert_eq!(
        model.list_markers(),
        vec!["v1".to_string(), "v2".to_string()]
    );
}

#[test]
fn remove_thread_then_get_returns_none() {
    let mut model = ReftableModel::new();
    model.set_thread("main", cid(1));
    assert_eq!(model.remove_thread("main"), Some(cid(1)));
    assert_eq!(model.get_thread("main"), None);
    assert_eq!(model.thread_count(), 0);
}

#[test]
fn remove_marker_then_get_returns_none() {
    let mut model = ReftableModel::new();
    model.set_marker("v1", cid(1));
    assert_eq!(model.remove_marker("v1"), Some(cid(1)));
    assert_eq!(model.get_marker("v1"), None);
}

#[test]
fn magic_bytes_present_in_header_and_footer() {
    let model = ReftableModel::new();
    let bytes = model.to_bytes();
    assert_eq!(&bytes[..MAGIC.len()], MAGIC, "header magic");
    let footer_start = bytes.len() - MAGIC.len();
    assert_eq!(&bytes[footer_start..], MAGIC, "footer magic");
}

#[test]
fn from_bytes_rejects_missing_magic() {
    let mut bytes = ReftableModel::new().to_bytes();
    bytes[0] = b'X';
    let err = ReftableModel::from_bytes(&bytes).expect_err("must reject bad magic");
    let msg = err.to_string();
    assert!(msg.contains("magic"), "error mentions magic: {msg}");
}

#[test]
fn cold_lookup_thread_in_bytes_matches_get() {
    let mut model = ReftableModel::new();
    for i in 0..32u8 {
        model.set_thread(&format!("branch-{i:02}"), cid(i));
    }
    let bytes = model.to_bytes();

    for i in 0..32u8 {
        let name = format!("branch-{i:02}");
        let cold = ReftableModel::lookup_thread_in_bytes(&bytes, &name).expect("cold lookup ok");
        assert_eq!(cold, Some(cid(i)), "cold lookup matches set for {name}");
    }

    let absent =
        ReftableModel::lookup_thread_in_bytes(&bytes, "branch-99").expect("cold lookup ok");
    assert_eq!(absent, None, "absent name returns None");
}

#[test]
fn cold_lookup_marker_in_bytes_matches_get() {
    let mut model = ReftableModel::new();
    model.set_marker("v1", cid(1));
    model.set_marker("v2", cid(2));
    model.set_marker("v3", cid(3));
    let bytes = model.to_bytes();

    assert_eq!(
        ReftableModel::lookup_marker_in_bytes(&bytes, "v2").unwrap(),
        Some(cid(2))
    );
    assert_eq!(
        ReftableModel::lookup_marker_in_bytes(&bytes, "v9").unwrap(),
        None
    );
}
