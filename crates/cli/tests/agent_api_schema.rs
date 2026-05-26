// SPDX-License-Identifier: Apache-2.0
//! Stable JSON schema snapshot for the `heddle agent` API.
//!
//! The agent API is the contract orchestrators and harnesses speak to
//! Heddle. Any change to its on-the-wire shape — added field, removed
//! field, retyped value, renamed kind — would silently break those
//! callers. This test re-derives the schema from the live structs at
//! every build and diffs it against the committed snapshot at
//! `tests/snapshots/agent_api_schema.json`. A drift fails the test;
//! the only way to update the snapshot is to set
//! `HEDDLE_BLESS_AGENT_API_SCHEMA=1` and rerun, then commit the diff.

use std::path::PathBuf;

#[test]
fn agent_api_schema_matches_committed_snapshot() {
    let actual = cli::cli::commands::agent_api_schema();
    let actual_pretty = serde_json::to_string_pretty(&actual).unwrap();

    let snapshot_path = snapshot_path();
    if std::env::var_os("HEDDLE_BLESS_AGENT_API_SCHEMA").is_some() {
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(&snapshot_path, format!("{actual_pretty}\n")).unwrap();
        eprintln!(
            "Updated agent API schema snapshot at {}",
            snapshot_path.display()
        );
        return;
    }

    let expected = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|err| {
        panic!(
            "Missing snapshot at {} ({err}). Run with HEDDLE_BLESS_AGENT_API_SCHEMA=1 to bless.",
            snapshot_path.display()
        )
    });

    let expected_trimmed = expected.trim_end();
    let actual_trimmed = actual_pretty.trim_end();
    assert_eq!(
        actual_trimmed, expected_trimmed,
        "agent API schema drift detected; review the change and re-bless with \
         HEDDLE_BLESS_AGENT_API_SCHEMA=1 cargo test agent_api_schema_matches_committed_snapshot \
         once the new shape is intentional"
    );
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join("agent_api_schema.json")
}
