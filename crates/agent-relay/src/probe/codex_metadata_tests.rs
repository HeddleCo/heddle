// SPDX-License-Identifier: Apache-2.0
use super::*;

fn metadata(contents: &str) -> BTreeMap<String, String> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("synthetic-rollout.jsonl");
    std::fs::write(&path, contents).unwrap();
    read_codex_session_metadata(&path, "thread").unwrap()
}

#[test]
fn rejects_mismatched_session_and_pre_session_turns() {
    let before = r#"{"type":"turn_context","payload":{"model":"foreign"}}
{"type":"session_meta","payload":{"id":"thread"}}"#;
    assert!(!metadata(before).contains_key("model"));
    let mismatch = r#"{"type":"session_meta","payload":{"id":"other"}}
{"type":"turn_context","payload":{"model":"foreign"}}"#;
    assert!(metadata(mismatch).is_empty());
}

#[test]
fn resets_model_and_effort_at_sparse_new_turn() {
    let actual = metadata(
        r#"{"type":"session_meta","payload":{"id":"thread","cli_version":"0.100.0","model_provider":"provider-a"}}
{"type":"turn_context","payload":{"turn_id":"one","model":"model-a","effort":"high"}}
{"type":"turn_context","payload":{"turn_id":"two"}}"#,
    );
    assert!(!actual.contains_key("model"));
    assert!(!actual.contains_key("model_reasoning_effort"));
    assert!(!actual.contains_key("model_source"));
    assert_eq!(actual.get("turn_id").map(String::as_str), Some("two"));
    assert_eq!(
        actual.get("cli_version").map(String::as_str),
        Some("0.100.0")
    );
}

#[test]
fn latest_matching_turn_has_transcript_provenance() {
    let actual = metadata(
        r#"{"type":"session_meta","payload":{"id":"thread","session_id":"root","parent_thread_id":"parent"}}
{"type":"turn_context","payload":{"turn_id":"one","model":"model-a"}}
{"type":"turn_context","payload":{"turn_id":"two","model":"model-b"}}"#,
    );
    assert_eq!(actual.get("model").map(String::as_str), Some("model-b"));
    assert_eq!(
        actual.get("model_source").map(String::as_str),
        Some("rollout_turn_context")
    );
    assert_eq!(
        actual.get("parent_thread_id").map(String::as_str),
        Some("parent")
    );
}
