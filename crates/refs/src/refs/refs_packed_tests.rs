// SPDX-License-Identifier: Apache-2.0
use objects::{error::HeddleError, object::ChangeId};
use tempfile::TempDir;

use super::*;

fn create_ref_manager() -> (TempDir, RefManager) {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();
    let refs = RefManager::new(&heddle_dir);
    refs.init().unwrap();
    (temp_dir, refs)
}

#[test]
fn test_get_thread_from_packed_refs() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("cold-branch", &id).unwrap();
    refs.pack_refs().unwrap();
    let loose = refs.root.join("refs/threads/cold-branch");
    assert!(!loose.exists(), "pack_refs should delete loose file");
    assert_eq!(refs.get_thread("cold-branch").unwrap(), Some(id));
}
#[test]
fn test_loose_overrides_packed_refs() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread("main", &id1).unwrap();
    refs.pack_refs().unwrap();
    refs.set_thread("main", &id2).unwrap();
    assert_eq!(refs.get_thread("main").unwrap(), Some(id2));
}
#[test]
fn test_pack_refs_consolidates_loose() {
    let (_temp, refs) = create_ref_manager();
    let ids: Vec<ChangeId> = (0..5).map(|_| ChangeId::generate()).collect();
    for (i, id) in ids.iter().enumerate() {
        refs.set_thread(&format!("branch-{}", i), id).unwrap();
    }
    refs.pack_refs().unwrap();
    let packed_path = refs.packed_refs_path();
    assert!(packed_path.exists(), "packed-refs file should exist");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            refs.get_thread(&format!("branch-{}", i)).unwrap(),
            Some(*id)
        );
    }
}
#[test]
fn test_list_threads_includes_packed() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("packed-branch", &id).unwrap();
    refs.pack_refs().unwrap();
    let threads = refs.list_threads().unwrap();
    assert!(threads.contains(&"packed-branch".to_string()));
}
#[test]
fn test_delete_thread_removes_from_packed() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("to-delete", &id).unwrap();
    refs.pack_refs().unwrap();
    refs.delete_thread("to-delete").unwrap();
    assert_eq!(refs.get_thread("to-delete").unwrap(), None);
}
#[test]
fn test_packed_refs_format() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("format-test", &id).unwrap();
    refs.pack_refs().unwrap();
    let packed_path = refs.packed_refs_path();
    let contents = std::fs::read_to_string(&packed_path).unwrap();
    assert!(contents.contains("refs/threads/format-test"));
    assert!(contents.contains(&id.to_string_full()));
}
#[test]
fn test_markers_in_packed_refs() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.create_marker("v1.0.0", &id).unwrap();
    refs.pack_refs().unwrap();
    let loose = refs.root.join("refs/markers/v1.0.0");
    assert!(!loose.exists(), "pack_refs should delete loose marker file");
    assert_eq!(refs.get_marker("v1.0.0").unwrap(), Some(id));
}
#[test]
fn test_delete_thread_cas_removes_packed_entry() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("packed-thread", &id).unwrap();
    refs.pack_refs().unwrap();
    refs.delete_thread_cas("packed-thread", RefExpectation::Value(id))
        .unwrap();
    assert_eq!(refs.get_thread("packed-thread").unwrap(), None);
}
#[test]
fn test_delete_thread_cas_packed_conflict() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread("packed-thread", &id1).unwrap();
    refs.pack_refs().unwrap();
    let result = refs.delete_thread_cas("packed-thread", RefExpectation::Value(id2));
    assert!(matches!(result, Err(HeddleError::Conflict(_))));
    assert_eq!(refs.get_thread("packed-thread").unwrap(), Some(id1));
}

#[test]
fn test_ref_summary_index_reports_packed_entries_and_loose_overrides() {
    let (_temp, refs) = create_ref_manager();
    let packed_thread = ChangeId::generate();
    let packed_marker = ChangeId::generate();
    let loose_override = ChangeId::generate();

    refs.set_thread("release", &packed_thread).unwrap();
    refs.create_marker("v1.0.0", &packed_marker).unwrap();
    refs.pack_refs().unwrap();

    let packed_summary = refs.inspect_ref_summary_index().unwrap();
    assert!(packed_summary.present);
    assert!(packed_summary.valid);
    assert_eq!(packed_summary.threads, 1);
    assert_eq!(packed_summary.markers, 1);
    assert_eq!(packed_summary.packed_threads, 1);
    assert_eq!(packed_summary.packed_markers, 1);

    refs.set_thread("release", &loose_override).unwrap();
    let override_summary = refs.inspect_ref_summary_index().unwrap();
    assert!(override_summary.present);
    assert!(override_summary.valid);
    assert_eq!(override_summary.threads, 1);
    assert_eq!(override_summary.packed_threads, 1);
    assert_eq!(refs.list_threads().unwrap(), vec!["release".to_string()]);
    assert_eq!(refs.get_thread("release").unwrap(), Some(loose_override));
}