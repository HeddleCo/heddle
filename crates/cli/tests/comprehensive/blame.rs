// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_blame_large_file_performance() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let mut content = String::new();
    for i in 0..1000 {
        content.push_str(&format!("Line {}: some content here\n", i));
    }
    fs::write(temp.path().join("large.txt"), content).unwrap();
    heddle(&["capture", "-m", "Large file"], Some(temp.path())).unwrap();

    assert_performance(
        "blame large file",
        || {
            let _ = heddle(&["query", "--attribution", "large.txt"], Some(temp.path()));
        },
        performance_budget(Duration::from_secs(2), Duration::from_secs(4)),
    );
}

#[test]
fn test_blame_binary_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let binary_content: Vec<u8> = (0..256).map(|i| i as u8).collect();
    fs::write(temp.path().join("binary.bin"), binary_content).unwrap();
    heddle(&["capture", "-m", "Binary"], Some(temp.path())).unwrap();

    let result = heddle(&["query", "--attribution", "binary.bin"], Some(temp.path()));
    assert!(result.is_ok() || result.unwrap_err().contains("binary"));
}

#[test]
fn test_blame_nonexistent_file() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(
        &["query", "--attribution", "nonexistent.txt"],
        Some(temp.path()),
    );
    assert!(result.is_err(), "blame of nonexistent file should fail");
}

#[test]
fn test_blame_empty_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("empty.txt"), "").unwrap();
    heddle(&["capture", "-m", "Empty"], Some(temp.path())).unwrap();

    let result = heddle(&["query", "--attribution", "empty.txt"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "blame of empty file should succeed: {:?}",
        result.err()
    );
}

#[test]
fn test_blame_json_attribution_is_structured() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\n").unwrap();
    heddle(&["capture", "-m", "First line"], Some(temp.path())).unwrap();

    let output = heddle(
        &["--output", "json", "query", "--attribution", "tracked.txt"],
        Some(temp.path()),
    )
    .expect("query --attribution --output json should succeed");
    let v: Value = serde_json::from_str(&output).expect("blame should emit JSON");

    let line = &v["lines"][0];
    // No flattened `author` string — attribution is structured like
    // `log` / `show` so consumers don't have to string-parse.
    assert!(
        line.get("author").is_none(),
        "blame line should not carry a flattened `author` string: {output}"
    );
    assert_eq!(line["principal"]["name"], "Heddle Test");
    assert_eq!(line["principal"]["email"], "test@heddle.dev");
    // Agent is either absent (`null`) or a structured object — never a
    // string baked into the principal field.
    let agent = &line["agent"];
    assert!(
        agent.is_null() || agent.is_object(),
        "blame agent must be structured or null, got: {agent}"
    );

    // Origins mirror the same structured shape.
    let origin = &line["origins"][0];
    assert!(
        origin.get("author").is_none(),
        "blame origin should not carry a flattened `author` string: {output}"
    );
    assert_eq!(origin["principal"]["name"], "Heddle Test");
    assert_eq!(origin["principal"]["email"], "test@heddle.dev");
}

#[test]
fn test_blame_json_agent_is_structured_object() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\n").unwrap();
    heddle(
        &[
            "capture",
            "-m",
            "Agent line",
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-opus-4-7",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "json", "query", "--attribution", "tracked.txt"],
        Some(temp.path()),
    )
    .expect("query --attribution --output json should succeed");
    let v: Value = serde_json::from_str(&output).expect("blame should emit JSON");

    let agent = &v["lines"][0]["agent"];
    assert_eq!(
        agent["provider"], "anthropic",
        "blame should expose the agent provider as a field: {output}"
    );
    assert_eq!(
        agent["model"], "claude-opus-4-7",
        "blame should expose the agent model as a field: {output}"
    );
}

#[test]
fn test_blame_multiple_commits_attribution() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\n").unwrap();
    heddle(&["capture", "-m", "First line"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\nline 2\n").unwrap();
    heddle(&["capture", "-m", "Second line"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\nmodified 2\n").unwrap();
    heddle(&["capture", "-m", "Modified second"], Some(temp.path())).unwrap();

    let result = heddle(
        &["query", "--attribution", "tracked.txt"],
        Some(temp.path()),
    );
    assert!(result.is_ok(), "blame failed: {:?}", result.err());

    let output = result.unwrap();
    assert!(output.contains("line 1") || output.contains("modified 2") || output.contains("hd-"));
}
