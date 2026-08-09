// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage for the stored-index symbol diff CLI.

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
fn semantic_diff_reports_added_removed_modified_and_moved_symbols() {
    let temp = TempDir::new().expect("temp repo");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    std::fs::write(
        temp.path().join("lib.rs"),
        "fn changed() -> i32 { 1 }\nfn removed() {}\nfn moved() -> i32 { 7 }\n",
    )
    .expect("write first state");
    let first = repo
        .snapshot(Some("first".into()), None)
        .expect("first state");

    std::fs::write(
        temp.path().join("lib.rs"),
        "fn changed() -> i32 { 2 }\nfn added() {}\n",
    )
    .expect("write second state");
    std::fs::write(temp.path().join("moved.rs"), "fn moved() -> i32 { 7 }\n")
        .expect("move symbol to second file");
    let second = repo
        .snapshot(Some("second".into()), None)
        .expect("second state");
    let first_id = first.state_id.to_string_full();
    let second_id = second.state_id.to_string_full();

    let json = run_heddle(
        &temp,
        &[
            "--output", "json", "semantic", "diff", &first_id, &second_id,
        ],
    );
    assert!(
        json.status.success(),
        "semantic diff JSON failed: {}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: Value = serde_json::from_slice(&json.stdout).expect("valid JSON output");
    assert_eq!(value["output_kind"], "semantic_diff");
    assert_eq!(value["from_state"], first_id);
    assert_eq!(value["to_state"], second_id);
    let deltas = value["deltas"].as_array().expect("deltas array");
    assert_eq!(deltas.len(), 5, "unexpected deltas: {value}");
    for (symbol, change, old_present, new_present) in [
        ("changed", "modified", true, true),
        ("removed", "removed", true, false),
        ("added", "added", false, true),
    ] {
        let delta = deltas
            .iter()
            .find(|delta| delta["anchor"]["symbol"] == symbol)
            .unwrap_or_else(|| panic!("missing {symbol} delta: {value}"));
        assert_eq!(delta["change"], change);
        assert_eq!(delta["anchor"]["file"], "lib.rs");
        assert_eq!(delta["kind"], "function");
        assert_eq!(delta["old_hash"].is_string(), old_present);
        assert_eq!(delta["new_hash"].is_string(), new_present);
    }
    let moved_from = deltas
        .iter()
        .find(|delta| delta["anchor"]["file"] == "lib.rs" && delta["anchor"]["symbol"] == "moved")
        .unwrap_or_else(|| panic!("missing moved-symbol removal: {value}"));
    let moved_to = deltas
        .iter()
        .find(|delta| delta["anchor"]["file"] == "moved.rs" && delta["anchor"]["symbol"] == "moved")
        .unwrap_or_else(|| panic!("missing moved-symbol addition: {value}"));
    assert_eq!(moved_from["change"], "removed");
    assert_eq!(moved_to["change"], "added");
    assert_eq!(moved_from["old_hash"], moved_to["new_hash"]);

    let human = run_heddle(&temp, &["semantic", "diff", &first_id, &second_id]);
    assert!(
        human.status.success(),
        "semantic diff text failed: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8(human.stdout).expect("UTF-8 text output");
    assert!(
        stdout.contains("~ modified   lib.rs::changed (function)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("- removed    lib.rs::removed (function)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ added      lib.rs::added (function)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("- removed    lib.rs::moved (function)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("+ added      moved.rs::moved (function)"),
        "{stdout}"
    );
}
