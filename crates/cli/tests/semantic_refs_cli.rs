// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage for `heddle semantic refs`.

use std::process::Command;

use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn run_heddle(repo: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(repo.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run heddle")
}

#[test]
fn semantic_refs_time_travels_without_reparse() {
    let temp = TempDir::new().expect("temp repo");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/api.rs"),
        "pub fn greet() -> u8 { 1 }\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/client.rs"),
        "use crate::api::greet;\npub fn run() { greet(); }\n",
    )
    .unwrap();
    let first = repo
        .snapshot(Some("first".into()), None)
        .expect("first state");
    let first_id = first.state_id.to_string_full();

    std::fs::write(
        temp.path().join("src/api.rs"),
        "pub fn greet() -> u8 { 2 }\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/extra.rs"),
        "pub fn extra() { crate::api::greet(); }\n",
    )
    .unwrap();
    let second = repo
        .snapshot(Some("second".into()), None)
        .expect("second state");
    let second_id = second.state_id.to_string_full();

    let old = run_heddle(
        &temp,
        &[
            "--output",
            "json",
            "semantic",
            "refs",
            "--at",
            &first_id,
            "src/api.rs:greet",
        ],
    );
    assert!(
        old.status.success(),
        "old-state refs failed: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let old_value: Value = serde_json::from_slice(&old.stdout).expect("valid JSON");
    assert_eq!(old_value["output_kind"], "semantic_refs");
    assert_eq!(old_value["kind"], "refs_of");
    assert_eq!(old_value["index_present"], true);
    let old_paths: Vec<&str> = old_value["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["source_path"].as_str().unwrap())
        .collect();
    assert_eq!(old_paths, ["src/client.rs"]);

    let old_importers = run_heddle(
        &temp,
        &[
            "--output",
            "json",
            "semantic",
            "refs",
            "--at",
            &first_id,
            "--importers",
            "src/api.rs",
        ],
    );
    assert!(old_importers.status.success());
    let importer_value: Value = serde_json::from_slice(&old_importers.stdout).unwrap();
    assert_eq!(importer_value["kind"], "importers_of");
    assert_eq!(importer_value["importers"], ["src/client.rs"]);

    let new_importers = run_heddle(
        &temp,
        &[
            "--output",
            "json",
            "semantic",
            "refs",
            "--at",
            &second_id,
            "--importers",
            "src/api.rs",
        ],
    );
    assert!(new_importers.status.success());
    let new_value: Value = serde_json::from_slice(&new_importers.stdout).unwrap();
    let new_paths: Vec<&str> = new_value["importers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap())
        .collect();
    assert!(
        new_paths.contains(&"src/extra.rs"),
        "later state should see the new importer: {new_value}"
    );
}
