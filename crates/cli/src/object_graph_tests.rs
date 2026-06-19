// SPDX-License-Identifier: Apache-2.0
use std::fs;

use objects::object::ChangeId;
use repo::Repository;
use tempfile::TempDir;
use wire::{ObjectId, StateClosureOptions, enumerate_state_closure_with_options};

fn create_repo_with_two_states() -> (TempDir, Repository, ChangeId, ChangeId) {
    let temp_dir = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp_dir.path()).expect("init repo");

    fs::write(temp_dir.path().join("a.txt"), "one").expect("write file");
    let state1 = repo
        .snapshot(Some("one".to_string()), None)
        .expect("snapshot one");

    fs::write(temp_dir.path().join("a.txt"), "two").expect("write file");
    let state2 = repo
        .snapshot(Some("two".to_string()), None)
        .expect("snapshot two");

    (temp_dir, repo, state1.change_id, state2.change_id)
}

#[test]
fn test_enumerate_with_depth() {
    let (_temp_dir, repo, parent, child) = create_repo_with_two_states();

    let options = StateClosureOptions {
        depth: Some(0),
        exclude_states: Vec::new(),
    };

    let objects = enumerate_state_closure_with_options(repo.store(), child, options).unwrap();

    let states: Vec<_> = objects
        .iter()
        .filter_map(|obj| match obj.id {
            ObjectId::ChangeId(id) => Some(id),
            _ => None,
        })
        .collect();

    assert!(states.contains(&child));
    assert!(!states.contains(&parent));
}

#[test]
fn test_enumerate_with_excludes() {
    let (_temp_dir, repo, parent, child) = create_repo_with_two_states();

    let options = StateClosureOptions {
        depth: None,
        exclude_states: vec![parent],
    };

    let objects = enumerate_state_closure_with_options(repo.store(), child, options).unwrap();

    let states: Vec<_> = objects
        .iter()
        .filter_map(|obj| match obj.id {
            ObjectId::ChangeId(id) => Some(id),
            _ => None,
        })
        .collect();

    assert!(states.contains(&child));
    assert!(!states.contains(&parent));
}
