// SPDX-License-Identifier: Apache-2.0
use super::*;

struct SubmoduleStatusSnapshot {
    text: String,
    json: Value,
    submodules: Vec<(String, String)>,
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "Heddle Test")
        .env("GIT_AUTHOR_EMAIL", "heddle@example.com")
        .env("GIT_COMMITTER_NAME", "Heddle Test")
        .env("GIT_COMMITTER_EMAIL", "heddle@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("failed to run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn init_git_repo(path: &Path) {
    git(path, &["init", "-q", "--initial-branch=main"]);
}

fn capture_status_snapshot(paths: &[&str]) -> SubmoduleStatusSnapshot {
    let superproject = TempDir::new().expect("superproject tempdir");
    init_git_repo(superproject.path());
    std::fs::write(superproject.path().join("README.md"), "superproject\n")
        .expect("write superproject file");
    git(superproject.path(), &["add", "README.md"]);
    git(superproject.path(), &["commit", "-q", "-m", "base"]);

    let mut sources = Vec::new();
    let mut submodules = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let source = TempDir::new().expect("submodule source tempdir");
        init_git_repo(source.path());
        std::fs::write(
            source.path().join("lib.txt"),
            format!("submodule {index}\n"),
        )
        .expect("write submodule file");
        git(source.path(), &["add", "lib.txt"]);
        git(source.path(), &["commit", "-q", "-m", "submodule base"]);
        let commit = git(source.path(), &["rev-parse", "HEAD"]);
        git(
            superproject.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.path().to_str().expect("UTF-8 source path"),
                path,
            ],
        );
        sources.push(source);
        submodules.push(((*path).to_string(), commit));
    }
    if !submodules.is_empty() {
        git(
            superproject.path(),
            &["commit", "-q", "-am", "add submodules"],
        );
    }

    heddle(&["init"], Some(superproject.path())).expect("initialize Heddle overlay");
    let text =
        heddle(&["status", "--no-color"], Some(superproject.path())).expect("render text status");
    let json = heddle(
        &["status", "--output", "json", "--no-color"],
        Some(superproject.path()),
    )
    .expect("render JSON status");

    SubmoduleStatusSnapshot {
        text,
        json: serde_json::from_str(&json).expect("status JSON parses"),
        submodules,
    }
}

#[test]
fn status_reports_one_submodule() {
    let snapshot = capture_status_snapshot(&["vendor/library"]);
    let (path, commit) = &snapshot.submodules[0];

    assert!(
        snapshot.text.contains(&format!(
            "\nSubmodules\n  submodule {path} @ {}\n",
            &commit[..12]
        )),
        "text status should contain a dedicated submodule section:\n{}",
        snapshot.text
    );
    assert_eq!(
        snapshot.json["submodules"],
        serde_json::json!([{"path": path, "commit": commit}]),
        "JSON status should expose the full pinned commit"
    );
}

#[test]
fn status_reports_multiple_submodules_in_path_order() {
    let snapshot = capture_status_snapshot(&["vendor/zeta", "deps/alpha"]);
    let mut expected = snapshot.submodules.clone();
    expected.sort_by(|left, right| left.0.cmp(&right.0));

    let expected_text = format!(
        "\nSubmodules\n{}\n",
        expected
            .iter()
            .map(|(path, commit)| format!("  submodule {path} @ {}", &commit[..12]))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        snapshot.text.contains(&expected_text),
        "text status should sort submodules by path:\n{}",
        snapshot.text
    );
    assert_eq!(
        snapshot.json["submodules"],
        Value::Array(
            expected
                .iter()
                .map(|(path, commit)| serde_json::json!({"path": path, "commit": commit}))
                .collect()
        ),
        "JSON status should sort submodules by path"
    );
}

#[test]
fn status_omits_text_section_and_emits_empty_json_list_without_submodules() {
    let snapshot = capture_status_snapshot(&[]);

    assert!(
        !snapshot.text.contains("\nSubmodules\n"),
        "text status should stay silent without submodules:\n{}",
        snapshot.text
    );
    assert_eq!(
        snapshot.json["submodules"],
        serde_json::json!([]),
        "JSON status should keep a stable empty submodule list"
    );
}
