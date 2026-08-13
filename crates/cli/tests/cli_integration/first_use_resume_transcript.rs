// SPDX-License-Identifier: Apache-2.0
//! End-to-end ceremony gates for first use and resumed work.

use std::fs;

use tempfile::TempDir;

use super::{
    git_hermetic, heddle,
    transcript_harness::{DecisionBudget, SessionTranscript},
};

const FIRST_USE_BUDGET: DecisionBudget = DecisionBudget {
    max_commands: 3,
    max_choices: 0,
    max_total: 3,
};
const RESUME_BUDGET: DecisionBudget = DecisionBudget {
    max_commands: 2,
    max_choices: 0,
    max_total: 2,
};

fn plain_git_with_pending_work() -> TempDir {
    let repo = TempDir::new().expect("create first-use fixture");
    git_hermetic(&["init", "-b", "main"], repo.path());
    git_hermetic(&["config", "user.name", "Transcript Test"], repo.path());
    git_hermetic(
        &["config", "user.email", "transcript@example.com"],
        repo.path(),
    );
    fs::write(repo.path().join("tracked.txt"), "seed\n").unwrap();
    git_hermetic(&["add", "tracked.txt"], repo.path());
    git_hermetic(&["commit", "-m", "seed"], repo.path());
    fs::write(repo.path().join("pending.txt"), "first useful work\n").unwrap();
    repo
}

fn record_first_use(extra_command: bool) -> SessionTranscript {
    let repo = plain_git_with_pending_work();
    let mut transcript = SessionTranscript::new("first use", repo.path());

    let status = transcript.run(&["status", "--output", "json"]).json();
    assert_eq!(status["repository_capability"], "plain-git");
    assert_eq!(status["recommended_action"], "heddle init");

    let init = transcript.run(&["init", "--output", "json"]).json();
    assert_eq!(init["repository_mode"], "git-overlay");
    assert_eq!(init["recommended_action"], "heddle capture -m \"...\"");

    if extra_command {
        transcript.run(&["help"]).assert_success();
    }

    let saved = transcript
        .run(&[
            "capture",
            "-m",
            "save first useful work",
            "--output",
            "json",
        ])
        .json();
    assert_eq!(saved["output_kind"], "capture");
    assert_eq!(saved["status"], "captured");
    transcript
}

fn resumed_repo() -> (TempDir, std::path::PathBuf) {
    let repo = TempDir::new().expect("create resume fixture");
    heddle(&["init"], Some(repo.path())).expect("initialize resume fixture");
    fs::write(repo.path().join("work.txt"), "before pause\n").unwrap();
    heddle(&["capture", "-m", "save before pause"], Some(repo.path()))
        .expect("save resume baseline");

    let nested = repo.path().join("src/feature");
    fs::create_dir_all(&nested).unwrap();
    fs::write(repo.path().join("work.txt"), "continued after pause\n").unwrap();
    (repo, nested)
}

fn record_resume() -> SessionTranscript {
    let (_repo, nested) = resumed_repo();
    let mut transcript = SessionTranscript::new("resume from nested directory", &nested);

    let status = transcript.run(&["status", "--output", "json"]).json();
    assert_eq!(status["thread"], "main");
    assert_eq!(status["changed_path_count"], 1);
    assert_eq!(status["recommended_action"], "heddle capture -m \"...\"");

    let saved = transcript
        .run(&["capture", "-m", "continue resumed work", "--output", "json"])
        .json();
    assert_eq!(saved["output_kind"], "capture");
    assert_eq!(saved["status"], "captured");
    transcript
}

#[test]
fn first_use_transcript_stays_within_decision_budget() {
    let extra_command =
        std::env::var("HEDDLE_TRANSCRIPT_NEGATIVE_CONTROL").as_deref() == Ok("extra-command");
    let transcript = record_first_use(extra_command);
    println!("{transcript}");
    transcript.assert_budget(FIRST_USE_BUDGET);
}

#[test]
fn resume_transcript_stays_within_decision_budget() {
    let transcript = record_resume();
    println!("{transcript}");
    transcript.assert_budget(RESUME_BUDGET);
}

#[test]
fn first_use_budget_rejects_one_extra_command() {
    let transcript = record_first_use(true);
    let error = transcript
        .check_budget(FIRST_USE_BUDGET)
        .expect_err("one extra command must trip the first-use ceremony gate");
    assert_eq!(error.observed().commands, 4);
    assert_eq!(error.observed().choices, 0);
    assert_eq!(error.observed().total(), 4);
    assert!(
        error
            .to_string()
            .contains("observed commands=4, choices=0, total=4")
    );
}
