// SPDX-License-Identifier: Apache-2.0
//! Field-study coverage for heddle#1215: text vocabulary matches the
//! capture contract, `save` suggests `capture`, and `query --verb capture`
//! finds captures.

use super::*;

#[test]
fn save_suggests_capture() {
    let output = heddle_output(&["save", "-m", "field study"], None).expect("invoke save");
    assert_eq!(
        output.status.code(),
        Some(64),
        "unknown subcommand is Usage (64); stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized subcommand 'save'") && stderr.contains("capture"),
        "heddle save should name capture as the near-miss: {stderr}"
    );
}

#[test]
fn status_log_and_capture_text_use_capture_vocabulary() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let empty_log = heddle(&["--output", "text", "log"], Some(temp.path())).unwrap();
    assert!(
        empty_log.contains("genesis")
            && empty_log.contains("heddle init")
            && empty_log.contains("heddle show"),
        "log after init must name the omitted genesis state: {empty_log}"
    );

    let clean = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        clean.contains("Nothing to capture, worktree clean"),
        "clean status should use nothing-to-capture language: {clean}"
    );
    assert!(
        !clean.contains("unsaved") && !clean.contains("Saved change"),
        "clean status should not teach save/unsaved: {clean}"
    );

    std::fs::write(temp.path().join("note.txt"), "field study\n").unwrap();
    let captured = heddle(&["capture", "-m", "field-study capture"], Some(temp.path())).unwrap();
    assert!(
        captured.contains("Captured by:"),
        "capture text should say Captured by: {captured}"
    );
    assert!(
        !captured.contains("Saved by:"),
        "capture text should not say Saved by: {captured}"
    );

    let status = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        status.contains("Captured state:"),
        "status should label the current state as captured: {status}"
    );
    assert!(
        !status.contains("Saved change:"),
        "status should not say Saved change: {status}"
    );

    let log = heddle(&["--output", "text", "log"], Some(temp.path())).unwrap();
    assert!(
        log.contains("field-study capture"),
        "log should list the capture: {log}"
    );
    assert!(
        log.contains("genesis") && log.contains("heddle init") && log.contains("heddle show"),
        "log must name the omitted init root with a visible reason: {log}"
    );
}

#[test]
fn query_verb_capture_finds_captures_case_insensitively() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("note.txt"), "query slice\n").unwrap();
    heddle(&["capture", "-m", "query slice capture"], Some(temp.path())).unwrap();

    for verb in ["capture", "Capture"] {
        let text = heddle(&["query", "--verb", verb], Some(temp.path()))
            .unwrap_or_else(|err| panic!("query --verb {verb} should find captures: {err}"));
        assert!(
            !text.contains("(no matches)"),
            "query --verb {verb} should not be empty: {text}"
        );
        assert!(
            text.contains("capture"),
            "query text should show the public capture verb: {text}"
        );
        assert!(
            text.contains("heddle@example.com"),
            "query should surface the capture principal email: {text}"
        );
    }

    let json = heddle(
        &["query", "--verb", "capture", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["output_kind"], "query");
    let hits = report["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|hit| hit["verb"] == "snapshot"),
        "JSON keeps the stored snapshot verb: {report}"
    );
    assert!(
        hits.iter().any(|hit| {
            hit["verb"] == "snapshot" && hit["actor_email"].as_str() == Some("heddle@example.com")
        }),
        "JSON actor_email should come from the captured state: {report}"
    );
    assert!(
        !json.contains("\"save\"") && !json.contains("\"saved\""),
        "query JSON must not grow save/saved keys: {json}"
    );
}
