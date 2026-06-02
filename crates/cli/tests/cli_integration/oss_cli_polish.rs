// SPDX-License-Identifier: Apache-2.0
use clap::CommandFactory;
use cli::cli::Cli;
use repo::operation_dedup::{OperationDedupStore, hash_request_body};

use super::*;

#[test]
fn git_overlay_guide_is_concise_and_actionable() {
    let help = heddle(&["help", "git-overlay"], None).unwrap();
    assert!(
        help.contains("Git-overlay quick start")
            && help.contains("heddle adopt")
            && help.contains("heddle commit -m")
            && help.contains("heddle merge <name> --preview"),
        "help git-overlay should render the actual guide, not only clap usage: {help}"
    );

    let output = heddle(&["--output", "text", "git-overlay"], None).unwrap();

    assert!(
        output.contains("Git-overlay quick start"),
        "guide should have a clear title: {output}"
    );
    assert!(
        output.contains("heddle adopt"),
        "guide should teach one-command adoption: {output}"
    );
    assert!(
        output.contains("Worktree has unsaved edits")
            && output.contains("Captured in Heddle but not Git")
            && output.contains("Git refs changed externally"),
        "guide should name concrete recovery states instead of vague Git/Heddle disagreement: {output}"
    );
    assert!(
        output.contains("heddle start <name> --path ../<name>"),
        "guide should teach isolated threads with the real start argument name: {output}"
    );
    assert!(
        output.contains("heddle merge <name> --preview"),
        "guide should teach preview before landing: {output}"
    );
    assert!(
        output.contains("heddle undo"),
        "guide should make recovery part of the core loop: {output}"
    );
    assert!(
        output.contains("heddle verify"),
        "guide should end on the proof surface: {output}"
    );
}

#[test]
fn model_help_topic_gives_short_first_time_mental_model() {
    let help = heddle(&["help", "model"], None).expect("model help topic should render");
    assert!(
        help.contains("Heddle mental model")
            && help.contains("State:")
            && help.contains("Thread:")
            && help.contains("Capture:")
            && help.contains("Commit:")
            && help.contains("Verify:")
            && help.contains("heddle merge <name> --preview")
            && help.contains("heddle adopt"),
        "model topic should explain the everyday concepts without the long thread manual: {help}"
    );
    assert!(
        !help.contains("# Workspace modes"),
        "model topic should stay concise; detailed thread mechanics belong in `heddle help threads`: {help}"
    );
}

#[test]
fn bridge_help_topic_teaches_adoption_before_export_notes() {
    let help = heddle(&["help", "bridge"], None).expect("bridge help topic should render");
    assert!(
        help.starts_with("Git bridge"),
        "bridge topic should open with the workflow, not advanced notes metadata: {help}"
    );
    for needle in [
        "heddle status",
        "heddle adopt",
        "heddle init",
        "heddle bridge git import --ref <branch>",
        "heddle verify",
        "heddle commit -m",
        "heddle push",
        "heddle merge <name> --preview",
        "heddle ship --thread <name> --no-push",
        "heddle bridge git reconcile --ref <branch> --preview",
        "Export metadata for Git readers",
    ] {
        assert!(
            help.contains(needle),
            "bridge topic should include `{needle}`: {help}"
        );
    }
    assert!(
        help.find("First run:") < help.find("Export metadata for Git readers"),
        "bridge topic should put adoption before notes/export details: {help}"
    );
    assert!(
        !help.contains("\n    heddle ship --push\n"),
        "bridge topic should not teach a threadless ship from the main checkout: {help}"
    );
}

#[test]
fn import_alias_leads_to_adopt_instead_of_clap_guesswork() {
    let help = heddle(&["import", "--help"], None).expect("import alias help should render");
    assert!(
        help.contains("Adopt the current Git repository into Heddle")
            && help.contains("heddle adopt"),
        "`heddle import --help` should route first-run import intent to adopt, not suggest an unrelated command: {help}"
    );
}

#[test]
fn adopt_help_does_not_claim_dirty_git_worktree_becomes_clean() {
    let help = heddle(&["adopt", "--help"], None).expect("adopt help should render");
    assert!(
        help.contains("without modifying existing Git worktree changes"),
        "adopt help should say adoption leaves existing dirty work untouched: {help}"
    );
    assert!(
        !help.contains("leaves the Git working tree clean"),
        "adopt help must not imply dirty Git worktrees are cleaned by adoption: {help}"
    );
}

#[test]
fn automation_discovery_aliases_accept_common_guesses() {
    let temp = TempDir::new().unwrap();
    let catalog = heddle(&["--output", "json", "catalog"], Some(temp.path()))
        .expect("catalog alias should render command catalog");
    let parsed: Value = serde_json::from_str(&catalog).expect("catalog alias emits JSON");
    assert_eq!(parsed["kind"], "command_catalog");

    let schema = heddle(&["--output", "json", "schema", "status"], Some(temp.path()))
        .expect("schema alias should render command schema");
    let parsed: Value = serde_json::from_str(&schema).expect("schema alias emits JSON");
    assert_eq!(parsed["title"], "StatusSchema");
}

#[test]
fn branch_delete_does_not_recommend_deleted_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    heddle(&["branch", "try-git-muscle"], Some(temp.path())).unwrap();

    let output = heddle(
        &["branch", "-d", "try-git-muscle", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Dropped thread 'try-git-muscle'"),
        "branch delete should confirm the removed thread: {output}"
    );
    assert!(
        !output.contains("ready --thread try-git-muscle"),
        "branch delete must not recommend an action for a deleted thread: {output}"
    );
}

#[test]
fn checkout_dash_b_guides_to_heddle_thread_flow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["checkout", "-b", "try-git-muscle", "--output", "text"],
        Some(temp.path()),
    )
    .expect("invoke checkout -b");
    assert!(!output.status.success(), "checkout -b should be guided");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("heddle checkout -b")
            && stderr.contains("heddle start try-git-muscle --path ../try-git-muscle")
            && !stderr.contains("unexpected argument"),
        "checkout -b should produce Heddle-native guidance instead of a generic clap error: {stderr}"
    );
}

#[test]
fn switch_dash_c_guides_to_heddle_thread_flow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["switch", "-c", "try-git-muscle", "--output", "text"],
        Some(temp.path()),
    )
    .expect("invoke switch -c");
    assert!(!output.status.success(), "switch -c should be guided");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("heddle switch -c")
            && stderr.contains("heddle start try-git-muscle --path ../try-git-muscle")
            && !stderr.contains("unexpected argument"),
        "switch -c should produce Heddle-native guidance instead of a generic clap error: {stderr}"
    );
}

#[test]
fn switch_print_cd_path_alias_matches_thread_switch() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let thread = "alias-print-cd";
    let checkout = temp.path().parent().unwrap().join("alias-print-cd-path");
    let checkout_str = checkout.to_str().expect("checkout path should be utf-8");
    heddle(
        &["start", thread, "--path", checkout_str],
        Some(temp.path()),
    )
    .unwrap();

    let direct = heddle_output(
        &["thread", "switch", thread, "--print-cd-path"],
        Some(temp.path()),
    )
    .expect("invoke thread switch --print-cd-path");
    assert!(
        direct.status.success(),
        "thread switch --print-cd-path should succeed"
    );
    let expected = std::str::from_utf8(&direct.stdout)
        .unwrap()
        .trim()
        .to_string();

    let alias = heddle_output(&["switch", "--print-cd-path", thread], Some(temp.path()))
        .expect("invoke switch --print-cd-path");
    assert!(
        alias.status.success(),
        "switch --print-cd-path should behave like thread switch; stderr={}",
        std::str::from_utf8(&alias.stderr).unwrap_or("")
    );
    let stdout = std::str::from_utf8(&alias.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        expected,
        "switch --print-cd-path should print the thread checkout path"
    );
    assert_eq!(
        stdout.trim().lines().count(),
        1,
        "switch --print-cd-path should only print the path"
    );
    assert!(
        !stdout.contains("unexpected argument"),
        "alias should parse --print-cd-path instead of surfacing clap text: {stdout}"
    );

    if checkout.exists() {
        std::fs::remove_dir_all(checkout).unwrap();
    }
}

#[test]
fn log_help_examples_use_singular_path_flag() {
    let help = heddle(&["log", "--help"], None).expect("log help should render");
    assert!(
        help.contains("heddle log --path src/auth.rs"),
        "log help should document the implemented --path flag: {help}"
    );
    assert!(
        !help.contains("heddle log --paths"),
        "log help examples should not use the obsolete --paths spelling: {help}"
    );
}

#[test]
fn verify_help_names_checks_and_core_examples() {
    let help = heddle(&["verify", "--help"], None).expect("verify help should render");
    assert!(
        help.contains(
            "Checks: Git mapping, worktree, remote, operation, clone verification, machine contract."
        ),
        "verify help should name the central checks without requiring docs: {help}"
    );
    for example in [
        "heddle verify",
        "heddle verify --verbose",
        "heddle verify --output json",
    ] {
        assert!(
            help.contains(example),
            "verify help should include `{example}` example: {help}"
        );
    }
}

#[test]
fn thread_cleanup_help_renders_modes_as_bullets() {
    let help =
        heddle(&["thread", "cleanup", "--help"], None).expect("thread cleanup help should render");
    assert!(
        help.contains("Modes:")
            && help.contains("  - --merged: clean up threads recorded as merged.")
            && help
                .contains("  - --auto --older-than <duration>: clean up harness-created threads")
            && help.contains("heddle thread cleanup --merged --dry-run"),
        "thread cleanup help should keep paragraph and bullet formatting readable: {help}"
    );
}

#[test]
fn verify_is_strict_by_default() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("dirty.txt"), "dirty\n").expect("write dirty file");

    let blocked = heddle_output(&["verify", "--output", "json"], Some(temp.path()))
        .expect("invoke default verify");
    assert!(
        !blocked.status.success(),
        "default verify should fail when repository is not verified"
    );
    assert!(
        blocked.stdout.is_empty(),
        "JSON-mode verify failure should emit exactly one JSON document on stderr, not a separate proof on stdout: {}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("verify failure should be JSON advice");
    assert_eq!(envelope["kind"], "verify_failed");
    assert_eq!(envelope["verification"]["status"], "uncaptured");
    assert!(
        envelope["primary_command"]
            .as_str()
            .is_some_and(|command| command.starts_with("heddle ")),
        "verify advice should name a Heddle recovery command: {stderr}"
    );

    heddle(&["commit", "--all", "-m", "clean"], Some(temp.path())).expect("clean repo");
    let clean =
        heddle_output(&["verify", "--output", "json"], Some(temp.path())).expect("invoke verify");
    assert!(
        clean.status.success(),
        "verify should pass once repository is verified: stdout={} stderr={}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean_proof: Value = serde_json::from_slice(&clean.stdout)
        .expect("clean verify should print exactly one proof JSON document");
    assert_eq!(clean_proof["verified"], true, "{clean_proof}");
}

#[test]
fn core_json_surfaces_use_verification_not_trust() {
    let temp = TempDir::new().unwrap();

    let init = json_value(temp.path(), &["init", "--output", "json"]);
    assert!(init.get("verification").is_some(), "{init}");
    assert_no_json_key_named(&init, "trust", "init");

    for (label, args) in [
        ("status", &["status", "--output", "json"][..]),
        ("diagnose", &["diagnose", "--output", "json"]),
        ("workspace show", &["workspace", "show", "--output", "json"]),
        ("thread list", &["thread", "list", "--output", "json"]),
    ] {
        let value = json_value(temp.path(), args);
        assert!(
            value.get("verification").is_some(),
            "{label} should expose verification state: {value}"
        );
        assert_no_json_key_named(&value, "trust", label);
    }

    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["output_kind"], "verify", "{verify}");
    assert!(
        verify.get("verified").is_some(),
        "verify flattens the proof state instead of nesting it: {verify}"
    );
    assert_no_json_key_named(&verify, "trust", "verify");
}

#[test]
fn native_dirty_status_blocks_verification_without_git_overlay_language() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending\n").unwrap();

    let status = json_value(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["verified"], false);
    assert_eq!(status["verification"]["status"], "uncaptured");
    assert_eq!(status["verification"]["worktree_state"], "dirty");
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
    assert_eq!(
        status["verification"]["recommended_action"],
        "heddle commit -m \"...\""
    );
    assert_eq!(
        status["blockers"].as_array().map(Vec::len),
        Some(1),
        "native dirty status should list the actionable blocker only: {status}"
    );
    assert!(
        status["verification"]["checks"]
            .as_array()
            .is_some_and(|checks| {
                checks.iter().any(|check| {
                    check["name"] == "Worktree"
                        && check["status"] == "uncaptured"
                        && check["clean"] == false
                        && check["details"]["dirty_paths"] == "work.txt"
                })
            }),
        "worktree verify check should carry dirty path details: {status}"
    );

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Verification: 1 Heddle worktree path(s) are not captured"),
        "native dirty status should name the verify blocker: {text}"
    );
    assert!(
        !text.contains("Git overlay:"),
        "native Heddle status should not use Git-overlay labeling: {text}"
    );
    assert!(
        text.contains("commit captures them as a Heddle state") && !text.contains("Git checkpoint"),
        "native Heddle status should not describe native commits as Git checkpoints: {text}"
    );
}

#[test]
fn native_isolated_verify_status_and_doctor_present_non_overlay_as_valid() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "native-checkout");
    let checkout_arg = checkout.to_str().expect("checkout path should be utf8");
    heddle(
        &["start", "feature/native-verify", "--path", checkout_arg],
        Some(temp.path()),
    )
    .expect("isolated native checkout should start");

    let verify = json_value(&checkout, &["verify", "--output", "json"]);
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["repository_mode"], "native-heddle");
    assert!(
        verify["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("Heddle-native repository")),
        "native clean verify should summarize native mode positively: {verify}"
    );
    assert!(
        !verify.to_string().contains("not using the Git overlay"),
        "native clean verify should not frame non-overlay mode as absence: {verify}"
    );

    let checks = verify["checks"].as_array().expect("verify checks");
    let git = checks
        .iter()
        .find(|check| check["name"] == "Git")
        .unwrap_or_else(|| panic!("verify checks should include Git row: {verify}"));
    assert_eq!(git["status"], "not_applicable");
    assert_eq!(git["clean"], true);
    assert!(
        git["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("non-overlay mode")),
        "native Git row should describe valid non-overlay mode: {verify}"
    );
    let mapping = checks
        .iter()
        .find(|check| check["name"] == "Mapping")
        .unwrap_or_else(|| panic!("verify checks should include Mapping row: {verify}"));
    assert!(
        mapping["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("do not require Git-overlay mapping")),
        "native mapping row should not sound blocked: {verify}"
    );
    let clone = checks
        .iter()
        .find(|check| check["name"] == "Clone")
        .unwrap_or_else(|| panic!("verify checks should include Clone row: {verify}"));
    assert!(
        clone["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("native Heddle state")),
        "native clone row should point at the valid native authority: {verify}"
    );

    let verify_text = heddle(
        &["verify", "--verbose", "--output", "text"],
        Some(&checkout),
    )
    .expect("native verify text");
    assert!(
        verify_text.contains("Repository verification: clean"),
        "native verify text should use a generic verify label: {verify_text}"
    );
    assert!(
        !verify_text.contains("Git and Heddle: clean")
            && !verify_text.contains("not using the Git overlay"),
        "native verify text should not present native mode as a Git-overlay downgrade: {verify_text}"
    );

    let status_text =
        heddle(&["status", "--output", "text"], Some(&checkout)).expect("native status text");
    assert!(
        !status_text.contains("Git overlay:") && !status_text.contains("not using the Git overlay"),
        "native status should not use Git-overlay problem language: {status_text}"
    );

    let doctor = json_value(&checkout, &["doctor", "--output", "json"]);
    assert_eq!(doctor["verification"]["verified"], true);
    assert_eq!(doctor["verification"]["status"], "clean");
    assert!(
        doctor["git_overlay_health"]["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("Heddle-native repository")),
        "native doctor JSON should summarize non-overlay mode positively: {doctor}"
    );
}

#[test]
fn first_status_before_capture_names_default_identity() {
    let temp = TempDir::new().unwrap();
    let init = heddle_output_without_principal_env(&["init"], temp.path()).expect("init output");
    assert!(init.status.success(), "init should succeed");

    let output = heddle_output_without_principal_env(&["status", "--output", "text"], temp.path())
        .expect("status output");
    assert!(output.status.success(), "status should succeed");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Identity:")
            && text.contains("first capture/checkpoint")
            && text.contains("Unknown <unknown@example.com>")
            && text.contains("HEDDLE_PRINCIPAL_NAME"),
        "first-run status should make default attribution explicit before capture: {text}"
    );
}

#[test]
fn capture_without_principal_refuses_before_recording_empty_identity() {
    let temp = TempDir::new().unwrap();
    let init = heddle_output_without_principal_env(&["init"], temp.path()).expect("init output");
    assert!(init.status.success(), "init should succeed");
    std::fs::write(temp.path().join("work.txt"), "anonymous\n").unwrap();
    let output = heddle_output_without_principal_env(
        &["capture", "-m", "anonymous work", "--output", "json"],
        temp.path(),
    )
    .expect("capture should run");
    assert!(
        !output.status.success(),
        "capture must refuse missing identity"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "capture_identity_required");
    assert_eq!(
        envelope["primary_command"],
        "heddle init --principal-name <name> --principal-email <email>"
    );
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("Unknown <unknown@example.com>")),
        "identity refusal should name the unsafe fallback: {envelope}"
    );
    let log = heddle_output_without_principal_env(&["log", "--output", "json"], temp.path())
        .expect("log should run after refused capture");
    let log_stdout = std::str::from_utf8(&log.stdout).unwrap();
    assert!(
        !log_stdout.contains("anonymous work") && !log_stdout.contains(" <>"),
        "refused capture must not record anonymous state: {log_stdout}"
    );
}

#[test]
fn git_overlay_isolated_checkout_status_and_verify_identify_parent_context() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt"], Some(temp.path())).expect("adopt Git overlay repo");

    let checkout = sibling_checkout_path(temp.path(), "git-overlay-child");
    let checkout_arg = checkout.to_str().expect("checkout path should be utf8");
    json_value(
        temp.path(),
        &[
            "start",
            "feature/git-overlay-child",
            "--path",
            checkout_arg,
            "--output",
            "json",
        ],
    );

    let status = json_value(&checkout, &["status", "--output", "json"]);
    assert_eq!(
        status["repository_capability"], "native-heddle",
        "isolated child checkout should keep core capability semantics: {status}"
    );
    assert_eq!(
        status["repository_label"], "Git + Heddle isolated checkout",
        "status JSON should not identify the child only as native-heddle: {status}"
    );
    assert_eq!(
        status["repository_context"]["kind"],
        "git-overlay-isolated-checkout"
    );
    assert_eq!(
        status["repository_context"]["parent_repository"],
        temp.path().display().to_string()
    );
    assert_eq!(status["repository_context"]["target_thread"], "main");
    assert_eq!(status["target_thread"], "main");

    let status_text =
        heddle(&["status", "--output", "text"], Some(&checkout)).expect("status text");
    assert!(
        status_text.contains("Repository: Git + Heddle isolated checkout")
            && status_text.contains(&format!("Parent repo: {}", temp.path().display()))
            && status_text
                .contains("Git checkout: no .git here; raw Git commands belong in the parent repo")
            && status_text.contains("Target thread: main")
            && !status_text.contains("Repository: native-heddle"),
        "status text should surface managed Git-overlay child context: {status_text}"
    );

    let verify = json_value(&checkout, &["verify", "--output", "json"]);
    assert_eq!(verify["repository_mode"], "native-heddle");
    assert_eq!(verify["repository_label"], "Git + Heddle isolated checkout");
    assert_eq!(
        verify["repository_context"]["parent_repository"],
        temp.path().display().to_string()
    );
    assert_eq!(verify["repository_context"]["target_thread"], "main");

    let verify_text = heddle(
        &["verify", "--verbose", "--output", "text"],
        Some(&checkout),
    )
    .expect("verify text");
    assert!(
        verify_text.contains("Repository: Git + Heddle isolated checkout")
            && verify_text.contains(&format!("Parent repo: {}", temp.path().display()))
            && verify_text.contains("Target thread: main"),
        "verify text should surface managed Git-overlay child context: {verify_text}"
    );
}

#[test]
fn status_short_reports_clean_state_instead_of_silence() {
    let native = TempDir::new().unwrap();
    heddle(&["init"], Some(native.path())).unwrap();
    let native_clean = heddle(
        &["status", "--short", "--output", "text"],
        Some(native.path()),
    )
    .unwrap();
    assert_eq!(
        native_clean.trim(),
        "repository clean",
        "clean native short status should be one compact line: {native_clean:?}"
    );

    std::fs::write(native.path().join("draft.txt"), "draft\n").unwrap();
    let native_dirty = heddle(
        &["status", "--short", "--output", "text"],
        Some(native.path()),
    )
    .unwrap();
    assert!(
        native_dirty.contains("A  draft.txt") && !native_dirty.contains("main clean"),
        "dirty short status should stay path-focused: {native_dirty:?}"
    );

    let plain = TempDir::new().unwrap();
    init_git_repo_for_json_contract(plain.path(), "main");
    std::fs::write(plain.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(plain.path(), "seed");
    let plain_short = heddle(
        &["status", "--short", "--output", "text"],
        Some(plain.path()),
    )
    .unwrap();
    assert_eq!(
        plain_short.trim(),
        "main setup needed",
        "plain Git short status should not be silent: {plain_short:?}"
    );
    assert!(
        !plain.path().join(".heddle").exists(),
        "plain Git short status must remain observe-only"
    );
}

#[test]
fn global_repo_short_flag_runs_from_outside_repo_without_initializing() {
    let repo = TempDir::new().unwrap();
    init_git_repo_for_json_contract(repo.path(), "main");
    std::fs::write(repo.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(repo.path(), "seed");

    let repo_arg = repo.path().to_string_lossy().to_string();
    let short = heddle(
        &["-C", &repo_arg, "status", "--short", "--output", "text"],
        None,
    )
    .unwrap();
    assert_eq!(
        short.trim(),
        "main setup needed",
        "`heddle -C <repo>` should behave like --repo for first-contact status: {short:?}"
    );
    assert!(
        !repo.path().join(".heddle").exists(),
        "`heddle -C <repo> status` must remain observe-only in a plain Git repo"
    );

    let catalog = heddle(&["-C", &repo_arg, "--output", "json"], None)
        .expect("global -C without a verb should still render the command catalog");
    let catalog: Value = serde_json::from_str(&catalog).expect("command catalog should parse");
    assert_eq!(catalog["kind"], "command_catalog");
}

#[test]
fn plain_git_diff_and_inspect_path_route_to_adoption_guidance() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("README.md"), "# project\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed readme");

    let diff = heddle_output(&["diff", "--output", "json"], Some(temp.path()))
        .expect("invoke pre-adoption diff");
    assert!(
        !diff.status.success(),
        "clean pre-adoption diff should refuse instead of emitting an empty success payload"
    );
    assert!(
        diff.stdout.is_empty(),
        "diff refusal should not emit a blank-success diff payload: {}",
        String::from_utf8_lossy(&diff.stdout)
    );
    let diff_stderr = String::from_utf8_lossy(&diff.stderr);
    let envelope: Value =
        serde_json::from_str(&diff_stderr).expect("diff refusal should be JSON advice");
    assert_eq!(envelope["kind"], "plain_git_not_adopted");
    assert_eq!(envelope["primary_command"], "heddle adopt --ref main");
    assert_eq!(envelope["repository_capability"], "plain-git");
    assert_eq!(envelope["verification"]["repository_mode"], "plain-git");
    assert_eq!(
        envelope["verification"]["recommended_action"],
        "heddle adopt --ref main"
    );

    let inspect = heddle_output(
        &["inspect", "README.md", "--output", "text"],
        Some(temp.path()),
    )
    .expect("invoke pre-adoption inspect path");
    assert!(
        inspect.status.success(),
        "pre-adoption inspect should render setup guidance: stderr={}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspect_text = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        inspect_text.contains("Git repo, Heddle not initialized")
            && inspect_text.contains("heddle adopt --ref main")
            && !inspect_text.contains("heddle log")
            && !inspect_text.contains("state_not_found"),
        "inspect path should route to adoption guidance, not Heddle state lookup advice: {inspect_text}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "pre-adoption inspect/diff guidance must remain observe-only"
    );
}

#[test]
fn command_catalog_alias_serves_machine_catalog() {
    let catalog = heddle(&["command-catalog", "--output", "json"], None)
        .expect("command-catalog alias should render the command catalog");
    let catalog: Value = serde_json::from_str(&catalog).expect("command catalog should parse");
    assert_eq!(catalog["kind"], "command_catalog");
}

#[test]
fn init_json_names_side_effects_next_action_and_schema() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    let init = json_value(temp.path(), &["init", "--output", "json"]);
    assert_eq!(init["status"], "initialized");
    assert_eq!(init["action"], "init");
    assert_eq!(init["repository_mode"], "git-overlay");
    assert_eq!(init["git_detected"], true);
    assert_eq!(init["heddle_initialized"], true);
    assert_eq!(init["installed_heddleignore"], false);
    assert_eq!(init["principal_configured"], false);
    assert_eq!(init["principal_status"], "configured");
    assert_eq!(init["principal_source"], "git_config");
    assert_eq!(init["principal"]["name"], "Heddle Test");
    assert_eq!(init["principal"]["email"], "heddle@example.com");
    assert_eq!(init["principal_recommended_action"], Value::Null);
    assert_eq!(init["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        init["verification"]["status"], "needs_import",
        "post-init verify should keep the first-run import blocker explicit: {init}"
    );
    assert!(
        init["side_effects"].as_array().is_some_and(|effects| {
            effects.iter().any(|effect| {
                effect
                    .as_str()
                    .is_some_and(|effect| effect.contains("Git-tracked files"))
            })
        }),
        "Git-overlay init should say it left Git-tracked files untouched: {init}"
    );
    assert!(
        init["side_effects"].as_array().is_some_and(|effects| {
            effects.iter().any(|effect| {
                effect.as_str().is_some_and(|effect| {
                    effect.contains(".git/info/exclude")
                        && effect.contains("Heddle metadata")
                        && !effect.contains("default generated noise")
                })
            })
        }),
        "Git-overlay init should name its local Git exclude policy update: {init}"
    );
    assert_schema_declares_runtime_top_level(&["init"], &init);
    assert_eq!(
        std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(temp.path())
            .output()
            .expect("git status should run")
            .stdout,
        b"",
        "init in a Git repo must keep Git status clean"
    );

    let text_temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(text_temp.path(), "main");
    std::fs::write(text_temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(text_temp.path(), "seed");
    let text = heddle(&["init", "--output", "text"], Some(text_temp.path())).unwrap();
    assert!(
        text.contains("Side effects:")
            && text.contains("Principal: Heddle Test <heddle@example.com> from git_config")
            && text.contains(".git/info/exclude")
            && text.contains("left Git-tracked files")
            && text.contains("Next: heddle adopt --ref main"),
        "init text should make side effects and import next step obvious: {text}"
    );
}

#[test]
fn native_init_reports_missing_or_configured_principal_without_agent_default() {
    let missing = TempDir::new().unwrap();
    let output = heddle_output_without_principal_env(&["init", "--output", "json"], missing.path())
        .expect("native init without principal env should run");
    assert!(output.status.success(), "init should succeed: {output:?}");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let init = parse_exactly_one_json_value(stdout)
        .unwrap_or_else(|err| panic!("init JSON should parse: {err}: {stdout}"));
    assert_eq!(init["principal_configured"], false);
    assert_eq!(init["principal_status"], "not_configured");
    assert_eq!(init["principal_source"], Value::Null);
    assert_eq!(init["principal"], Value::Null);
    assert_eq!(
        init["principal_recommended_action"],
        "heddle init --principal-name <name> --principal-email <email>"
    );
    assert!(
        stdout.contains("not_configured") && !stdout.contains("agent@example.com"),
        "native init should not imply an agent fallback identity: {stdout}"
    );
    assert_schema_declares_runtime_top_level(&["init"], &init);

    let text_temp = TempDir::new().unwrap();
    let output =
        heddle_output_without_principal_env(&["init", "--output", "text"], text_temp.path())
            .expect("native text init without principal env should run");
    assert!(
        output.status.success(),
        "text init should succeed: {output:?}"
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert!(
        text.contains("Principal: not configured")
            && text.contains(
                "set with: heddle init --principal-name <name> --principal-email <email>"
            )
            && !text.contains("agent@example.com"),
        "native init text should make attribution setup explicit: {text}"
    );

    let configured = TempDir::new().unwrap();
    let output = heddle_output_without_principal_env(
        &[
            "init",
            "--principal-name",
            "Cold Dev",
            "--principal-email",
            "cold@example.com",
            "--output",
            "json",
        ],
        configured.path(),
    )
    .expect("native init with principal should run");
    assert!(
        output.status.success(),
        "configured init should succeed: {output:?}"
    );
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let init = parse_exactly_one_json_value(stdout)
        .unwrap_or_else(|err| panic!("configured init JSON should parse: {err}: {stdout}"));
    assert_eq!(init["principal_configured"], true);
    assert_eq!(init["principal_status"], "configured");
    assert_eq!(init["principal_source"], "user_config");
    assert_eq!(init["principal"]["name"], "Cold Dev");
    assert_eq!(init["principal"]["email"], "cold@example.com");
    assert_eq!(init["principal_recommended_action"], Value::Null);
}

#[test]
fn json_mode_parse_errors_emit_error_envelope() {
    let output = heddle_output(&["--output", "json", "statuz"], None).expect("invoke heddle");
    assert!(!output.status.success(), "unknown command should fail");
    assert!(
        output.stdout.is_empty(),
        "parse failures in JSON mode must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "parse_error");
    assert_eq!(parsed["code"], "parse_error");
    // EX_USAGE (sysexits) — unknown subcommand. The taxonomy in
    // `crates/cli/src/exit.rs` reserves `2` for unhandled errors /
    // panics, never intentional emission.
    assert_eq!(parsed["exit_code"], 64);
    assert_eq!(
        parsed["primary_command_template"]["argv_template"],
        heddle_argv_json(["commands", "--output", "json"])
    );
    assert!(
        parsed["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("no command body was executed")),
        "parse envelope should say that no mutation ran: {parsed}"
    );
    assert!(
        parsed["error"].as_str().unwrap_or("").contains("statuz"),
        "parse envelope should preserve clap's command detail: {parsed}"
    );
}

#[test]
fn confidence_parse_errors_fail_loudly_in_json_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for value in ["1.7", "NaN"] {
        let output = heddle_output(
            &[
                "--output",
                "json",
                "capture",
                "-m",
                "bad confidence",
                "--confidence",
                value,
            ],
            Some(temp.path()),
        )
        .expect("invoke heddle");
        assert!(!output.status.success(), "invalid confidence should fail");
        assert!(
            output.stdout.is_empty(),
            "parse failures in JSON mode must not write stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let parsed: Value = serde_json::from_str(stderr)
            .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
        assert_eq!(parsed["kind"], "parse_error");
        assert_eq!(parsed["code"], "parse_error");
        assert_eq!(
            parsed["primary_command_template"]["argv_template"],
            heddle_argv_json(["commands", "--output", "json"])
        );
        assert!(
            parsed["error"].as_str().is_some_and(
                |error| error.contains("confidence must be a finite number from 0.0 to 1.0")
            ),
            "parse error should explain the accepted confidence range: {parsed}"
        );
    }
}

#[test]
fn explicit_json_for_text_only_command_uses_contract_advice() {
    let output = heddle_output(&["--output", "json", "completion", "bash"], None).expect("invoke");
    assert!(
        !output.status.success(),
        "text-only command should reject explicit JSON"
    );
    assert!(
        output.stdout.is_empty(),
        "contract refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "json_unsupported");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle commands --output json")),
        "contract advice should point to command catalog: {stderr}"
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("heddle completion")),
        "contract advice should use the typed refusal envelope: {stderr}"
    );
}

#[test]
fn command_catalog_exposes_agent_metadata_for_options() {
    let json = heddle(&["--output", "json", "commands"], None).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert!(
        !parsed["recommended_action_placeholders"]
            .as_array()
            .expect("placeholder registry should be cataloged")
            .iter()
            .any(|action| action
                .as_str()
                .is_some_and(|action| action.starts_with("git "))),
        "command catalog should not recommend Git CLI recovery in a no-git runtime: {parsed}"
    );
    for placeholder in [
        "heddle capture -m \"...\"",
        "heddle checkpoint -m \"...\"",
        "heddle commit -m \"...\"",
        "heddle stash push -m \"...\"",
        "heddle switch <branch>",
        "heddle clone <remote> <fresh-path>",
    ] {
        assert!(
            parsed["recommended_action_placeholders"]
                .as_array()
                .expect("placeholder registry should be cataloged")
                .iter()
                .any(|action| action == placeholder),
            "message-template placeholder should be explicit: {placeholder}: {parsed}"
        );
    }
    let commands = parsed["commands"].as_array().unwrap();
    for command in commands {
        assert_eq!(
            command["requires_git_executable"],
            false,
            "`{}` must advertise the no-Git-runtime contract: {parsed}",
            command["display"].as_str().unwrap_or("<unknown>")
        );
    }
    let workspace = commands
        .iter()
        .find(|entry| entry["display"] == "workspace")
        .expect("bare workspace command should be cataloged");
    assert_eq!(
        workspace["schema_verbs"],
        serde_json::json!(["workspace show"])
    );
    assert_eq!(
        workspace["documented_schema_verbs"],
        serde_json::json!(["workspace show"]),
        "bare workspace defaults to workspace show, so its catalog schema docs should match: {parsed}"
    );
    let status = commands
        .iter()
        .find(|entry| entry["display"] == "status")
        .expect("status command should be cataloged");
    assert_eq!(status["supports_json"], true);
    assert_eq!(status["mutates"], false);
    assert_eq!(status["side_effect_class"], "observe_only");
    assert_eq!(status["side_effects"], serde_json::json!(["observe_only"]));
    assert_eq!(status["first_run_behavior"], "observe_only_no_init");
    assert_eq!(status["json_kind"], "json_or_jsonl");
    assert_eq!(status["op_id_store_scope"], "none");
    assert_eq!(status["schema_verbs"], serde_json::json!(["status"]));
    assert_eq!(
        status["documented_schema_verbs"],
        serde_json::json!(["status"])
    );
    let short = status["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["long"] == "short")
        .expect("status --short should be cataloged");
    assert_eq!(short["value_kind"], "boolean");

    let commit = commands
        .iter()
        .find(|entry| entry["display"] == "commit")
        .expect("commit shim should be cataloged");
    assert_eq!(commit["mutates"], true);
    assert_eq!(commit["supports_op_id"], true);
    assert_eq!(commit["persists_op_id"], false);
    assert_eq!(commit["op_id_behavior"], "explicit_replay");
    assert_eq!(commit["op_id_store_scope"], "repository");
    assert_eq!(commit["side_effect_class"], "ref_mutation");
    assert_eq!(commit["writes_heddle_refs"], true);
    assert_eq!(commit["writes_git_refs"], true);
    assert_eq!(
        commit["side_effects"],
        serde_json::json!(["writes_heddle_refs", "writes_git_refs"])
    );
    assert_eq!(commit["first_run_behavior"], "requires_initialized_repo");

    let capture = commands
        .iter()
        .find(|entry| entry["display"] == "capture")
        .expect("capture command should be cataloged");
    assert_eq!(capture["supports_op_id"], true);
    assert_eq!(capture["persists_op_id"], false);
    assert_eq!(capture["op_id_behavior"], "explicit_replay");
    assert_eq!(capture["op_id_store_scope"], "repository");
    assert_eq!(capture["side_effect_class"], "ref_mutation");
    assert_eq!(capture["writes_heddle_refs"], true);
    assert_eq!(capture["writes_git_refs"], false);
    assert_eq!(
        capture["side_effects"],
        serde_json::json!(["writes_heddle_refs"])
    );
    assert_eq!(capture["first_run_behavior"], "requires_initialized_repo");

    let init = commands
        .iter()
        .find(|entry| entry["display"] == "init")
        .expect("init should be cataloged");
    assert_eq!(init["mutates"], true);
    assert_eq!(init["supports_op_id"], true);
    assert_eq!(init["op_id_behavior"], "explicit_replay");
    assert_eq!(init["op_id_store_scope"], "bootstrap");
    assert_eq!(init["side_effect_class"], "initialize");
    assert_eq!(init["writes_config"], true);
    assert_eq!(
        init["side_effects"],
        serde_json::json!(["initialize", "writes_config"])
    );
    assert_eq!(init["first_run_behavior"], "may_initialize");

    let diff = commands
        .iter()
        .find(|entry| entry["display"] == "diff")
        .expect("diff should be cataloged");
    assert_eq!(diff["side_effect_class"], "observe_only");
    assert_eq!(diff["side_effects"], serde_json::json!(["observe_only"]));
    assert_eq!(diff["first_run_behavior"], "observe_only_no_init");
    assert_eq!(diff["schema_verbs"], serde_json::json!(["diff"]));
    assert_eq!(diff["documented_schema_verbs"], serde_json::json!(["diff"]));

    let push = commands
        .iter()
        .find(|entry| entry["display"] == "push")
        .expect("push should be cataloged");
    assert_eq!(push["network_io"], true);
    assert_eq!(push["writes_heddle_refs"], true);
    assert_eq!(push["writes_git_refs"], true);
    assert_eq!(push["side_effect_class"], "network_mutation");

    let remote_add = commands
        .iter()
        .find(|entry| entry["display"] == "remote add")
        .expect("remote add should be cataloged");
    assert_eq!(remote_add["writes_config"], true);
    assert_eq!(remote_add["writes_heddle_refs"], false);
    assert_eq!(remote_add["side_effect_class"], "config_mutation");

    let hook_install = commands
        .iter()
        .find(|entry| entry["display"] == "hook install")
        .expect("hook install should be cataloged");
    assert_eq!(hook_install["writes_hooks"], true);
    assert_eq!(hook_install["writes_config"], true);
    assert_eq!(hook_install["side_effect_class"], "hook_mutation");

    let maintenance_gc = commands
        .iter()
        .find(|entry| entry["display"] == "maintenance gc")
        .expect("maintenance gc should be cataloged");
    assert_eq!(maintenance_gc["object_gc"], true);
    assert_eq!(maintenance_gc["writes_heddle_refs"], false);
    assert_eq!(maintenance_gc["side_effect_class"], "object_gc");

    let clean = commands
        .iter()
        .find(|entry| entry["display"] == "clean")
        .expect("clean should be cataloged");
    assert_eq!(clean["writes_worktree"], true);
    assert_eq!(clean["writes_heddle_refs"], false);
    assert_eq!(clean["destructive_data"], true);
    assert_eq!(clean["side_effect_class"], "destructive_worktree_mutation");

    let stash_drop = commands
        .iter()
        .find(|entry| entry["display"] == "stash drop")
        .expect("stash drop should be cataloged");
    assert_eq!(stash_drop["writes_worktree"], false);
    assert_eq!(stash_drop["writes_heddle_refs"], false);
    assert_eq!(stash_drop["destructive_data"], true);
    assert_eq!(stash_drop["side_effect_class"], "destructive_data");
    assert_eq!(
        stash_drop["side_effects"],
        serde_json::json!(["destructive_data"])
    );

    let start = commands
        .iter()
        .find(|entry| entry["display"] == "start")
        .expect("start should be cataloged");
    assert_eq!(start["writes_heddle_refs"], true);
    assert_eq!(start["writes_worktree"], true);
    assert_eq!(start["side_effect_class"], "worktree_mutation");

    let run = commands
        .iter()
        .find(|entry| entry["display"] == "run")
        .expect("run should be cataloged");
    assert_eq!(run["external_command"], true);
    assert_eq!(run["may_write_worktree"], true);
    assert_eq!(run["writes_worktree"], false);
    assert_eq!(run["writes_heddle_refs"], false);
    assert_eq!(run["side_effect_class"], "external_command");
    assert_eq!(
        run["side_effects"],
        serde_json::json!(["may_write_worktree", "external_command"])
    );

    let attempt = commands
        .iter()
        .find(|entry| entry["display"] == "attempt")
        .expect("attempt should be cataloged");
    assert_eq!(attempt["external_command"], true);
    assert_eq!(attempt["writes_worktree"], true);
    assert_eq!(attempt["writes_heddle_refs"], true);
    assert_eq!(attempt["side_effect_class"], "worktree_mutation");

    let watch = commands
        .iter()
        .find(|entry| entry["display"] == "watch")
        .expect("watch should be cataloged");
    assert_eq!(watch["json_kind"], "jsonl");

    let thread_show = commands
        .iter()
        .find(|entry| entry["display"] == "thread show")
        .expect("thread show should be cataloged");
    assert_eq!(thread_show["json_kind"], "json_or_jsonl");
    assert_eq!(
        thread_show["schema_verbs"],
        serde_json::json!(["thread show"])
    );

    let start = commands
        .iter()
        .find(|entry| entry["display"] == "start")
        .expect("start should be cataloged");
    assert_eq!(start["schema_verbs"], serde_json::json!(["start"]));

    let completion = commands
        .iter()
        .find(|entry| entry["display"] == "completion")
        .expect("completion should be cataloged");
    assert_eq!(completion["supports_json"], false);
    assert_eq!(completion["json_kind"], "none");
}

#[test]
fn diff_json_output_matches_registered_schema_top_level() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending\n").unwrap();

    let diff = json_value(temp.path(), &["diff", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["diff"], &diff);
    assert!(
        diff.get("from_state").is_some() && diff.get("to_state").is_some(),
        "diff JSON should expose runtime state fields declared by the schema: {diff}"
    );

    let schema = json_value(temp.path(), &["schemas", "diff", "--output", "json"]);
    let properties = schema["properties"]
        .as_object()
        .expect("diff schema should expose properties");
    assert!(
        properties.contains_key("from_state") && properties.contains_key("to_state"),
        "diff schema should use runtime field names: {schema}"
    );
    assert!(
        !properties.contains_key("from") && !properties.contains_key("to"),
        "diff schema should not advertise stale aliases: {schema}"
    );
}

#[test]
fn diff_text_summarizes_binary_without_raw_control_bytes() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("asset.bin"), [b'A', 0, b'B', 7, b'C']).unwrap();
    heddle(&["capture", "-m", "base binary"], Some(temp.path())).unwrap();

    std::fs::write(
        temp.path().join("asset.bin"),
        [b'A', 0, b'Z', 0x1b, b'[', b'3', b'1', b'm'],
    )
    .unwrap();

    let output = heddle_output_with_env(
        &["--output", "text", "diff"],
        Some(temp.path()),
        &[("NO_COLOR", "1"), ("HEDDLE_NO_PAGER", "1")],
    )
    .expect("diff should run");
    assert!(
        output.status.success(),
        "diff should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Binary file changed: asset.bin"),
        "binary diff should render a human summary: {stdout:?}"
    );
    assert!(
        output
            .stdout
            .iter()
            .all(|byte| !byte.is_ascii_control() || matches!(byte, b'\n' | b'\t')),
        "binary diff text should not contain terminal-hostile control bytes: {stdout:?}"
    );
}

#[test]
fn schemas_no_arg_lists_verbs_and_ignores_trailing_global_flags() {
    let listing = parse_exactly_one_json_value(
        &heddle(&["schemas"], None).expect("schemas without a verb should list schema verbs"),
    )
    .expect("schema listing should be one JSON value");
    assert!(
        listing["schema_verbs"]
            .as_array()
            .is_some_and(|verbs| verbs.iter().any(|verb| verb == "verify")),
        "schema listing should include registered verbs: {listing}"
    );

    let direct = parse_exactly_one_json_value(
        &heddle(&["schemas", "merge", "--preview"], None)
            .expect("merge preview schema should be discoverable"),
    )
    .expect("merge preview schema should parse");
    let trailing = parse_exactly_one_json_value(
        &heddle(
            &[
                "schemas",
                "merge",
                "--preview",
                "--output",
                "text",
                "--verbose",
            ],
            None,
        )
        .expect("trailing global flags should not become part of the schema verb"),
    )
    .expect("merge preview schema with trailing global flags should parse");
    assert_eq!(trailing["properties"], direct["properties"]);
}

#[test]
fn verify_cold_flow_scripts_assert_required_proof_steps() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("cli crate should be under crates/cli")
        .to_path_buf();
    for script in [
        root.join("scripts/verify-cold-flow-human.sh"),
        root.join("scripts/verify-cold-flow-agent.sh"),
    ] {
        let source = std::fs::read_to_string(&script)
            .unwrap_or_else(|err| panic!("read {}: {err}", script.display()));
        for shape in ["small-app", "large-rust", "complex-git"] {
            assert!(
                source.contains(shape),
                "{} should cover {shape}",
                script.display()
            );
        }
        for proof in [
            "commit",
            "undo",
            "fetch",
            "pull",
            "push",
            "clone",
            "start",
            "ready",
            "--preview",
            "blame",
            "assert_final_verify",
            "assert_transcript_claims",
            "HEDDLE_RUNTIME_PATH",
            "heddle_runtime()",
            "heddle_runtime_path_label",
        ] {
            assert!(
                source.contains(proof),
                "{} should assert/run proof step `{proof}`",
                script.display()
            );
        }
        for (line_number, line) in source.lines().enumerate() {
            if line.contains("\"$HEDDLE_BIN\"") {
                assert!(
                    line.contains("env PATH=\"$HEDDLE_RUNTIME_PATH\""),
                    "{}:{} should invoke Heddle through the no-Git runtime helper, got `{line}`",
                    script.display(),
                    line_number + 1
                );
            }
        }
        assert!(
            source.contains("adopt")
                || source.contains("bridge git import")
                || source.contains("run_verify_recommended_action"),
            "{} should run one-command adoption, explicit import, or verify's recommended action",
            script.display()
        );
        for bridge_sync in ["bridge git push", "bridge git pull"] {
            assert!(
                !source.contains(bridge_sync),
                "{} should prove the everyday top-level sync path, not bridge plumbing `{bridge_sync}`",
                script.display()
            );
        }
        if script.file_name().and_then(|name| name.to_str()) == Some("verify-cold-flow-agent.sh") {
            assert!(
                source.contains("reconcile"),
                "{} should prove bridge reconcile in the machine-oriented flow",
                script.display()
            );
            assert!(
                source.contains("recommended_action_template") && source.contains("argv_template"),
                "{} should execute structured verify actions and fill display-only templates",
                script.display()
            );
            assert!(
                source.contains("--op-id") && source.contains("side_effects"),
                "{} should prove op-id replay and precise command side effects",
                script.display()
            );
            assert!(
                source.contains("assert_local_ahead_verified_json")
                    && source.contains("\"remote_drift\": \"remote_ahead\""),
                "{} should prove local-ahead commits remain verified sync guidance",
                script.display()
            );
            assert!(
                source.contains("assert_merge_preview_points_to_ship_json"),
                "{} should prove merge preview points to the ship landing loop",
                script.display()
            );
            assert!(
                source.contains("checkpoint") && source.contains("capture"),
                "{} should prove the explicit capture/checkpoint machine loop",
                script.display()
            );
            assert!(
                source.contains("exit_code_text) == 0 and stderr.strip()"),
                "{} should fail successful JSON commands that write stderr",
                script.display()
            );
            assert!(
                source.contains("\"heddle_runtime_path\"")
                    && source.contains("\"requires_git_executable\": False"),
                "{} should record the no-Git Heddle runtime proof in JSONL transcripts",
                script.display()
            );
        } else {
            assert!(
                !source.contains("run_text \"$transcript\" \"$repo\" capture -m")
                    && !source.contains("run_text \"$transcript\" \"$repo\" checkpoint -m"),
                "{} should keep the human cold path on the one-step commit loop",
                script.display()
            );
            assert!(
                !source.contains("bridge git status")
                    && !source.contains("bridge git reconcile")
                    && source.contains("\"heddle bridge git\"")
                    && source.contains("\"reconcile\""),
                "{} should keep bridge ceremony out of the human cold path and lint it from transcripts",
                script.display()
            );
            assert!(
                source.contains("Heddle runtime proof:")
                    && source.contains("PATH=%s")
                    && source.contains("Git was used only to build fixture repositories"),
                "{} should record the no-Git Heddle runtime proof in human transcripts",
                script.display()
            );
        }
    }
}

#[test]
fn op_id_replays_local_mutating_command_and_rejects_arg_conflict() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440000";

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();

    let first = heddle(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "op replay",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&first).expect("first capture JSON should parse");
    assert!(
        parsed["change_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("hd-"),
        "first capture should return a state id: {parsed}"
    );
    assert_eq!(parsed["op_id"], op_id);
    assert_eq!(parsed["idempotency_status"], "executed");
    assert_eq!(parsed["replayed"], false);
    assert_eq!(parsed["operation_record"]["command"], "capture");
    assert_schema_declares_runtime_top_level(&["capture"], &parsed);

    std::fs::write(temp.path().join("tracked.txt"), "second\n").unwrap();
    let replay = heddle(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "op replay",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let replayed: Value =
        serde_json::from_str(&replay).expect("replayed capture JSON should parse");
    assert_eq!(
        replayed["change_id"], parsed["change_id"],
        "same op-id and args should replay the original mutation result"
    );
    assert_eq!(replayed["op_id"], op_id);
    assert_eq!(replayed["idempotency_status"], "replayed");
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["operation_record"]["command"], "capture");
    assert_schema_declares_runtime_top_level(&["capture"], &replayed);

    let status = heddle(&["--output", "json", "status"], Some(temp.path())).unwrap();
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(
        status["changes"]["modified"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|path| path == "tracked.txt")),
        "replayed capture must not execute a second mutation: {status}"
    );

    let conflict = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "different args",
        ],
        Some(temp.path()),
    )
    .expect("invoke conflicting op-id");
    assert!(!conflict.status.success(), "conflicting op-id should fail");
    let stderr = std::str::from_utf8(&conflict.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("conflict should be a JSON envelope: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "op_id_conflict");
    assert_eq!(parsed["op_id"], op_id);
    assert_eq!(parsed["idempotency_status"], "conflict");
    assert_eq!(parsed["replayed"], false);
    assert_eq!(parsed["recorded_command"], "capture");
    assert_eq!(parsed["incoming_command"], "capture");
    assert_eq!(parsed["recorded_status"], "completed");
    assert!(
        parsed["dedup_scope"]
            .as_str()
            .is_some_and(|scope| scope.contains(".heddle")),
        "repo-local conflict should name its dedup scope: {parsed}"
    );
    assert!(
        parsed["incoming_argv"]
            .as_array()
            .is_some_and(|argv| argv.iter().any(|arg| arg == "different args")),
        "conflict envelope should expose normalized incoming argv: {parsed}"
    );
    assert!(
        parsed["recorded_request_hash"].as_str().is_some()
            && parsed["incoming_request_hash"].as_str().is_some()
            && parsed["recorded_created_at_secs"].as_i64().is_some(),
        "conflict envelope should expose safe hash/timestamp diagnostics: {parsed}"
    );
}

/// Two parallel `heddle capture --op-id <same>` invocations must NOT both
/// execute the underlying command. Before r9 each CLI process opened its
/// own `OperationDedupStore`, took only an in-process `Mutex`, read an
/// empty `operation_dedup.bin`, and both proceeded to execute — defeating
/// the local idempotency guarantee for the retry/concurrent-submit case
/// the store exists for. The fix locks the dedup file via `RepoLock` and
/// reloads from disk inside every read-modify-write so the second process
/// observes the first's pending reservation.
#[test]
fn op_id_local_dedup_is_cross_process_safe() {
    use std::sync::{Arc, Barrier};

    let temp = TempDir::new().unwrap();
    let op_id = "1d4d8e92-58a1-4f73-9d4c-2d97a8e1b9aa";

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();

    let repo_path = temp.path().to_path_buf();
    let config_path = default_test_user_config_path(&repo_path);
    seed_default_test_user_config(&config_path, &repo_path).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let repo_path = repo_path.clone();
        let config_path = config_path.clone();
        let barrier = Arc::clone(&barrier);
        let op_id = op_id.to_string();
        handles.push(std::thread::spawn(move || {
            // Sync both processes as close to the reserve() call as
            // possible so the race window is real, not just sequential
            // serialization.
            barrier.wait();
            std::process::Command::new(env!("CARGO_BIN_EXE_heddle"))
                .args([
                    "--output",
                    "json",
                    "--op-id",
                    &op_id,
                    "capture",
                    "-m",
                    "concurrent race",
                ])
                .current_dir(&repo_path)
                .env("HEDDLE_CONFIG", &config_path)
                .output()
                .expect("spawn heddle")
        }));
    }
    let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let mut executed = 0;
    let mut deduped = 0;
    for output in &outputs {
        let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
        let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
        // Successful executions write the envelope to stdout; in-flight
        // rejections write a typed error envelope to stderr.
        let envelope_text = if output.status.success() {
            stdout
        } else {
            stderr
        };
        let envelope: Value = serde_json::from_str(envelope_text).unwrap_or_else(|err| {
            panic!(
                "expected JSON envelope from racing heddle: {err}\nstatus: {:?}\nstdout: {stdout}\nstderr: {stderr}",
                output.status.code(),
            )
        });
        match envelope["idempotency_status"].as_str() {
            Some("executed") => executed += 1,
            Some("replayed") => deduped += 1,
            _ if envelope["kind"] == "op_id_in_flight" => deduped += 1,
            _ => panic!(
                "unexpected racing envelope (success={}): {envelope}",
                output.status.success()
            ),
        }
    }
    assert_eq!(
        executed, 1,
        "exactly one CLI must execute the underlying capture; outputs: {outputs:?}"
    );
    assert_eq!(
        deduped, 1,
        "the losing CLI must surface a dedup-hit envelope (replay or in-flight); outputs: {outputs:?}"
    );

    // After both finish, the dedup store holds exactly one record for the
    // shared op-id — proving the file-level serialization prevented a
    // second `(op-id, verb)` slot from being claimed.
    let store = OperationDedupStore::open(repo_path.join(".heddle")).unwrap();
    assert_eq!(
        store.len(),
        1,
        "exactly one dedup entry should persist for the shared op-id"
    );

    // And the capture's side effect — the committed state — must have
    // landed exactly once. Reusing the op-id should now replay instead
    // of executing a second mutation.
    let replay = heddle(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "capture",
            "-m",
            "concurrent race",
        ],
        Some(temp.path()),
    )
    .expect("replay must succeed after the race resolves");
    let replay_value: Value = serde_json::from_str(&replay).expect("replay JSON");
    assert_eq!(replay_value["idempotency_status"], "replayed");
    assert_eq!(replay_value["replayed"], true);
}

#[test]
fn op_id_replays_first_contact_init_adopt_and_clone() {
    let init_repo = TempDir::new().unwrap();
    let init_op_id = objects::object::OperationId::new().to_string();
    let init_first = json_value(
        init_repo.path(),
        &["--output", "json", "--op-id", &init_op_id, "init"],
    );
    assert_eq!(init_first["action"], "init");
    assert_eq!(init_first["op_id"], init_op_id);
    assert_eq!(init_first["idempotency_status"], "executed");
    let init_replay = json_value(
        init_repo.path(),
        &["--output", "json", "--op-id", &init_op_id, "init"],
    );
    assert_eq!(init_replay["action"], "init");
    assert_eq!(init_replay["idempotency_status"], "replayed");

    let git_repo = TempDir::new().unwrap();
    init_git_repo_for_json_contract(git_repo.path(), "main");
    std::fs::write(git_repo.path().join("seed.txt"), "seed\n").unwrap();
    git_commit_all_for_json_contract(git_repo.path(), "seed");
    let adopt_op_id = objects::object::OperationId::new().to_string();
    let adopt_first = json_value(
        git_repo.path(),
        &["--output", "json", "--op-id", &adopt_op_id, "adopt"],
    );
    assert_eq!(adopt_first["action"], "adopt");
    assert_eq!(adopt_first["op_id"], adopt_op_id);
    assert_eq!(adopt_first["idempotency_status"], "executed");
    let adopt_replay = json_value(
        git_repo.path(),
        &["--output", "json", "--op-id", &adopt_op_id, "adopt"],
    );
    assert_eq!(adopt_replay["action"], "adopt");
    assert_eq!(adopt_replay["idempotency_status"], "replayed");

    let source = TempDir::new().unwrap();
    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("tracked.txt"), "clone me\n").unwrap();
    heddle(&["commit", "-m", "seed"], Some(source.path())).unwrap();
    let clone_parent = TempDir::new().unwrap();
    let clone_dest = clone_parent.path().join("copy");
    let clone_dest_arg = clone_dest.display().to_string();
    let source_arg = source.path().display().to_string();
    let clone_op_id = objects::object::OperationId::new().to_string();
    let clone_first = json_value(
        clone_parent.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &clone_op_id,
            "clone",
            &source_arg,
            &clone_dest_arg,
        ],
    );
    assert_eq!(clone_first["op_id"], clone_op_id);
    assert_eq!(clone_first["idempotency_status"], "executed");
    let clone_replay = json_value(
        clone_parent.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &clone_op_id,
            "clone",
            &source_arg,
            &clone_dest_arg,
        ],
    );
    assert_eq!(clone_replay["idempotency_status"], "replayed");
}

#[test]
fn bootstrap_op_ids_are_scoped_to_first_contact_repo_path() {
    let op_id = "11111111-1111-4111-8111-111111111111";

    let first = TempDir::new().unwrap();
    init_git_repo_for_json_contract(first.path(), "main");
    std::fs::write(first.path().join("seed.txt"), "first\n").unwrap();
    git_commit_all_for_json_contract(first.path(), "seed first");

    let first_adopt = json_value(
        first.path(),
        &[
            "--output", "json", "--op-id", op_id, "adopt", "--ref", "main",
        ],
    );
    assert_eq!(first_adopt["action"], "adopt");
    assert_eq!(first_adopt["op_id"], op_id);
    assert_eq!(first_adopt["idempotency_status"], "executed");

    let second = TempDir::new().unwrap();
    init_git_repo_for_json_contract(second.path(), "main");
    std::fs::write(second.path().join("seed.txt"), "second\n").unwrap();
    git_commit_all_for_json_contract(second.path(), "seed second");

    let second_adopt = json_value(
        second.path(),
        &[
            "--output", "json", "--op-id", op_id, "adopt", "--ref", "main",
        ],
    );
    assert_eq!(second_adopt["action"], "adopt");
    assert_eq!(second_adopt["op_id"], op_id);
    assert_eq!(
        second_adopt["idempotency_status"], "executed",
        "a fresh repo path must not see stale bootstrap op-id conflicts from another repo"
    );

    let conflict = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "adopt",
            "--ref",
            "refs/heads/main",
        ],
        Some(second.path()),
    )
    .expect("invoke same-scope conflicting bootstrap op-id");
    assert!(
        !conflict.status.success(),
        "same-scope conflicting op-id should fail"
    );
    let stderr = std::str::from_utf8(&conflict.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("conflict should be a JSON envelope: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "op_id_conflict");
    assert_eq!(parsed["op_id"], op_id);
    assert_eq!(parsed["recorded_command"], "adopt");
    assert_eq!(parsed["incoming_command"], "adopt");
    assert!(
        parsed["dedup_scope"].as_str().is_some_and(
            |scope| scope.contains(second.path().file_name().unwrap().to_str().unwrap())
        ),
        "bootstrap conflict should name the scoped repo path: {parsed}"
    );
    assert_eq!(parsed["recorded_status"], "completed");
}

#[test]
fn bootstrap_op_id_reused_by_commit_conflicts_before_noop_execution() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    let op_id = objects::object::OperationId::new().to_string();
    let adopt = json_value(
        temp.path(),
        &["--output", "json", "--op-id", &op_id, "adopt"],
    );
    assert_eq!(adopt["action"], "adopt");
    assert_eq!(adopt["idempotency_status"], "executed");

    let conflict = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            &op_id,
            "commit",
            "-m",
            "no-op should not run",
        ],
        Some(temp.path()),
    )
    .expect("invoke commit with reused bootstrap op-id");
    assert!(
        !conflict.status.success(),
        "cross-command op-id reuse should fail before no-op commit execution"
    );
    assert!(
        conflict.stdout.is_empty(),
        "conflicting op-id should not execute commit or write stdout"
    );
    let stderr = std::str::from_utf8(&conflict.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("conflict should be a JSON envelope: {err}: {stderr}"));
    assert_eq!(parsed["kind"], "op_id_conflict");
    assert_eq!(parsed["op_id"], op_id);
    assert_eq!(parsed["idempotency_status"], "conflict");
    assert_eq!(parsed["recorded_command"], "adopt");
    assert_eq!(parsed["incoming_command"], "commit");
    assert_eq!(parsed["recorded_status"], "completed");
}

#[test]
fn op_id_replays_bridge_git_init_and_export() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "export me\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    let init_op_id = objects::object::OperationId::new().to_string();
    let init_first = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &init_op_id,
            "bridge",
            "git",
            "init",
        ],
    );
    assert_eq!(init_first["op_id"], init_op_id);
    assert_eq!(init_first["idempotency_status"], "executed");
    let init_replay = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &init_op_id,
            "bridge",
            "git",
            "init",
        ],
    );
    assert_eq!(init_replay["idempotency_status"], "replayed");

    let export_dest = temp.path().join("export.git");
    let export_dest_arg = export_dest.display().to_string();
    let export_op_id = objects::object::OperationId::new().to_string();
    let export_first = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &export_op_id,
            "bridge",
            "git",
            "export",
            "--destination",
            &export_dest_arg,
        ],
    );
    assert_eq!(export_first["op_id"], export_op_id);
    assert_eq!(export_first["idempotency_status"], "executed");
    let export_replay = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            &export_op_id,
            "bridge",
            "git",
            "export",
            "--destination",
            &export_dest_arg,
        ],
    );
    assert_eq!(export_replay["idempotency_status"], "replayed");
}

#[test]
fn op_id_decorated_stash_push_matches_registered_schema() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440011";

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("stashed.txt"), "stash me\n").unwrap();

    let pushed = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "stash",
            "push",
            "-m",
            "schema op-id stash",
        ],
    );
    assert_eq!(pushed["op_id"], op_id);
    assert_eq!(pushed["idempotency_status"], "executed");
    assert_eq!(pushed["replayed"], false);
    assert_eq!(pushed["operation_record"]["command"], "stash push");
    assert_schema_declares_runtime_top_level(&["stash", "push"], &pushed);
}

#[test]
fn commit_schema_declares_real_op_id_commit_and_replay_fields() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440010";

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "commit me\n").unwrap();

    let first = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "commit",
            "-m",
            "op-id commit",
        ],
    );
    assert_eq!(first["output_kind"], "commit");
    assert_eq!(first["action"], "commit");
    assert_eq!(first["op_id"], op_id);
    assert_eq!(first["idempotency_status"], "executed");
    assert_eq!(first["replayed"], false);
    assert_eq!(first["operation_record"]["op_id"], op_id);
    assert_eq!(first["operation_record"]["command"], "commit");
    assert_eq!(first["operation_record"]["idempotency_status"], "executed");
    assert_eq!(first["operation_record"]["replayed"], false);
    assert_schema_declares_runtime_top_level(&["commit"], &first);

    let replayed = json_value(
        temp.path(),
        &[
            "--output",
            "json",
            "--op-id",
            op_id,
            "commit",
            "-m",
            "op-id commit",
        ],
    );
    assert_eq!(
        replayed["change_id"], first["change_id"],
        "same op-id and args should replay the original commit result"
    );
    assert_eq!(replayed["output_kind"], "commit");
    assert_eq!(replayed["op_id"], op_id);
    assert_eq!(replayed["idempotency_status"], "replayed");
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["operation_record"]["command"], "commit");
    assert_eq!(
        replayed["operation_record"]["idempotency_status"],
        "replayed"
    );
    assert_eq!(replayed["operation_record"]["replayed"], true);
    assert_schema_declares_runtime_top_level(&["commit"], &replayed);
}

#[test]
fn capture_json_reports_recorded_confidence_principal_and_agent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("agent.txt"), "agent work\n").unwrap();

    let output = heddle_output_with_env(
        &[
            "capture",
            "-m",
            "agent save",
            "--confidence",
            "0.9",
            "--output",
            "json",
        ],
        Some(temp.path()),
        &[
            ("HEDDLE_PRINCIPAL_NAME", "Ada Agent"),
            ("HEDDLE_PRINCIPAL_EMAIL", "ada-agent@example.com"),
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5-codex"),
        ],
    )
    .expect("capture should run");
    assert!(
        output.status.success(),
        "capture should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let capture: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("capture JSON should parse: {err}: {stdout}"));

    assert_eq!(capture["output_kind"], "capture");
    assert_eq!(capture["confidence"], serde_json::json!(0.9));
    assert_eq!(capture["principal"]["name"], "Ada Agent");
    assert_eq!(capture["principal"]["email"], "ada-agent@example.com");
    assert_eq!(capture["agent"]["provider"], "codex");
    assert_eq!(capture["agent"]["model"], "gpt-5-codex");
    assert!(capture["agent"].get("session_id").is_none());
    assert!(capture["agent"].get("segment_id").is_none());
    assert!(capture["agent"].get("policy_id").is_none());
    assert_schema_declares_runtime_top_level(&["capture"], &capture);

    let show = json_value(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_eq!(show["confidence"], capture["confidence"]);
    assert_eq!(show["principal"], capture["principal"]);
    assert_eq!(show["agent"]["provider"], capture["agent"]["provider"]);
    assert_eq!(show["agent"]["model"], capture["agent"]["model"]);
    assert!(show["agent"].get("session_id").is_none());
    assert!(show["agent"].get("policy_id").is_none());
}

#[test]
fn commit_json_reports_recorded_confidence_principal_and_agent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("agent.txt"), "agent work\n").unwrap();

    let output = heddle_output_with_env(
        &[
            "commit",
            "-m",
            "agent save",
            "--confidence",
            "0.9",
            "--output",
            "json",
        ],
        Some(temp.path()),
        &[
            ("HEDDLE_PRINCIPAL_NAME", "Ada Agent"),
            ("HEDDLE_PRINCIPAL_EMAIL", "ada-agent@example.com"),
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5-codex"),
        ],
    )
    .expect("commit should run");
    assert!(
        output.status.success(),
        "commit should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("commit JSON should parse: {err}: {stdout}"));

    assert_eq!(commit["output_kind"], "commit");
    assert_eq!(commit["confidence"], serde_json::json!(0.9));
    assert_eq!(commit["principal"]["name"], "Ada Agent");
    assert_eq!(commit["principal"]["email"], "ada-agent@example.com");
    assert_eq!(commit["agent"]["provider"], "codex");
    assert_eq!(commit["agent"]["model"], "gpt-5-codex");
    assert!(commit["agent"].get("session_id").is_none());
    assert!(commit["agent"].get("segment_id").is_none());
    assert!(commit["agent"].get("policy_id").is_none());
    assert_schema_declares_runtime_top_level(&["commit"], &commit);

    let show = json_value(temp.path(), &["show", "HEAD", "--output", "json"]);
    assert_eq!(show["confidence"], commit["confidence"]);
    assert_eq!(show["principal"], commit["principal"]);
    assert_eq!(show["agent"]["provider"], commit["agent"]["provider"]);
    assert_eq!(show["agent"]["model"], commit["agent"]["model"]);
    assert!(show["agent"].get("session_id").is_none());
    assert!(show["agent"].get("policy_id").is_none());
}

#[test]
fn save_text_surfaces_principal_and_agent_attribution() {
    let capture_repo = TempDir::new().unwrap();
    heddle(&["init"], Some(capture_repo.path())).unwrap();
    std::fs::write(capture_repo.path().join("agent.txt"), "agent work\n").unwrap();
    let capture = heddle_output_with_env(
        &[
            "capture",
            "-m",
            "agent save",
            "--confidence",
            "0.9",
            "--output",
            "text",
        ],
        Some(capture_repo.path()),
        &[
            ("NO_COLOR", "1"),
            ("HEDDLE_PRINCIPAL_NAME", "Ada Agent"),
            ("HEDDLE_PRINCIPAL_EMAIL", "ada-agent@example.com"),
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5-codex"),
        ],
    )
    .expect("capture text should run");
    assert!(
        capture.status.success(),
        "capture should succeed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let capture_text = String::from_utf8_lossy(&capture.stdout);
    assert!(
        capture_text.contains("Saved by: Ada Agent <ada-agent@example.com>")
            && capture_text.contains("Agent: codex/gpt-5-codex"),
        "capture text should make attribution visible at save time: {capture_text}"
    );

    let commit_repo = TempDir::new().unwrap();
    heddle(&["init"], Some(commit_repo.path())).unwrap();
    std::fs::write(commit_repo.path().join("agent.txt"), "agent work\n").unwrap();
    let commit = heddle_output_with_env(
        &[
            "commit",
            "-m",
            "agent save",
            "--confidence",
            "0.9",
            "--output",
            "text",
        ],
        Some(commit_repo.path()),
        &[
            ("NO_COLOR", "1"),
            ("HEDDLE_PRINCIPAL_NAME", "Ada Agent"),
            ("HEDDLE_PRINCIPAL_EMAIL", "ada-agent@example.com"),
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5-codex"),
        ],
    )
    .expect("commit text should run");
    assert!(
        commit.status.success(),
        "commit should succeed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let commit_text = String::from_utf8_lossy(&commit.stdout);
    assert!(
        commit_text.contains("Saved by: Ada Agent <ada-agent@example.com>")
            && commit_text.contains("Agent: codex/gpt-5-codex"),
        "commit text should make attribution visible at save time: {commit_text}"
    );
}

#[test]
fn git_overlay_commit_respects_staged_index_and_leaves_extra_work() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "staged\n").unwrap();
    git_ok_for_json_contract(temp.path(), &["add", "file.txt"]);
    std::fs::write(temp.path().join("file.txt"), "staged\nunstaged\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "do not sweep\n").unwrap();

    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Git index and worktree")
            && status_text.contains("will commit staged paths")
            && status_text.contains("will leave unstaged paths")
            && status_text.contains("will leave untracked paths")
            && status_text.contains("plain `heddle commit` checkpoints staged paths only")
            && status_text.contains("heddle commit --all -m \"...\""),
        "status text should explain staged-index commit scope before the user commits: {status_text}"
    );

    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "dirty_worktree", "{verify}");
    assert_eq!(
        verify["recommended_action"], "heddle commit -m \"...\"",
        "verify should recommend the staged-index commit path: {verify}"
    );
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["commit", "-m", "<message>"]),
        "{verify}"
    );

    let before = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);
    let output = heddle_output(
        &["--output", "json", "commit", "-m", "index respect audit"],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        output.status.success(),
        "plain commit should checkpoint only the staged index: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit: Value =
        serde_json::from_slice(&output.stdout).expect("staged commit JSON should parse");
    assert_eq!(commit["git_index"]["commit_mode"], "staged_index");
    assert_eq!(
        commit["git_index"]["will_commit"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        commit["git_index"]["preserved_after_commit"],
        serde_json::json!(["unstaged: file.txt", "untracked: scratch.txt"]),
        "commit JSON should repeat the exact scope status predicted: {commit}"
    );
    assert!(
        commit["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("left 2 unstaged/untracked")),
        "commit summary should disclose preserved extra work: {commit}"
    );
    let after = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(after, before, "staged commit should write a Git commit");
    let committed_file = git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]);
    assert_eq!(
        committed_file, "staged",
        "Git commit should contain the staged index version only"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "staged\nunstaged\n",
        "unstaged edit should remain in the worktree"
    );
    assert!(
        temp.path().join("scratch.txt").exists(),
        "untracked extra work should remain in the worktree"
    );
    let names = git_stdout_for_json_contract(temp.path(), &["show", "--name-only", "--format="]);
    assert!(names.contains("file.txt"));
    assert!(!names.contains("scratch.txt"));
    let porcelain = git_stdout_for_json_contract(temp.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.contains("M file.txt") && porcelain.contains("?? scratch.txt"),
        "remaining work should look like ordinary unstaged Git work: {porcelain}"
    );

    git_ok_for_json_contract(temp.path(), &["add", "file.txt"]);
    let all = heddle_output(
        &[
            "--output",
            "json",
            "commit",
            "--all",
            "-m",
            "index respect audit",
        ],
        Some(temp.path()),
    )
    .expect("commit --all should run");
    assert!(
        all.status.success(),
        "explicit --all should keep the full-worktree Heddle save available: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    let all_commit: Value =
        serde_json::from_slice(&all.stdout).expect("commit --all JSON should parse");
    assert_eq!(
        all_commit["git_index"]["commit_mode"],
        "worktree_all_explicit"
    );
    assert_eq!(
        all_commit["git_index"]["preserved_after_commit"],
        serde_json::json!([])
    );
    let names = git_stdout_for_json_contract(temp.path(), &["show", "--name-only", "--format="]);
    assert!(names.contains("file.txt"));
    assert!(names.contains("scratch.txt"));
}

#[test]
fn git_overlay_commit_empty_index_sweeps_whole_worktree() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    // Nothing staged: a worktree edit plus an untracked file.
    std::fs::write(temp.path().join("file.txt"), "base\nswept\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "untracked\n").unwrap();

    let commit = json_value(
        temp.path(),
        &["commit", "-m", "sweep all", "--output", "json"],
    );
    assert_eq!(
        commit["git_index"]["commit_mode"], "worktree_all",
        "an empty index should commit all worktree paths: {commit}"
    );
    let names = git_stdout_for_json_contract(temp.path(), &["show", "--name-only", "--format="]);
    assert!(
        names.contains("file.txt") && names.contains("scratch.txt"),
        "empty-index commit should sweep both the edited and the untracked path: {names}"
    );
}

#[test]
fn git_overlay_commit_no_all_empty_index_refuses_nothing_to_commit() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    // Nothing genuinely staged: the index matches HEAD. Only worktree edits +
    // an untracked file exist, so the worktree is dirty.
    std::fs::write(temp.path().join("file.txt"), "base\nworktree edit\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "untracked\n").unwrap();

    let before = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);

    // `--no-all` is index-only. With the index identical to HEAD there is
    // nothing staged, so it must refuse with nothing-to-commit rather than
    // writing a spurious empty / index-identical Git checkpoint.
    let output = heddle_output(
        &["--output", "json", "commit", "--no-all", "-m", "index only"],
        Some(temp.path()),
    )
    .expect("commit --no-all should run");
    assert!(
        !output.status.success(),
        "commit --no-all with an empty index must refuse, not create a commit: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(
        envelope["kind"], "nothing_to_commit",
        "--no-all with no staged changes must surface nothing-to-commit: {envelope}"
    );

    // HEAD is unchanged: no spurious commit was created.
    let after = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        before, after,
        "--no-all must not create a commit when nothing is staged"
    );
    let head_file = git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]);
    assert_eq!(
        head_file, "base",
        "--no-all must not sweep worktree edits into a commit"
    );
    // The worktree edits remain untouched.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "base\nworktree edit\n",
        "the worktree edit should remain after --no-all"
    );
    assert!(
        temp.path().join("scratch.txt").exists(),
        "the untracked file should remain after --no-all"
    );
}

#[test]
fn git_overlay_commit_no_all_with_real_staged_changes_commits_index_only() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    // Genuinely stage a change, then add an unstaged edit + untracked file on
    // top so the worktree is dirty beyond the index.
    std::fs::write(temp.path().join("file.txt"), "staged\n").unwrap();
    git_ok_for_json_contract(temp.path(), &["add", "file.txt"]);
    std::fs::write(temp.path().join("file.txt"), "staged\nunstaged\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "do not sweep\n").unwrap();

    let before = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);
    let output = heddle_output(
        &["--output", "json", "commit", "--no-all", "-m", "index only"],
        Some(temp.path()),
    )
    .expect("commit --no-all should run");
    assert!(
        output.status.success(),
        "commit --no-all with real staged changes should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit: Value =
        serde_json::from_slice(&output.stdout).expect("commit --no-all JSON should parse");
    assert_eq!(
        commit["git_index"]["commit_mode"], "staged_index",
        "--no-all must report an index-only commit: {commit}"
    );
    assert_eq!(
        commit["git_index"]["will_commit"],
        serde_json::json!(["file.txt"]),
        "--no-all must commit only the staged path: {commit}"
    );
    assert_eq!(
        commit["git_index"]["preserved_after_commit"],
        serde_json::json!(["unstaged: file.txt", "untracked: scratch.txt"]),
        "--no-all must preserve the unstaged/untracked worktree paths: {commit}"
    );

    let after = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);
    assert_ne!(before, after, "--no-all should write a Git commit");
    // The commit reflects the staged index version only.
    let head_file = git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]);
    assert_eq!(
        head_file, "staged",
        "--no-all must commit the staged index content, not the worktree edit"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "staged\nunstaged\n",
        "the unstaged edit should remain in the worktree"
    );
}

#[test]
fn git_overlay_commit_no_all_does_not_checkpoint_pending_capture() {
    // Regression for the clean-worktree `needs_checkpoint` fast-path: after a
    // `heddle capture`, the worktree matches Heddle's tree (status is clean)
    // while Git HEAD is behind, so commit would normally checkpoint the
    // captured worktree change into Git. `--no-all` must force an INDEX-ONLY
    // commit and refuse to auto-checkpoint that pending capture.
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    // Capture a worktree edit into Heddle only. The Git index/HEAD still match
    // the seed commit, and the worktree now matches Heddle's captured tree, so
    // commit sees a clean worktree with an empty index that nonetheless needs a
    // Git checkpoint.
    std::fs::write(temp.path().join("file.txt"), "base\ncaptured\n").unwrap();
    heddle(&["capture", "-m", "recoverable save"], Some(temp.path())).unwrap();
    assert_eq!(
        git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]),
        "base",
        "capture must not have moved Git HEAD"
    );

    // `--no-all`: index is empty, so there is nothing to commit. The captured
    // worktree change must NOT be written into Git.
    let output = heddle_output(
        &["--output", "json", "commit", "--no-all", "-m", "index only"],
        Some(temp.path()),
    )
    .expect("commit --no-all should run");
    assert!(
        !output.status.success(),
        "commit --no-all with an empty index must refuse with nothing-to-commit: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).unwrap_or_else(|err| panic!("stderr JSON: {err}: {stderr}"));
    assert_eq!(
        envelope["kind"], "nothing_to_commit",
        "--no-all must surface nothing-to-commit, not a silent capture checkpoint: {envelope}"
    );
    assert_eq!(
        git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]),
        "base",
        "--no-all must not checkpoint the pending capture into Git"
    );
}

#[test]
fn git_overlay_commit_without_no_all_checkpoints_pending_capture() {
    // Contrast: the same clean-worktree `needs_checkpoint` scenario WITHOUT
    // `--no-all` must still checkpoint the pending capture into Git (the
    // default fast-path behavior).
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "base\ncaptured\n").unwrap();
    let capture = json_value(
        temp.path(),
        &["capture", "-m", "recoverable save", "--output", "json"],
    );
    let captured_state = capture["change_id"]
        .as_str()
        .expect("capture should report change id")
        .to_string();

    let commit = json_value(
        temp.path(),
        &["commit", "-m", "checkpoint capture", "--output", "json"],
    );
    assert_eq!(
        commit["included_pending_capture"], captured_state,
        "without --no-all the fast-path should checkpoint the pending capture: {commit}"
    );
    assert_eq!(
        git_stdout_for_json_contract(temp.path(), &["show", "HEAD:file.txt"]),
        "base\ncaptured",
        "without --no-all the captured worktree change must land in Git"
    );
}

#[test]
fn commit_help_surfaces_index_vs_worktree_auto_switch() {
    let commit = heddle(&["commit", "--help"], None).expect("heddle commit --help should render");
    assert!(
        commit.contains("auto-switches on the Git index")
            && commit.contains("with nothing staged it commits all worktree paths")
            && commit.contains("with staged paths it commits only the index")
            && commit.contains("--no-all"),
        "commit help should surface the accurate index-vs-worktree auto-switch: {commit}"
    );
    assert!(
        !commit.contains("by default behaves like `git commit -a`"),
        "commit help must not use the misleading default-is-`git commit -a` framing: {commit}"
    );
}

#[test]
fn git_overlay_commit_discloses_pending_capture_when_checkpointing_later_delta() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "captured\n").unwrap();
    let capture = json_value(
        temp.path(),
        &["capture", "-m", "recoverable save", "--output", "json"],
    );
    let captured_state = capture["change_id"]
        .as_str()
        .expect("capture should report change id")
        .to_string();
    let previous_git = git_stdout_for_json_contract(temp.path(), &["rev-parse", "HEAD"]);

    std::fs::write(temp.path().join("file.txt"), "captured\nthen committed\n").unwrap();
    let commit = json_value(
        temp.path(),
        &["commit", "-m", "checkpoint later delta", "--output", "json"],
    );

    assert_eq!(
        commit["included_pending_capture"], captured_state,
        "commit should disclose that it checkpointed work on top of an earlier Heddle-only save: {commit}"
    );
    assert_eq!(
        commit["git_previous_commit"], previous_git,
        "commit should expose the Git commit that HEAD moved from: {commit}"
    );
    assert_ne!(
        commit["git_commit"], commit["git_previous_commit"],
        "commit should expose Git commit movement when checkpointing: {commit}"
    );
    assert_eq!(commit["git_index"]["commit_mode"], "worktree_all");
    assert_eq!(
        commit["git_index"]["will_commit"],
        serde_json::json!(["file.txt"])
    );
    assert_eq!(
        commit["verification"]["verified"], true,
        "commit should finish with a clean verification report: {commit}"
    );
}

#[test]
fn git_overlay_commit_updates_head_reflog_for_git_muscle_memory() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "heddle commit\n").unwrap();
    heddle(&["commit", "-m", "reflog audit"], Some(temp.path())).unwrap();

    let head_reflog = git_stdout_for_json_contract(temp.path(), &["reflog", "-1", "--format=%gs"]);
    let branch_reflog = git_stdout_for_json_contract(
        temp.path(),
        &["reflog", "show", "main", "-1", "--format=%gs"],
    );
    assert!(
        head_reflog.contains("heddle: write-through current thread"),
        "HEAD reflog should show the Heddle movement: {head_reflog}"
    );
    assert!(
        branch_reflog.contains("heddle: write-through current thread"),
        "branch reflog should still show the Heddle movement: {branch_reflog}"
    );
}

fn git_stdout_for_json_contract(path: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_ok_for_json_contract(path: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unsupported_op_id_fails_from_command_contract_table() {
    let temp = TempDir::new().unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440001";

    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "--op-id", op_id, "status"],
        Some(temp.path()),
    )
    .expect("invoke status with unsupported op-id");
    assert!(
        !output.status.success(),
        "read-only status must reject unsupported op-id"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let parsed: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("unsupported op-id should be a JSON envelope: {err}: {stderr}")
    });
    assert_eq!(parsed["kind"], "op_id_unsupported");
    assert!(
        parsed["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle commands --output json")),
        "unsupported op-id should point to the command catalog: {parsed}"
    );
}

#[test]
fn invalid_op_id_fails_before_mutating_commit() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "initial");
    heddle(&["adopt"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "two\n").unwrap();
    std::fs::write(temp.path().join("new.txt"), "new\n").unwrap();

    let before = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(before["status"], "dirty_worktree");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            "agent-cold-commit-1",
            "commit",
            "-m",
            "agent cold commit",
            "--confidence",
            "0.77",
        ],
        Some(temp.path()),
    )
    .expect("invoke invalid op-id commit");
    assert!(!output.status.success(), "invalid op-id should fail");
    assert!(
        output.stdout.is_empty(),
        "invalid op-id failure must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("invalid op-id should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "op_id_invalid");
    assert_eq!(envelope["op_id"], "agent-cold-commit-1");
    assert_eq!(envelope["idempotency_status"], "invalid");
    assert_eq!(envelope["replayed"], false);
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("agent-cold-commit-1")),
        "invalid op-id refusal should use typed recovery detail: {stderr}"
    );

    let after = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(
        after["status"], "dirty_worktree",
        "invalid op-id must not capture, checkpoint, or advance verify state: {after}"
    );
    assert_eq!(
        after["recommended_action"], before["recommended_action"],
        "invalid op-id should leave the recommended action unchanged"
    );
}

#[test]
fn op_id_replays_terminal_failure_and_reports_in_flight() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let terminal_op_id = "550e8400-e29b-41d4-a716-446655440002";
    let terminal_args = [
        "--output",
        "json",
        "--op-id",
        terminal_op_id,
        "thread",
        "drop",
        "missing-thread",
    ];
    let first = heddle_output(&terminal_args, Some(temp.path())).expect("invoke first failure");
    assert!(
        !first.status.success(),
        "missing thread drop should fail before replay"
    );
    assert!(
        first.stdout.is_empty(),
        "JSON-mode terminal failure should keep stdout quiet: {}",
        String::from_utf8_lossy(&first.stdout)
    );
    let first_stderr = std::str::from_utf8(&first.stderr).unwrap();
    let first_envelope: Value = serde_json::from_str(first_stderr)
        .unwrap_or_else(|err| panic!("first failure should be JSON: {err}: {first_stderr}"));
    assert_eq!(first_envelope["kind"], "thread_not_found");
    assert_eq!(first_envelope["op_id"], terminal_op_id);
    assert_eq!(first_envelope["idempotency_status"], "executed");
    assert_eq!(first_envelope["replayed"], false);

    let replay = heddle_output(&terminal_args, Some(temp.path())).expect("invoke replay failure");
    assert_eq!(
        replay.status.code(),
        first.status.code(),
        "terminal op-id replay should preserve the original exit code"
    );
    assert_eq!(
        replay.stdout, first.stdout,
        "terminal op-id replay should preserve stdout exactly"
    );
    let replay_stderr = std::str::from_utf8(&replay.stderr).unwrap();
    let replay_envelope: Value = serde_json::from_str(replay_stderr)
        .unwrap_or_else(|err| panic!("replay failure should be JSON: {err}: {replay_stderr}"));
    assert_eq!(replay_envelope["kind"], first_envelope["kind"]);
    assert_eq!(replay_envelope["op_id"], terminal_op_id);
    assert_eq!(replay_envelope["idempotency_status"], "replayed");
    assert_eq!(replay_envelope["replayed"], true);

    let pending_op_id = "550e8400-e29b-41d4-a716-446655440003";
    let parsed_pending_op_id = pending_op_id.parse().expect("valid op id");
    let repo = Repository::open(temp.path()).expect("repo should open");
    let store = OperationDedupStore::open(repo.heddle_dir()).expect("open op-id store");
    let request_hash = hash_request_body(b"--output\0json\0thread\0drop\0pending-thread");
    let reserved = store
        .reserve(parsed_pending_op_id, "thread drop", request_hash)
        .expect("reserve pending op-id");
    assert!(
        matches!(reserved, repo::operation_dedup::DedupOutcome::Reserved),
        "test setup should reserve a fresh op-id slot"
    );

    let in_flight = heddle_output(
        &[
            "--output",
            "json",
            "--op-id",
            pending_op_id,
            "thread",
            "drop",
            "pending-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke in-flight op-id");
    assert!(
        !in_flight.status.success(),
        "in-flight op-id should fail closed"
    );
    assert!(
        in_flight.stdout.is_empty(),
        "op-id in-flight refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&in_flight.stdout)
    );
    let stderr = std::str::from_utf8(&in_flight.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("in-flight refusal should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "op_id_in_flight");
    assert_eq!(envelope["op_id"], pending_op_id);
    assert_eq!(envelope["idempotency_status"], "in_flight");
    assert_eq!(envelope["replayed"], false);
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("currently being executed")),
        "op-id in-flight refusal should use typed recovery detail: {stderr}"
    );
}

#[test]
fn attempt_invalid_count_uses_typed_advice_json() {
    let output = heddle_output(&["--output", "json", "attempt", "0", "--", "true"], None)
        .expect("invoke invalid attempt count");
    assert!(!output.status.success(), "attempt 0 should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode attempt refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("attempt count refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "attempt_count_invalid");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("N must be at least 1")),
        "attempt count refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("attempt 1")),
        "attempt count hint should name a valid retry: {stderr}"
    );
}

#[test]
fn watch_empty_since_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "watch", "--since", ""],
        Some(temp.path()),
    )
    .expect("invoke watch with empty since");
    assert!(!output.status.success(), "watch --since '' should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode watch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("watch since refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "watch_since_empty");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("--since cannot be empty")),
        "watch since refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("30s") && hint.contains("5m")),
        "watch since hint should name valid durations: {stderr}"
    );
}

#[test]
fn query_reads_live_oplog_before_operation_index_is_warm() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("story.txt"), "query should see this\n").unwrap();
    heddle(&["capture", "-m", "seed query event"], Some(temp.path())).unwrap();

    let query = json_value(
        temp.path(),
        &["query", "--include-checkpoints", "--output", "json"],
    );
    assert_eq!(query["output_kind"], "query");
    let hits = query["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("query should emit hits array: {query}"));
    assert!(
        hits.iter()
            .any(|hit| hit["verb"] == "snapshot" && hit["change_id"].is_string()),
        "query should fall back to the live oplog when the sidecar index is empty: {query}"
    );
}

#[test]
fn core_loop_schemas_are_discoverable() {
    for verb in [
        "init",
        "capture",
        "commit",
        "checkpoint",
        "doctor",
        "doctor docs",
        "doctor schemas",
        "diff",
        "git-overlay",
        "actor spawn",
        "actor list",
        "actor show",
        "actor explain",
        "actor done",
        "agent serve",
        "agent status",
        "agent stop",
        "agent reserve",
        "agent heartbeat",
        "agent capture",
        "agent ready",
        "agent release",
        "agent list",
        "branch",
        "switch",
        "checkout",
        "bridge git reconcile",
        "remote list",
        "remote show",
        "remote add",
        "remote remove",
        "remote set-default",
        "schemas",
        "session start",
        "session segment",
        "session end",
        "session show",
        "session list",
        "fetch",
        "pull",
        "push",
        "stash push",
        "stash list",
        "stash pop",
        "stash apply",
        "stash drop",
        "stash clear",
        "stash show",
        "revert",
        "ship",
        "start",
        "thread create",
        "thread current",
        "thread switch",
        "thread captures",
        "thread rename",
        "thread refresh",
        "thread drop",
        "thread show",
        "try",
        "undo",
        "redo",
        "version",
        "watch",
    ] {
        let mut args = vec!["schemas"];
        args.extend(verb.split_whitespace());
        let json = heddle(&args, None).unwrap_or_else(|err| panic!("schema for {verb}: {err}"));
        let parsed: Value = serde_json::from_str(&json)
            .unwrap_or_else(|err| panic!("schema for {verb} should parse: {err}: {json}"));
        assert!(
            parsed.get("title").is_some(),
            "schema for {verb} should have a title: {parsed}"
        );
    }
}

#[test]
fn review_next_envelope_top_level_keys_match_registered_schema() {
    // The output_kind sweep wrapped `review next --output json` in a
    // stable envelope (`output_kind` + the flattened pending view +
    // `next`), replacing the old bare `NextStateView`/top-level-`null`
    // shape. Pin the wire contract against the registered schema mirror
    // so the doc/schema can't drift back. A fresh capture carries no
    // signatures, so it surfaces as the pending state — exercising the
    // richer flattened envelope (all five top-level keys), a superset of
    // the empty `{output_kind, next: null}` case.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("review.txt"), "needs review\n").unwrap();
    heddle(&["capture", "-m", "seed review"], Some(temp.path())).unwrap();

    let value = json_value(temp.path(), &["review", "next"]);
    assert_eq!(value["output_kind"], "review_next");
    assert!(
        value.get("next").is_some(),
        "review next must always emit a `next` field (object or null): {value}"
    );
    assert_schema_declares_runtime_top_level(&["review", "next"], &value);
}

#[test]
fn isolated_thread_json_outputs_match_registered_schemas() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "schema-checkout");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    let start = json_value(
        temp.path(),
        &[
            "start",
            "feature/schema-contract",
            "--path",
            checkout_arg,
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["start"], &start);
    assert!(
        start.get("verification").is_some(),
        "start should prove its post-mutation verify state for agents: {start}"
    );

    let create = json_value(
        temp.path(),
        &["thread", "create", "feature/schema-ref", "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["thread", "create"], &create);
    assert!(
        create.get("verification").is_some(),
        "thread create should prove post-mutation verify: {create}"
    );

    let current = json_value(temp.path(), &["thread", "current", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["thread", "current"], &current);

    let captures = json_value(
        temp.path(),
        &["thread", "captures", "main", "--output", "json"],
    );
    assert!(
        captures.as_array().is_some(),
        "thread captures should emit an array schema surface: {captures}"
    );

    let switch = json_value(
        temp.path(),
        &["thread", "switch", "feature/schema-ref", "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["thread", "switch"], &switch);
    assert!(
        switch.get("verification").is_some(),
        "thread switch should prove post-mutation verify: {switch}"
    );

    let rename = json_value(
        temp.path(),
        &[
            "thread",
            "rename",
            "feature/schema-ref",
            "feature/schema-renamed",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["thread", "rename"], &rename);

    let show = json_value(
        temp.path(),
        &[
            "thread",
            "show",
            "feature/schema-contract",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["thread", "show"], &show);
    assert_eq!(show["output_kind"], "thread_show");
    // `session_id`, `native_actor_key`, `probe_source` are populated
    // by harness probes only when an AI tool (Claude Code / Codex /
    // OpenCode) is the active parent — they are
    // `skip_serializing_if = "Option::is_none"`, so they're absent in
    // bare CI environments. The schema-registration check above pins
    // their declared shape; runtime population is exercised in the
    // probe-specific tests under `crates/cli/src/harness/probe/`.
    assert!(show.get("next_action").is_some());
    assert!(show.get("next_action_template").is_some());
    assert!(show.get("recommended_action_template").is_some());
    assert!(
        show.get("verification").is_some() && show.get("recovery_commands").is_some(),
        "thread show schema-critical verify fields should be present at runtime: {show}"
    );

    let text = heddle(
        &[
            "thread",
            "show",
            "feature/schema-contract",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !text.contains("Base root:") && !text.contains("Base tree:"),
        "non-verbose thread show should not surface raw tree hashes in the human path: {text}"
    );
    let verbose = heddle(
        &[
            "-v",
            "thread",
            "show",
            "feature/schema-contract",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        verbose.contains("Base tree:") && !verbose.contains("Base root:"),
        "verbose thread show should expose the debug tree hash with a clearer label: {verbose}"
    );
}

#[test]
fn core_git_overlay_json_surfaces_emit_one_machine_value() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked changed\n").unwrap();
    json_value(
        temp.path(),
        &["--output", "json", "commit", "-m", "checkpoint"],
    );

    for (label, args) in [
        ("commands", vec!["commands", "--output", "json"]),
        ("schemas status", vec!["schemas", "status"]),
        ("status", vec!["status", "--output", "json"]),
        ("diagnose", vec!["diagnose", "--output", "json"]),
        ("doctor", vec!["doctor", "--output", "json"]),
        ("verify", vec!["verify", "--output", "json"]),
        (
            "bridge git status",
            vec!["bridge", "git", "status", "--output", "json"],
        ),
        ("log", vec!["log", "--output", "json"]),
        ("show", vec!["show", "HEAD", "--output", "json"]),
        ("thread list", vec!["thread", "list", "--output", "json"]),
        (
            "thread show",
            vec!["thread", "show", "main", "--output", "json"],
        ),
        (
            "workspace show",
            vec!["workspace", "show", "--output", "json"],
        ),
        ("diff", vec!["diff", "--output", "json"]),
        ("ready", vec!["ready", "--output", "json"]),
    ] {
        let output = heddle_output(&args, Some(temp.path()))
            .unwrap_or_else(|err| panic!("invoke {label}: {err}"));
        assert!(
            output.status.success(),
            "{label} should succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{label} JSON success should keep stderr quiet: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = std::str::from_utf8(&output.stdout).expect("stdout should be utf8");
        let parsed = parse_exactly_one_json_value(stdout)
            .unwrap_or_else(|err| panic!("{label} should emit one JSON value: {err}: {stdout}"));
        assert!(
            parsed.is_object(),
            "{label} should emit a JSON object machine contract: {parsed}"
        );
    }
}

#[test]
fn captured_git_overlay_work_recommends_checkpoint_not_recapture() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked changed\n").unwrap();
    let capture_text = heddle(
        &[
            "capture",
            "-m",
            "captured but not checkpointed",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        capture_text.contains("Next:") && capture_text.contains("heddle checkpoint -m \"...\""),
        "Git-overlay capture should point to the concrete checkpoint step: {capture_text}"
    );
    assert!(
        !capture_text.contains("agent-style saves"),
        "human capture output should not leak agent-oriented copy: {capture_text}"
    );
    assert!(
        !capture_text.contains("Confidence:"),
        "human capture without an explicit confidence should not render an empty confidence field: {capture_text}"
    );

    let status = json_value(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "needs_checkpoint");
    assert_eq!(status["thread_health"], "needs_checkpoint");
    assert_ne!(
        status["coordination_status"], "blocked",
        "captured work that is already saved in Heddle should not make the thread coordination look blocked: {status}"
    );
    assert_ne!(
        status["thread_state"], "blocked",
        "captured work that only needs a Git checkpoint should not rewrite lifecycle as blocked: {status}"
    );
    assert_eq!(status["recommended_action"], "heddle checkpoint -m \"...\"");
    assert!(
        status["verification"]["recommended_action_template"]["required_inputs"]
            .as_array()
            .is_some_and(|inputs| !inputs.is_empty()),
        "templated checkpoint advice must stay display-only until a message is supplied: {status}"
    );
    assert_eq!(
        status["recovery_commands"],
        serde_json::json!(["heddle checkpoint -m \"...\""])
    );
    assert_eq!(
        status["recovery_action_templates"], status["verification"]["recovery_action_templates"],
        "top-level status recovery templates should match verification so agents do not have to mine nested state: {status}"
    );
    assert_eq!(
        status["recovery_action_templates"][0]["argv_template"],
        heddle_argv_json(["checkpoint", "-m", "<message>"]),
        "templated checkpoint recovery should be machine-fillable at top level: {status}"
    );
    let thread_list = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(
        thread_list["recommended_action"], "heddle checkpoint -m \"...\"",
        "thread list should use the same verification blocker as status: {thread_list}"
    );
    assert_eq!(
        thread_list["recommended_action_template"]["argv_template"],
        heddle_argv_json(["checkpoint", "-m", "<message>"]),
        "thread list top-level placeholder action should be machine-fillable: {thread_list}"
    );
    assert_eq!(
        thread_list["recovery_action_templates"], status["recovery_action_templates"],
        "thread list recovery templates should match status/verify: {thread_list}"
    );
    let workspace = json_value(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(
        workspace["recommended_action"], "heddle checkpoint -m \"...\"",
        "workspace should use the same verification blocker as status: {workspace}"
    );
    assert_eq!(
        workspace["recommended_action_template"]["argv_template"],
        heddle_argv_json(["checkpoint", "-m", "<message>"]),
        "workspace top-level placeholder action should be machine-fillable: {workspace}"
    );
    assert_eq!(status["verification"]["worktree_dirty"], true);
    assert_eq!(status["changed_path_count"], 0);
    assert_eq!(status["changes"]["modified"], serde_json::json!([]));
    assert!(
        status["git_overlay_health"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["status"] == "needs_checkpoint"
                && check["details"]["dirty_paths"] == "tracked.txt"),
        "git overlay health should name the Git-dirty path already captured by Heddle: {status}"
    );

    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("Verdict: checkpoint needed")
            && status_text
                .contains("Git checkpoint pending: saved Heddle state is not yet a Git commit")
            && status_text.contains("Saved in Heddle")
            && status_text.contains("ready to checkpoint to Git")
            && !status_text.contains("Changed paths: 0")
            && !status_text.contains("Coordination: blocked")
            && !status_text.contains("Lifecycle: blocked"),
        "captured-but-not-checkpointed status should feel saved locally, not blocked: {status_text}"
    );

    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "needs_checkpoint");
    assert_eq!(verify["recommended_action"], "heddle checkpoint -m \"...\"");
    assert!(
        verify["recommended_action_template"]["required_inputs"]
            .as_array()
            .is_some_and(|inputs| !inputs.is_empty()),
        "templated checkpoint advice must stay display-only until a message is supplied: {verify}"
    );
    assert!(
        verify["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Worktree"
                && check["status"] == "needs_checkpoint"
                && check["details"]["dirty_paths"] == "tracked.txt"),
        "verify JSON should expose dirty-path details at the top-level check surface: {verify}"
    );

    let commit = heddle_output(
        &[
            "commit",
            "-m",
            "checkpoint captured work",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .expect("commit should run");
    assert!(
        commit.status.success(),
        "commit should checkpoint already captured work: stdout={} stderr={}",
        String::from_utf8_lossy(&commit.stdout),
        String::from_utf8_lossy(&commit.stderr)
    );
    let stdout = String::from_utf8_lossy(&commit.stdout);
    assert!(
        stdout.contains("Included prior Heddle-only save")
            && stdout.contains("Git HEAD moved:")
            && stdout.contains("Verification: clean"),
        "captured-but-not-checkpointed commit should complete the checkpoint: {stdout}"
    );
    let clean_after_commit = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(
        clean_after_commit["verified"], true,
        "commit should restore verify after checkpointing captured work: {clean_after_commit}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "captured again\n").unwrap();
    let capture_again = json_value(
        temp.path(),
        &["capture", "-m", "captured again", "--output", "json"],
    );
    let captured_again = capture_again["change_id"]
        .as_str()
        .expect("capture should report a change id")
        .to_string();
    let commit_json = heddle_output(
        &[
            "commit",
            "-m",
            "json checkpoint captured work",
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("commit json should run");
    assert!(
        commit_json.status.success(),
        "json commit should checkpoint already captured work"
    );
    let committed: serde_json::Value = inject_post_verification_at(
        temp.path(),
        &["commit"],
        serde_json::from_slice(&commit_json.stdout)
            .expect("captured-but-not-checkpointed commit should emit JSON success"),
    );
    assert_eq!(committed["included_pending_capture"], captured_again);
    assert_eq!(committed["verification"]["verified"], true);
    assert_eq!(
        committed["git_index"],
        serde_json::Value::Null,
        "checkpointing a captured-clean state should not claim a new Git index capture: {committed}"
    );

    let clean = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(
        clean["verified"], true,
        "commit should restore verify: {clean}"
    );
    let git_short = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(temp.path())
        .output()
        .expect("git status should run");
    assert!(
        git_short.status.success(),
        "git status should succeed: {}",
        String::from_utf8_lossy(&git_short.stderr)
    );
    assert!(
        String::from_utf8_lossy(&git_short.stdout).trim().is_empty(),
        "commit should leave Git clean: {}",
        String::from_utf8_lossy(&git_short.stdout)
    );
}

#[test]
fn verify_reports_machine_contract_coverage() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    heddle(&["adopt"], Some(temp.path())).unwrap();
    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["clean"], verify["verified"]);
    assert!(
        verify["summary"]
            .as_str()
            .is_some_and(|summary| !summary.is_empty()),
        "verify JSON should carry a decisive top-level summary: {verify}"
    );
    let coverage = &verify["machine_contract_coverage"];
    assert!(
        coverage.is_object(),
        "verify should expose structured machine contract coverage: {verify}"
    );
    assert_eq!(coverage["status"], "available");
    assert_eq!(coverage["verified_scope"], "everyday_and_agent");
    assert_eq!(coverage["advanced_scope"], "advanced_internal_admin");
    assert_eq!(verify["machine_contract"], "available");
    assert_eq!(verify["verified"], true);
    assert_eq!(verify["status"], "clean");
    assert_eq!(verify["recommended_action"], Value::Null);
    assert_eq!(verify["recommended_action_argv"], Value::Null);
    assert_eq!(verify["recovery_commands"], serde_json::json!([]));
    assert!(
        verify.get("verification").is_none(),
        "verify JSON should be the canonical flattened proof, not a nested wrapper: {verify}"
    );
    assert!(
        coverage["catalog_commands_total"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "machine contract should count catalog commands: {verify}"
    );
    assert!(
        coverage["json_commands_total"].as_u64().unwrap_or_default() > 0,
        "machine contract should count JSON-capable commands: {verify}"
    );
    assert!(
        coverage["json_commands_with_schema"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "machine contract should count concrete schema-backed commands: {verify}"
    );
    assert!(
        coverage["json_commands_with_accepted_opaque_schema"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "machine contract should count advanced opaque schemas separately: {verify}"
    );
    assert!(
        coverage["verified_scope_json_commands_total"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "machine contract should expose the verified advertised scope: {verify}"
    );
    assert_eq!(
        coverage["verified_scope_json_commands_with_accepted_opaque_schema"], 0,
        "verified advertised scope should not rely on opaque schemas: {verify}"
    );
    assert!(
        coverage["advanced_scope_json_commands_with_accepted_opaque_schema"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "advanced scope should segment opaque schemas outside clean verify: {verify}"
    );
    assert_eq!(
        coverage["verified_scope_json_commands_without_schema"], 0,
        "verified advertised scope should have schemas for every JSON command: {verify}"
    );
    assert_eq!(
        coverage["json_commands_without_schema"],
        serde_json::json!(
            coverage["json_commands_total"].as_u64().unwrap()
                - coverage["json_commands_with_schema"].as_u64().unwrap()
                - coverage["json_commands_with_accepted_opaque_schema"]
                    .as_u64()
                    .unwrap()
        ),
        "schema gap count should be derived from catalog coverage: {verify}"
    );
    assert!(
        coverage["supports_op_id_total"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "machine contract should count op-id capable commands: {verify}"
    );
    assert!(
        coverage.get("persists_op_id_total").is_none(),
        "zero generated-resume op-id aggregate should not appear in verify coverage: {verify}"
    );
    assert_eq!(coverage["undocumented_schema_verbs_total"], 0);
    assert!(
        coverage["accepted_opaque_schema_verbs_total"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "advanced generic schemas should be explicit accepted opaque coverage, not counted as concrete: {verify}"
    );
    assert_eq!(coverage["unaccepted_opaque_schema_verbs_total"], 0);
    assert!(
        verify["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "Machine contract"
                && check["status"] == "available"
                && check["clean"] == true
                && check["recommended_action"] == serde_json::Value::Null
                && check["details"]["coverage_status"] == coverage["status"]
                && check["details"]["json_commands_total"].as_str()
                    == coverage["json_commands_total"]
                        .as_u64()
                        .map(|n| n.to_string())
                        .as_deref()),
        "machine contract check should mirror coverage details: {verify}"
    );
}

#[test]
fn verify_text_reports_runtime_contract_cleanly_and_blocked_checks_honestly() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let verify_output = heddle_output(&["verify", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !verify_output.status.success(),
        "verify should exit nonzero until import is complete"
    );
    let verify = String::from_utf8_lossy(&verify_output.stdout);
    assert!(
        verify.contains("observe-only"),
        "default human verify should say it is observe-only: {verify}"
    );
    assert!(
        !verify.lines().any(|line| line.contains("Machine contract")),
        "default clean human verify should keep machine-contract internals out of first-contact text: {verify}"
    );
    assert!(
        !verify.contains("missing_schema_examples")
            && !verify.contains("available_with_schema_gaps")
            && !verify.contains("available_with_doc_gaps")
            && !verify.contains("schemas partial")
            && !verify.contains("missing schemas"),
        "default human verify should keep schema registry internals out of the first-contact view: {verify}"
    );
    let verbose_output = heddle_output(
        &["verify", "--verbose", "--output", "text"],
        Some(temp.path()),
    )
    .expect("verbose verify should run");
    assert!(
        !verbose_output.status.success(),
        "verbose verify should preserve strict exit semantics"
    );
    let verbose_verify = String::from_utf8_lossy(&verbose_output.stdout);
    assert!(
        verbose_verify.contains("observe-only"),
        "verbose human verify should say it is observe-only: {verbose_verify}"
    );
    assert!(
        verbose_verify.contains("Machine contract")
            && verbose_verify.contains("coverage_status=available")
            && verbose_verify.contains("verified_scope=everyday_and_agent")
            && verbose_verify.contains("json_commands_without_schema=0"),
        "verbose verify should expose runtime schema coverage details: {verbose_verify}"
    );
    assert!(
        !verify.contains("Checkout"),
        "default human verify should keep the checklist behind verbose proof: {verify}"
    );
    assert!(
        !verify.contains("Checkout           blocked Git checkout and Heddle mapping agree"),
        "blocked checkout verification must not reuse the success summary: {verify}"
    );

    let parsed = json_value(temp.path(), &["verify", "--output", "json"]);
    assert!(
        parsed["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "Worktree" && check["status"] == "not_checked" }),
        "verify should not report Worktree ok before the primary import blocker is resolved: {parsed}"
    );
}

#[test]
fn commit_without_default_remote_does_not_recommend_unconfigured_push() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "tracked changed\n").unwrap();
    let commit_text = heddle(
        &["commit", "-m", "local checkpoint", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !commit_text.contains("Next: heddle push"),
        "commit should not recommend a default push when no default remote is configured: {commit_text}"
    );
    assert!(
        commit_text.contains("Verification: clean"),
        "commit should still end with a calm clean proof when no next action is known: {commit_text}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "tracked changed again\n").unwrap();
    let commit_json = json_value(
        temp.path(),
        &["commit", "-m", "local checkpoint json", "--output", "json"],
    );
    assert_eq!(commit_json["next_action"], Value::Null);
    assert_eq!(commit_json["next_action_argv"], Value::Null);
    assert_eq!(commit_json["next_action_template"], Value::Null);
    assert_eq!(commit_json["recommended_action"], Value::Null);
    assert_eq!(commit_json["recommended_action_argv"], Value::Null);
    assert_eq!(commit_json["recommended_action_template"], Value::Null);
    assert!(commit_json.get("next").is_none());
    assert!(commit_json.get("next_argv").is_none());
    assert!(commit_json.get("next_template").is_none());
    assert_eq!(commit_json["verification"]["verified"], true);
}

#[test]
fn native_commit_saves_heddle_state_without_impossible_checkpoint_recovery() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();

    let commit = json_value(
        temp.path(),
        &[
            "commit",
            "-m",
            "add readme",
            "--confidence",
            "0.82",
            "--output",
            "json",
        ],
    );
    assert_eq!(commit["status"], "committed");
    assert_eq!(commit["action"], "commit");
    assert!(
        commit["change_id"]
            .as_str()
            .is_some_and(|state| state.starts_with("hd-")),
        "native commit should save a Heddle state: {commit}"
    );
    assert_eq!(commit["git_commit"], Value::Null);
    assert_eq!(commit["verification"]["verified"], true);

    std::fs::write(temp.path().join("NEXT.md"), "next\n").unwrap();
    let text = heddle(
        &["commit", "-m", "next native state", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        text.contains("Committed Heddle state") && !text.contains("checkpoint"),
        "native commit text should not recommend an unavailable Git checkpoint: {text}"
    );

    std::fs::write(temp.path().join("MORE.md"), "more\n").unwrap();
    let status_text = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status_text.contains("heddle commit -m \"...\"") && !status_text.contains("heddle push"),
        "dirty native status without a remote should recommend a local save, not a publish follow-up: {status_text}"
    );
}

#[test]
fn core_mutations_emit_post_verification_in_json() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "seed\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    heddle(&["init"], Some(temp.path())).unwrap();
    let import = json_value(
        temp.path(),
        &[
            "bridge", "git", "import", "--ref", "main", "--output", "json",
        ],
    );
    assert!(
        import["already_in_sync"].as_bool().is_some(),
        "import should produce a normal JSON setup response: {import}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "captured\n").unwrap();
    let capture = json_value(
        temp.path(),
        &["capture", "-m", "captured", "--output", "json"],
    );
    assert_eq!(capture["status"], "captured");
    assert_eq!(capture["output_kind"], "capture");
    assert_eq!(capture["action"], "capture");
    assert_schema_declares_runtime_top_level(&["capture"], &capture);
    assert_eq!(
        capture["verification"]["status"], "needs_checkpoint",
        "capture should prove the post-capture Git-overlay state needs a checkpoint: {capture}"
    );
    assert_eq!(
        capture["verification"]["recommended_action"],
        "heddle checkpoint -m \"...\""
    );
    assert_eq!(
        capture["next_action"], capture["verification"]["recommended_action"],
        "capture should promote post-capture verify advice to the top-level next action: {capture}"
    );
    assert_eq!(
        capture["recommended_action"], capture["verification"]["recommended_action"],
        "capture should promote post-capture verify advice to the top-level recommendation: {capture}"
    );
    assert!(
        capture["verification"]["recommended_action_template"]["required_inputs"]
            .as_array()
            .is_some_and(|inputs| !inputs.is_empty()),
        "capture's post-verify checkpoint template must be display-only: {capture}"
    );
    assert_eq!(
        capture["recommended_action_template"]["argv_template"],
        capture["verification"]["recommended_action_template"]["argv_template"],
        "capture top-level argv should match the promoted verify action: {capture}"
    );
    assert_eq!(
        capture["recommended_action_template"],
        capture["verification"]["recommended_action_template"],
        "display-only capture recommendation should carry matching top-level template metadata: {capture}"
    );
    assert_eq!(
        capture["next_action_template"]["argv_template"],
        capture["recommended_action_template"]["argv_template"],
        "capture next_action should carry matching argv metadata: {capture}"
    );
    assert_eq!(
        capture["next_action_template"], capture["recommended_action_template"],
        "capture next_action should carry matching template metadata: {capture}"
    );
    let status_after_capture = json_value(temp.path(), &["status", "--output", "json"]);
    assert_eq!(
        status_after_capture["state"]["change_id"], capture["change_id"],
        "status should describe the captured state: {status_after_capture}"
    );
    assert_eq!(
        status_after_capture["state"]["content_hash"], capture["content_hash"],
        "content_hash should mean the same state hash in capture and status: {status_after_capture}"
    );
    let log_after_capture = json_value(temp.path(), &["log", "--output", "json"]);
    let captured_log_entry = log_after_capture["states"]
        .as_array()
        .unwrap_or_else(|| panic!("log states should be an array: {log_after_capture}"))
        .iter()
        .find(|entry| entry["change_id"] == capture["change_id"])
        .unwrap_or_else(|| panic!("log should include captured state: {log_after_capture}"));
    assert_eq!(
        captured_log_entry["content_hash"], capture["content_hash"],
        "content_hash should mean the same state hash in capture and log: {log_after_capture}"
    );

    let checkpoint = json_value(
        temp.path(),
        &["checkpoint", "-m", "checkpointed", "--output", "json"],
    );
    assert_eq!(checkpoint["status"], "checkpointed");
    assert_eq!(checkpoint["output_kind"], "checkpoint");
    assert_eq!(checkpoint["action"], "checkpoint");
    assert_schema_declares_runtime_top_level(&["checkpoint"], &checkpoint);
    assert_eq!(
        checkpoint["verification"]["verified"], true,
        "checkpoint should prove the repo is verified after writing Git: {checkpoint}"
    );

    std::fs::write(temp.path().join("tracked.txt"), "committed\n").unwrap();
    let commit = json_value(
        temp.path(),
        &["commit", "-m", "committed", "--output", "json"],
    );
    assert_eq!(commit["status"], "committed");
    assert_eq!(commit["output_kind"], "commit");
    assert_eq!(commit["action"], "commit");
    assert_schema_declares_runtime_top_level(&["commit"], &commit);
    assert_eq!(
        commit["verification"]["verified"], true,
        "commit should prove its composite capture+checkpoint post-state: {commit}"
    );
    let undo = json_value(temp.path(), &["undo", "--output", "json"]);
    assert_eq!(undo["output_kind"], "undo");
    assert_eq!(undo["status"], "completed");
    assert_schema_declares_runtime_top_level(&["undo"], &undo);
    assert!(undo.get("next_action").is_some());
    assert!(undo.get("next_action_template").is_some());
    assert!(undo.get("recommended_action").is_some());
    assert!(undo.get("recommended_action_template").is_some());
    assert_eq!(
        undo["verification"]["verified"], true,
        "undo should prove the repository after restoring state: {undo}"
    );
}

#[test]
fn plain_git_core_save_refusals_do_not_initialize_heddle() {
    for (verb, args) in [
        ("capture", vec!["capture", "-m", "should not init"]),
        ("commit", vec!["commit", "-m", "should not init"]),
        ("checkpoint", vec!["checkpoint", "-m", "should not init"]),
    ] {
        let temp = TempDir::new().unwrap();
        init_git_repo_for_json_contract(temp.path(), "main");
        std::fs::write(temp.path().join("tracked.txt"), "seed\n").unwrap();
        git_commit_all_for_json_contract(temp.path(), "seed");
        std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
        let before_status = git_status_short_for_json_contract(temp.path());

        let mut command = vec!["--output", "json"];
        command.extend(args);
        let output = heddle_output(&command, Some(temp.path()))
            .unwrap_or_else(|err| panic!("{verb} should execute and refuse cleanly: {err}"));

        assert!(
            !output.status.success(),
            "{verb} must refuse before explicit adoption"
        );
        assert!(
            output.stdout.is_empty(),
            "{verb} refusal should keep JSON errors on stderr only: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !temp.path().join(".heddle").exists(),
            "{verb} refusal must not create .heddle in a plain Git repo"
        );
        assert_eq!(
            git_status_short_for_json_contract(temp.path()),
            before_status,
            "{verb} refusal must not change the Git worktree or index"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value = serde_json::from_str(stderr)
            .unwrap_or_else(|err| panic!("{verb} stderr should be JSON: {err}: {stderr}"));
        assert_eq!(envelope["kind"], "git_repo_needs_adoption");
        assert_eq!(envelope["primary_command"], "heddle adopt --ref main");
        assert_eq!(
            envelope["primary_command_template"]["argv_template"],
            heddle_argv_json(["adopt", "--ref", "main"])
        );
        assert!(
            envelope["preserved"]
                .as_str()
                .is_some_and(|value| value.contains("Heddle metadata")),
            "{verb} refusal should say metadata was preserved: {envelope}"
        );
    }
}

#[test]
fn dirty_git_repo_after_init_requires_import_before_commit() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "seed\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();

    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "needs_import");
    assert_eq!(verify["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        verify["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert_eq!(
        verify["recovery_action_templates"][0]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert!(
        verify["checks"].as_array().unwrap().iter().any(|check| {
            check["name"] == "Mapping"
                && check["status"] == "needs_import"
                && check["recommended_action"] == "heddle adopt --ref main"
                && check["recommended_action_template"]["argv_template"]
                    == heddle_argv_json(["adopt", "--ref", "main"])
        }),
        "dirty first-run verify should block on import before worktree advice: {verify}"
    );

    let status = json_value(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "needs_import");
    assert_eq!(status["recommended_action"], "heddle adopt --ref main");
    assert_eq!(
        status["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert_eq!(
        status["recovery_action_templates"][0]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );

    let capture = heddle_output(
        &["--output", "json", "capture", "-m", "should fail closed"],
        Some(temp.path()),
    )
    .expect("capture should run");
    assert!(!capture.status.success(), "capture must fail before import");
    assert!(
        capture.stdout.is_empty(),
        "failed JSON command should not write stdout: {}",
        String::from_utf8_lossy(&capture.stdout)
    );
    let stderr = std::str::from_utf8(&capture.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "git_history_needs_import");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle adopt --ref main")),
        "capture refusal should name the import recovery: {envelope}"
    );

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();
    let after_import = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(
        after_import["status"], "dirty_worktree",
        "after import, dirty advice should become the primary blocker: {after_import}"
    );
    assert_eq!(
        after_import["recommended_action"],
        "heddle commit -m \"...\""
    );
    assert!(
        after_import["recommended_action_template"]["required_inputs"]
            .as_array()
            .is_some_and(|inputs| !inputs.is_empty()),
        "templated commit advice must stay display-only until a message is supplied: {after_import}"
    );
    assert_eq!(
        after_import["recommended_action_template"]["argv_template"],
        heddle_argv_json(["commit", "-m", "<message>"]),
        "templated commit advice should expose a structured machine plan: {after_import}"
    );
    assert_eq!(
        after_import["recommended_action_template"]["required_inputs"],
        serde_json::json!(["message"])
    );
    assert_eq!(
        after_import["recommended_action_template"]["agent_may_fill"],
        true
    );
}

#[test]
fn emitted_first_run_recommended_actions_parse_through_clap() {
    let catalog = parse_exactly_one_json_value(
        &heddle(&["commands", "--output", "json"], None).expect("commands JSON"),
    )
    .expect("commands should emit one JSON value");
    let placeholders = catalog["recommended_action_placeholders"]
        .as_array()
        .expect("catalog should expose placeholders")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();

    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    for args in [
        vec!["status", "--output", "json"],
        vec!["diagnose", "--output", "json"],
        vec!["verify", "--output", "json"],
        vec!["bridge", "git", "status", "--output", "json"],
        vec!["thread", "list", "--output", "json"],
        vec!["thread", "show", "main", "--output", "json"],
        vec!["workspace", "show", "--output", "json"],
    ] {
        let value = json_value(temp.path(), &args);
        assert_runtime_actions_parse(&value, &placeholders, &args);
    }

    heddle(&["init"], Some(temp.path())).unwrap();
    for args in [
        vec!["status", "--output", "json"],
        vec!["diagnose", "--output", "json"],
        vec!["verify", "--output", "json"],
        vec!["bridge", "git", "status", "--output", "json"],
        vec!["thread", "list", "--output", "json"],
        vec!["thread", "show", "main", "--output", "json"],
        vec!["workspace", "show", "--output", "json"],
    ] {
        let value = json_value(temp.path(), &args);
        assert_runtime_actions_parse(&value, &placeholders, &args);
    }
}

fn json_value(cwd: &std::path::Path, args: &[&str]) -> Value {
    let mut full_args: Vec<&str> = Vec::with_capacity(args.len() + 2);
    if !args.iter().any(|arg| *arg == "json" || *arg == "text") {
        full_args.push("--output");
        full_args.push("json");
    }
    full_args.extend_from_slice(args);
    let output = heddle_output(&full_args, Some(cwd))
        .unwrap_or_else(|err| panic!("heddle {full_args:?}: {err}"));
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    if output.status.success() || !stdout.trim().is_empty() {
        let parsed = parse_exactly_one_json_value(stdout).unwrap_or_else(|err| {
            panic!("heddle {args:?} should emit one JSON value: {err}: {stdout}")
        });
        return inject_post_verification_at(cwd, args, parsed);
    }
    if args.contains(&"verify") {
        let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
            panic!("heddle {args:?} should emit a verify error envelope: {err}: {stderr}")
        });
        if envelope["kind"] == "verify_failed" {
            let mut verification = envelope["verification"].clone();
            if let Some(object) = verification.as_object_mut() {
                object.insert(
                    "output_kind".to_string(),
                    Value::String("verify".to_string()),
                );
                object.insert("clean".to_string(), Value::Bool(false));
            }
            return verification;
        }
    }
    panic!(
        "heddle {:?} failed: code={:?}\nstdout: {}\nstderr: {}",
        args,
        output.status.code(),
        stdout,
        stderr
    );
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire). This helper grafts the proof back onto
/// the returned value for test ergonomics by invoking
/// `heddle verify --output json` after the original call. Real
/// consumers see the field omitted.
fn inject_post_verification_at(cwd: &std::path::Path, args: &[&str], mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
    if args.iter().any(|a| *a == "verify" || *a == "doctor") {
        return value;
    }
    let verify_out = match heddle_output(&["--output", "json", "verify"], Some(cwd)) {
        Ok(out) => out,
        Err(_) => return value,
    };
    let stream = if !verify_out.status.success() {
        verify_out.stderr
    } else {
        verify_out.stdout
    };
    let text = std::str::from_utf8(&stream).unwrap_or("");
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return value,
    };
    let verification = if parsed.get("kind") == Some(&Value::String("verify_failed".to_string())) {
        parsed.get("verification").cloned().unwrap_or(Value::Null)
    } else {
        let mut obj_map = parsed.as_object().cloned().unwrap_or_default();
        obj_map.remove("output_kind");
        obj_map.remove("repository_label");
        obj_map.remove("repository_context");
        obj_map.remove("clean");
        Value::Object(obj_map)
    };
    obj.insert("verification".to_string(), verification);
    value
}

fn assert_no_json_key_named(value: &Value, forbidden: &str, context: &str) {
    match value {
        Value::Object(map) => {
            assert!(
                !map.contains_key(forbidden),
                "{context} JSON must expose `verification`, not `{forbidden}`: {value}"
            );
            for child in map.values() {
                assert_no_json_key_named(child, forbidden, context);
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_no_json_key_named(child, forbidden, context);
            }
        }
        _ => {}
    }
}

fn heddle_output_without_principal_env(
    args: &[&str],
    cwd: &std::path::Path,
) -> Result<std::process::Output, String> {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));
    cmd.current_dir(cwd);
    cmd.env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"));
    cmd.env_remove("HEDDLE_PRINCIPAL_NAME");
    cmd.env_remove("HEDDLE_PRINCIPAL_EMAIL");
    cmd.output().map_err(|e| e.to_string())
}

fn sibling_checkout_path(repo: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    repo.with_file_name(format!("{repo_name}-{suffix}"))
}

fn assert_schema_declares_runtime_top_level(verb: &[&str], runtime: &Value) {
    let mut args = vec!["schemas"];
    args.extend(verb.iter().copied());
    let schema = heddle(&args, None).unwrap_or_else(|err| panic!("schema for {verb:?}: {err}"));
    let schema: Value = serde_json::from_str(&schema)
        .unwrap_or_else(|err| panic!("schema for {verb:?} should parse: {err}: {schema}"));
    let properties = schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema for {verb:?} should expose properties: {schema}"));
    let runtime = runtime
        .as_object()
        .unwrap_or_else(|| panic!("runtime output for {verb:?} should be an object: {runtime}"));
    let missing = runtime
        .keys()
        // `verification` is no longer part of mutation schemas — the
        // wire payload omits it. Test helpers (e.g.
        // `inject_post_verification_at`) splice the proof back in for
        // ergonomic assertions on `value["verification"][...]`, so
        // skip it here to keep schema/runtime parity meaningful.
        .filter(|key| key.as_str() != "verification" && !properties.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "schema for {verb:?} is missing runtime top-level field(s) {missing:?}; runtime={runtime:?}; schema={schema}"
    );
}

fn assert_runtime_actions_parse(
    value: &Value,
    placeholders: &std::collections::BTreeSet<String>,
    source_args: &[&str],
) {
    assert_action_sidecars_match(value, placeholders, source_args);
    let mut actions = Vec::new();
    collect_runtime_actions(value, &mut actions);
    assert!(
        !actions.is_empty(),
        "{source_args:?} should expose at least one machine action field: {value}"
    );
    for action in actions {
        let trimmed = action.trim();
        if trimmed.is_empty() || placeholders.contains(trimmed) {
            continue;
        }
        let argv = split_recommended_action_for_test(trimmed)
            .unwrap_or_else(|err| panic!("{source_args:?} action should split: {err}: {trimmed}"));
        assert_eq!(
            argv.first().map(String::as_str),
            Some("heddle"),
            "{source_args:?} action should use heddle or a registered placeholder: {trimmed}"
        );
        Cli::command()
            .try_get_matches_from(argv.clone())
            .unwrap_or_else(|err| {
                panic!(
                    "{source_args:?} action should parse through clap: {err}: {}",
                    argv.join(" ")
                )
            });
    }
}

fn assert_action_sidecars_match(
    value: &Value,
    placeholders: &std::collections::BTreeSet<String>,
    source_args: &[&str],
) {
    match value {
        Value::Object(map) => {
            for field in ["recommended_action", "next_action"] {
                let argv_key = format!("{field}_argv");
                let template_key = format!("{field}_template");
                let action = map.get(field);
                let argv = map.get(&argv_key);
                let template = map.get(&template_key);
                if action.is_none() && argv.is_none() && template.is_none() {
                    continue;
                }
                match action {
                    Some(Value::String(action)) if !action.trim().is_empty() => {
                        if let Some(argv) = argv {
                            assert_eq!(
                                argv.clone(),
                                expected_action_argv_json(action, placeholders, source_args),
                                "{source_args:?} {argv_key} should match {field}: {action}; object={value}"
                            );
                        }
                        if let Some(template) = template {
                            let display_only = is_display_only_action(action)
                                || placeholders.contains(action.trim());
                            if display_only {
                                assert!(
                                    !template.is_null(),
                                    "{source_args:?} {template_key} should describe display-only action `{action}`; object={value}"
                                );
                            }
                        }
                    }
                    Some(Value::Null) | None => {
                        if let Some(argv) = argv {
                            assert!(
                                argv.is_null(),
                                "{source_args:?} {argv_key} must be null when {field} is null/missing: object={value}"
                            );
                        }
                        if let Some(template) = template {
                            assert!(
                                template.is_null(),
                                "{source_args:?} {template_key} must be null when {field} is null/missing: object={value}"
                            );
                        }
                    }
                    Some(other) => panic!(
                        "{source_args:?} {field} should be string or null, got {other}: object={value}"
                    ),
                }
            }
            for child in map.values() {
                assert_action_sidecars_match(child, placeholders, source_args);
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_action_sidecars_match(value, placeholders, source_args);
            }
        }
        _ => {}
    }
}

fn expected_action_argv_json(
    action: &str,
    placeholders: &std::collections::BTreeSet<String>,
    source_args: &[&str],
) -> Value {
    let trimmed = action.trim();
    if trimmed.is_empty() || placeholders.contains(trimmed) || is_display_only_action(trimmed) {
        return Value::Null;
    }
    let argv = split_recommended_action_for_test(trimmed)
        .unwrap_or_else(|err| panic!("{source_args:?} action should split: {err}: {trimmed}"));
    assert_eq!(
        argv.first().map(String::as_str),
        Some("heddle"),
        "{source_args:?} action should use heddle or a registered placeholder: {trimmed}"
    );
    Cli::command()
        .try_get_matches_from(argv.clone())
        .unwrap_or_else(|err| {
            panic!(
                "{source_args:?} action should parse through clap: {err}: {}",
                argv.join(" ")
            )
        });
    serde_json::json!(
        std::iter::once(env!("CARGO_BIN_EXE_heddle").to_string())
            .chain(argv.into_iter().skip(1))
            .collect::<Vec<_>>()
    )
}

fn is_display_only_action(action: &str) -> bool {
    action.contains("...") || action.contains('…') || (action.contains('<') && action.contains('>'))
}

fn collect_runtime_actions(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("recommended_action" | "next_action", Value::String(action)) => {
                        out.push(action.clone());
                    }
                    ("recovery_commands", Value::Array(commands)) => {
                        out.extend(
                            commands
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string),
                        );
                    }
                    _ => collect_runtime_actions(value, out),
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_runtime_actions(value, out);
            }
        }
        _ => {}
    }
}

fn split_recommended_action_for_test(action: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = action.chars().peekable();
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => in_double_quote = !in_double_quote,
            '\\' if in_double_quote => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            ch if ch.is_whitespace() && !in_double_quote => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }

    if in_double_quote {
        return Err("unterminated double quote".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn init_git_repo_for_json_contract(path: &std::path::Path, branch: &str) {
    let status = std::process::Command::new("git")
        .args(["init", "--initial-branch", branch])
        .current_dir(path)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init should succeed");
    for (key, value) in [
        ("user.name", "Heddle Test"),
        ("user.email", "heddle@example.com"),
    ] {
        let status = std::process::Command::new("git")
            .args(["config", key, value])
            .current_dir(path)
            .status()
            .expect("git config should run");
        assert!(status.success(), "git config {key} should succeed");
    }
}

fn git_commit_all_for_json_contract(path: &std::path::Path, message: &str) {
    for args in [&["add", "."][..], &["commit", "-m", message][..]] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} should succeed");
    }
}

fn git_status_short_for_json_contract(path: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(path)
        .output()
        .expect("git status should run");
    assert!(output.status.success(), "git status should succeed");
    String::from_utf8(output.stdout).expect("git status should be UTF-8")
}

fn configure_repo_local_git_identity_for_json_contract(path: &std::path::Path) {
    let config = path.join(".git").join("config");
    let mut contents = std::fs::read_to_string(&config).unwrap_or_default();
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("[user]\n\tname = Heddle Test\n\temail = heddle@example.com\n");
    std::fs::write(config, contents).expect("write repo-local git identity");
}

fn parse_exactly_one_json_value(raw: &str) -> Result<Value, String> {
    let mut values = serde_json::Deserializer::from_str(raw).into_iter::<Value>();
    let value = values
        .next()
        .ok_or_else(|| "stdout was empty".to_string())?
        .map_err(|err| err.to_string())?;
    match values.next() {
        Some(Ok(extra)) => Err(format!("extra JSON value after first value: {extra}")),
        Some(Err(err)) => Err(err.to_string()),
        None => Ok(value),
    }
}

#[test]
fn git_compat_commit_branch_and_switch_shims_work() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git repo");
    configure_repo_local_git_identity_for_json_contract(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();

    let commit_json = heddle(
        &["--output", "json", "commit", "-m", "seed commit"],
        Some(temp.path()),
    )
    .unwrap();
    let commit: Value = serde_json::from_str(&commit_json).unwrap();
    assert_eq!(commit["action"], "commit");
    assert!(
        commit["change_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("hd-")
    );
    assert!(
        commit["git_commit"].as_str().unwrap_or("").len() >= 7,
        "commit shim should write a Git checkpoint: {commit}"
    );

    let branch = heddle(&["branch", "feature/git-shim"], Some(temp.path())).unwrap();
    assert!(
        branch.contains("feature/git-shim") || branch.contains("Created"),
        "branch shim should create a thread: {branch}"
    );

    let switched = heddle(&["switch", "feature/git-shim"], Some(temp.path())).unwrap();
    assert!(
        switched.contains("feature/git-shim") || switched.contains("Switched"),
        "switch shim should route to thread switch: {switched}"
    );
}

#[test]
fn thread_switch_refuses_dirty_worktree_without_force() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &["thread", "create", "feature/dirty-switch"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let output = heddle_output(
        &[
            "--output",
            "text",
            "thread",
            "switch",
            "feature/dirty-switch",
        ],
        Some(temp.path()),
    )
    .expect("invoke dirty switch");
    assert!(!output.status.success(), "dirty switch should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: Save or stash worktree changes before switch threads")
            && stderr.contains("Next: heddle commit -m \"...\"")
            && stderr.contains("Paths: unsaved worktree path(s): tracked.txt")
            && stderr.contains("Reason: switch threads would write another tree into the worktree")
            && stderr.contains("Kept: repository state and worktree files were left unchanged")
            && stderr.contains("Also: heddle capture -m \"...\", heddle stash push -m \"...\"")
            && !stderr.contains("Unsafe:")
            && !stderr.contains("Would change:")
            && !stderr.contains("Preserved:")
            && !stderr.contains("Other recovery:"),
        "dirty switch should give calm preservation guidance without verbose safety labels: {stderr}"
    );

    let verbose = heddle_output(
        &[
            "-v",
            "--output",
            "text",
            "thread",
            "switch",
            "feature/dirty-switch",
        ],
        Some(temp.path()),
    )
    .expect("invoke dirty switch verbose");
    assert!(
        !verbose.status.success(),
        "dirty switch verbose should fail"
    );
    let verbose_stderr = String::from_utf8_lossy(&verbose.stderr);
    assert!(
        verbose_stderr.contains("Error: Save or stash worktree changes before switch threads")
            && verbose_stderr.contains("Next: heddle commit -m \"...\"")
            && verbose_stderr.contains("Unsafe:")
            && verbose_stderr.contains("Would change:")
            && verbose_stderr.contains("Preserved:")
            && verbose_stderr
                .contains("Also: heddle capture -m \"...\", heddle stash push -m \"...\"")
            && verbose_stderr.contains("Hint:"),
        "dirty switch verbose should expose full preservation detail: {verbose_stderr}"
    );

    let forced = heddle(
        &["thread", "switch", "feature/dirty-switch", "--force"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("feature/dirty-switch"),
        "forced switch should still be explicit about target: {forced}"
    );
}

#[test]
fn remote_list_and_show_json_share_git_overlay_remote_view() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    gix::init_bare(&origin).expect("init bare origin");
    gix::init(temp.path()).expect("init git worktree");
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join(".git/config"))
        .unwrap()
        .write_all(
            format!(
                "\n[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                origin.display()
            )
            .as_bytes(),
        )
        .unwrap();

    let list_json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_json).unwrap();
    assert_eq!(list["output_kind"], "remote_list");
    let remotes = list["remotes"].as_array().unwrap();
    assert!(
        remotes.iter().any(|remote| remote["name"] == "origin"
            && remote["source"] == "git"
            && remote["is_default"] == true),
        "remote list should include plain Git origin without initializing Heddle: {list}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "plain Git remote list must be observe-only"
    );

    let show_json = heddle(
        &["--output", "json", "remote", "show", "origin"],
        Some(temp.path()),
    )
    .unwrap();
    let show: Value = serde_json::from_str(&show_json).unwrap();
    assert_eq!(show["output_kind"], "remote_show");
    assert_eq!(show["name"], "origin");
    assert_eq!(show["source"], "git");
    assert_eq!(show["is_default"], true);
    assert!(
        !temp.path().join(".heddle").exists(),
        "plain Git remote show must be observe-only"
    );
}

#[test]
fn remote_mutations_honor_json_contract_and_schema() {
    let temp = TempDir::new().unwrap();
    json_value(temp.path(), &["init", "--output", "json"]);

    let add = json_value(
        temp.path(),
        &[
            "remote",
            "add",
            "origin",
            "/tmp/heddle-schema-origin",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["remote", "add"], &add);
    assert_eq!(add["output_kind"], "remote_add");
    assert_eq!(add["status"], "completed");
    assert_eq!(add["action"], "remote_add");
    assert_eq!(add["name"], "origin");
    assert!(
        add.get("verification").is_some(),
        "remote add should prove post-mutation verify: {add}"
    );

    let set_default = json_value(
        temp.path(),
        &["remote", "set-default", "origin", "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["remote", "set-default"], &set_default);
    assert_eq!(set_default["output_kind"], "remote_set_default");
    assert_eq!(set_default["action"], "remote_set_default");
    assert_eq!(set_default["default"], "origin");

    let remove = json_value(
        temp.path(),
        &["remote", "remove", "origin", "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["remote", "remove"], &remove);
    assert_eq!(remove["output_kind"], "remote_remove");
    assert_eq!(remove["action"], "remote_remove");
    assert_eq!(remove["name"], "origin");
}

#[test]
fn branch_delete_current_refuses_with_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "branch", "-d", "main"],
        Some(temp.path()),
    )
    .expect("invoke current branch delete");
    assert!(
        !output.status.success(),
        "deleting the current branch should fail"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("current branch delete should emit JSON envelope");
    assert_eq!(envelope["kind"], "branch_delete_current");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Refusing to delete current thread")),
        "error should explain the unsafe branch delete: {envelope}"
    );
    // The recovery is to switch/create a sibling thread first, never the
    // circular `heddle thread list` that loops a junior (heddle#258).
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread create <other>")),
        "hint should name a non-circular recovery command: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| !hint.contains("heddle thread list")),
        "hint must not loop back to the circular `thread list`: {envelope}"
    );
    let templates = envelope["recovery_action_templates"]
        .as_array()
        .expect("recovery_action_templates should be an array");
    let create = templates
        .iter()
        .find(|template| {
            template["argv_template"] == heddle_argv_json(["thread", "create", "<other>"])
        })
        .unwrap_or_else(|| panic!("create recovery template should be present: {envelope}"));
    assert_eq!(create["agent_may_fill"], Value::Bool(true));
}

#[test]
fn empty_undo_redo_refuse_with_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for (args, kind, label) in [
        (
            ["--output", "json", "undo"],
            "nothing_to_undo",
            "Nothing to undo",
        ),
        (
            ["--output", "json", "redo"],
            "nothing_to_redo",
            "Nothing to redo",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke undo/redo");
        assert!(!output.status.success(), "{label} should fail");
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("empty undo/redo should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(label)),
            "error should name the empty history: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle undo --list")),
            "hint should name the inspection command: {envelope}"
        );
    }
}

#[test]
fn undo_list_preview_conflict_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "undo", "--list", "--preview"],
        Some(temp.path()),
    )
    .expect("invoke undo mode conflict");
    assert!(
        !output.status.success(),
        "undo --list --preview should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode undo mode refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("undo mode conflict should emit JSON envelope");
    assert_eq!(envelope["kind"], "undo_mode_conflict");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Use either --list or --preview")),
        "undo mode conflict should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle undo --list")
                && hint.contains("heddle undo --preview")),
        "undo mode conflict hint should name both valid commands: {stderr}"
    );
}

#[test]
fn empty_stash_refusals_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for (args, kind, label, recovery) in [
        (
            ["--output", "json", "stash", "push"],
            "no_changes_to_stash",
            "No changes to stash",
            "heddle status",
        ),
        (
            ["--output", "json", "stash", "drop"],
            "no_stash_available",
            "No stash to drop",
            "heddle stash list",
        ),
        (
            ["--output", "json", "stash", "apply"],
            "no_stash_available",
            "No stash found",
            "heddle stash list",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke stash refusal");
        assert!(!output.status.success(), "{label} should fail");
        assert!(
            output.stdout.is_empty(),
            "JSON failure must keep stdout quiet: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("stash refusal should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(label)),
            "error should keep the full typed advice: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains(recovery)),
            "hint should name the primary recovery command: {envelope}"
        );
    }
}

#[test]
fn undo_thread_create_with_live_worktree_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let worktree = sibling_checkout_path(temp.path(), "feature-wt");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &[
            "start",
            "feature",
            "--path",
            worktree.to_str().unwrap(),
            "--workspace",
            "solid",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output =
        heddle_output(&["--output", "json", "undo"], Some(temp.path())).expect("invoke undo");
    assert!(
        !output.status.success(),
        "undo of start --path should refuse while the worktree exists"
    );
    assert!(
        worktree.exists(),
        "typed refusal must not remove the materialized worktree"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("live worktree undo should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_worktree_undo_unsafe");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("orphaned by the inverse")),
        "error should explain the unsafe undo: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread drop feature --delete-thread")),
        "hint should name the exact teardown command: {envelope}"
    );
}

#[test]
fn rebase_continue_abort_without_operation_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        ["--output", "json", "rebase", "--continue"],
        ["--output", "json", "rebase", "--abort"],
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke rebase recovery");
        assert!(
            !output.status.success(),
            "rebase recovery without an operation should fail"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("rebase recovery should emit JSON envelope");
        assert_eq!(envelope["kind"], "no_rebase_in_progress");
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("No rebase in progress")),
            "error should name the missing operation: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle status")),
            "hint should name the operation inspection command: {envelope}"
        );
    }
}

#[test]
fn rebase_target_refusals_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    for (args, kind, expected) in [
        (
            vec!["--output", "json", "rebase"],
            "rebase_target_required",
            "target thread required",
        ),
        (
            vec!["--output", "json", "rebase", "missing-thread"],
            "rebase_target_not_found",
            "missing-thread",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke rebase target refusal");
        assert!(
            !output.status.success(),
            "rebase target refusal should fail"
        );
        assert!(
            output.stdout.is_empty(),
            "JSON-mode rebase target refusal must keep stdout quiet: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("rebase target refusal should emit JSON envelope");
        assert_eq!(envelope["kind"], kind);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(expected)),
            "rebase target refusal should include full typed advice: {stderr}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle thread list")),
            "rebase target hint should name thread inspection: {stderr}"
        );
    }
}

#[test]
fn cherry_pick_missing_commit_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "cherry-pick", "hd-deadbeef1234"],
        Some(temp.path()),
    )
    .expect("invoke cherry-pick target refusal");
    assert!(
        !output.status.success(),
        "missing cherry-pick commit should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode cherry-pick refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("cherry-pick refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "cherry_pick_commit_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("commit 'hd-deadbeef1234' not found")),
        "cherry-pick refusal should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle log")),
        "cherry-pick hint should name history inspection: {stderr}"
    );
}

#[test]
fn goto_missing_state_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "goto", "hd-deadbeef1234"],
        Some(temp.path()),
    )
    .expect("invoke goto target refusal");
    assert!(!output.status.success(), "missing goto target should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode goto refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("goto missing state should emit JSON envelope");
    assert_eq!(envelope["kind"], "state_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("State not found: hd-deadbeef1234")),
        "goto missing state should include full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle log")),
        "goto missing state hint should name history inspection: {stderr}"
    );
}

#[test]
fn bisect_good_bad_without_operation_use_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        ["--output", "json", "bisect", "good", "HEAD"],
        ["--output", "json", "bisect", "bad", "HEAD"],
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke bisect mark");
        assert!(
            !output.status.success(),
            "bisect mark without an operation should fail"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("bisect recovery should emit JSON envelope");
        assert_eq!(envelope["kind"], "no_bisect_in_progress");
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("No bisect in progress")),
            "error should name the missing operation: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle bisect start")),
            "hint should name the start command: {envelope}"
        );
    }
}

#[test]
fn thread_start_active_reservation_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let first = sibling_checkout_path(temp.path(), "first");
    let second = sibling_checkout_path(temp.path(), "second");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    heddle(
        &[
            "start",
            "feature/reserved-json",
            "--workspace",
            "solid",
            "--path",
            first.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "feature/reserved-json",
            "--workspace",
            "solid",
            "--path",
            second.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke second thread start");
    assert!(
        !output.status.success(),
        "second active writer should be rejected"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("reservation conflict should emit JSON envelope");
    assert_eq!(envelope["kind"], "active_thread_reservation");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("already has an active reservation")),
        "error should name the active reservation: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread show feature/reserved-json")),
        "hint should name the inspection command: {envelope}"
    );
}

#[test]
fn thread_start_anchor_mismatch_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let checkout = sibling_checkout_path(temp.path(), "feature-checkout");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature/anchored"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();
    let requested = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "feature/anchored",
            "--from",
            &requested,
            "--path",
            checkout.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke thread start with mismatched anchor");
    assert!(
        !output.status.success(),
        "thread start with mismatched --from should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode anchor mismatch must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("anchor mismatch should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_anchor_mismatch");
    assert!(
        envelope["error"].as_str().is_some_and(
            |error| error.contains("feature/anchored") && error.contains("--from resolved")
        ),
        "anchor mismatch should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread show feature/anchored")),
        "anchor mismatch should name the inspection command: {stderr}"
    );
}

#[test]
fn thread_switch_from_worktree_to_shared_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let alpha = sibling_checkout_path(temp.path(), "alpha-worktree");
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    heddle(
        &[
            "start",
            "alpha/worktree",
            "--workspace",
            "solid",
            "--path",
            alpha.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "create", "beta/shared"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "switch", "beta/shared"],
        Some(&alpha),
    )
    .expect("invoke thread switch from dedicated worktree");
    assert!(
        !output.status.success(),
        "switching to shared thread from dedicated worktree should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode switch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("switch refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_switch_would_overwrite_worktree");
    assert!(
        envelope["error"].as_str().is_some_and(
            |error| error.contains("beta/shared") && error.contains("no dedicated worktree")
        ),
        "switch refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle start --workspace materialized beta/shared")),
        "switch refusal should name the materialization command: {stderr}"
    );
}

#[test]
fn dirty_goto_start_path_and_drop_refuse_without_force() {
    let temp = TempDir::new().unwrap();
    let checkout = temp.path().join("worker");
    let checkout_arg = checkout.to_str().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    let base = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();
    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let goto = heddle_output(&["--output", "json", "goto", &base], Some(temp.path()))
        .expect("invoke goto");
    assert!(!goto.status.success(), "dirty goto should fail");
    let envelope: Value = serde_json::from_slice(&goto.stderr)
        .unwrap_or_else(|err| panic!("dirty goto should emit JSON advice: {err}; {goto:?}"));
    assert_eq!(envelope["kind"], "dirty_worktree");
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("tracked.txt")),
        "dirty goto should list dirty paths: {envelope}"
    );
    let dirty_recovery_commands = serde_json::json!([
        "heddle commit -m \"...\"",
        "heddle capture -m \"...\"",
        "heddle stash push -m \"...\""
    ]);
    assert_eq!(
        &envelope["recovery_commands"], &dirty_recovery_commands,
        "dirty goto should use the shared preservation commands: {envelope}"
    );

    let start = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "dirty-start",
            "--path",
            checkout_arg,
        ],
        Some(temp.path()),
    )
    .expect("invoke start");
    assert!(!start.status.success(), "dirty start --path should fail");
    let envelope: Value = serde_json::from_slice(&start.stderr).unwrap_or_else(|err| {
        panic!("dirty start --path should emit JSON advice: {err}; {start:?}")
    });
    assert_eq!(envelope["kind"], "dirty_worktree");
    assert_eq!(
        &envelope["recovery_commands"], &dirty_recovery_commands,
        "dirty start --path should use the shared preservation commands: {envelope}"
    );

    heddle(&["goto", &base, "--force"], Some(temp.path())).unwrap();
    let checkout = temp.path().with_file_name("worker-drop-target");
    let checkout_arg = checkout.to_str().unwrap();
    heddle(
        &["start", "drop-target", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(checkout.join("tracked.txt"), "dirty worker\n").unwrap();
    let drop = heddle_output(
        &["--output", "json", "thread", "drop", "drop-target"],
        Some(temp.path()),
    )
    .expect("invoke thread drop");
    assert!(!drop.status.success(), "dirty drop should fail");
    let envelope: Value = serde_json::from_slice(&drop.stderr)
        .unwrap_or_else(|err| panic!("dirty drop should emit JSON advice: {err}; {drop:?}"));
    assert_eq!(envelope["kind"], "dirty_worktree");
    assert_eq!(
        &envelope["recovery_commands"], &dirty_recovery_commands,
        "dirty drop should use the shared preservation commands: {envelope}"
    );
    let forced = heddle(
        &["thread", "drop", "drop-target", "--force"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("drop-target"),
        "forced drop should still name the target: {forced}"
    );
}

#[test]
fn start_path_inside_repo_refuses_before_creating_dirty_nested_checkout() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    git_commit_all_for_json_contract(temp.path(), "base");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    let nested = temp.path().join("nested-checkout");
    let nested_arg = nested.to_str().unwrap();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "nested-start",
            "--path",
            nested_arg,
        ],
        Some(temp.path()),
    )
    .expect("invoke nested start");
    assert!(!output.status.success(), "nested start should fail");
    assert!(
        !nested.exists(),
        "refusal should happen before creating a nested checkout"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("nested start should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_start_path_inside_repo");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| { error.contains("inside the current repository") }),
        "nested start should explain why it is unsafe: {stderr}"
    );
    assert!(
        envelope["would_change"]
            .as_str()
            .is_some_and(|would_change| !would_change.is_empty())
            && envelope["preserved"]
                .as_str()
                .is_some_and(|preserved| !preserved.is_empty()),
        "nested start should expose structured safety fields: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--path") && hint.contains("nested-start")),
        "nested start hint should suggest a sibling checkout path: {stderr}"
    );
}

#[test]
fn start_relative_sibling_path_outside_repo_is_accepted_after_normalization() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    git_commit_all_for_json_contract(temp.path(), "base");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    let sibling_name = format!(
        "{}-sibling-start",
        temp.path().file_name().unwrap().to_string_lossy()
    );
    let sibling = temp.path().parent().unwrap().join(&sibling_name);
    let sibling_arg = format!("../{sibling_name}");
    let output = heddle(
        &[
            "--output",
            "json",
            "start",
            "relative-sibling-start",
            "--path",
            &sibling_arg,
        ],
        Some(temp.path()),
    )
    .expect("relative sibling start should succeed");
    let started: Value =
        serde_json::from_str(&output).expect("relative sibling start should emit JSON");
    assert_eq!(started["thread"]["name"], "relative-sibling-start");
    assert!(
        !started["execution_path"].as_str().unwrap().contains("/../"),
        "start should report normalized execution paths: {started}"
    );
    assert_eq!(
        std::fs::canonicalize(started["execution_path"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&sibling).unwrap()
    );
    assert!(
        sibling.join(".heddle").exists(),
        "sibling checkout should be created outside the source repo"
    );
    std::fs::remove_dir_all(&sibling).unwrap();
}

#[test]
fn start_normalized_nested_path_inside_repo_is_refused() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    git_commit_all_for_json_contract(temp.path(), "base");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    let nested = temp.path().join("still-inside");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "normalized-nested-start",
            "--path",
            "nested/../still-inside",
        ],
        Some(temp.path()),
    )
    .expect("invoke normalized nested start");
    assert!(
        !output.status.success(),
        "normalized nested start should fail"
    );
    assert!(
        !nested.exists(),
        "refusal should happen before creating the normalized nested checkout"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("normalized nested start should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_start_path_inside_repo");
}

#[test]
fn revert_refuses_dirty_worktree_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();
    let target = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    let output = heddle_output(&["--output", "json", "revert", &target], Some(temp.path()))
        .expect("invoke revert");
    assert!(!output.status.success(), "dirty revert should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("dirty revert should emit JSON error envelope");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["kind"] == "dirty_worktree"
            && envelope["error"].as_str().is_some_and(|error| error
                .contains("Save or stash worktree changes before revert")
                && !error.contains("Unsafe:")
                && !error.contains("Preserved:"))
            && envelope["unsafe_condition"]
                .as_str()
                .is_some_and(|condition| condition.contains("tracked.txt"))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle capture -m \"...\"")
                    && hint.contains("heddle stash push -m \"...\"")),
        "dirty revert should use the shared typed preservation advice: {stderr}"
    );
}

#[test]
fn revert_empty_state_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "empty"], Some(temp.path())).unwrap();
    let target = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();

    let output = heddle_output(&["--output", "json", "revert", &target], Some(temp.path()))
        .expect("invoke empty revert");
    assert!(!output.status.success(), "empty revert should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("empty revert should emit JSON envelope");
    assert_eq!(envelope["kind"], "no_changes_to_revert");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("No changes to revert")),
        "error should name the empty diff: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle show")),
        "hint should name the inspection command: {envelope}"
    );
}

#[test]
fn checkpoint_refuses_uncaptured_worktree_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    gix::init(temp.path()).expect("init git repo");
    configure_repo_local_git_identity_for_json_contract(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "seed checkpoint"],
        Some(temp.path()),
    )
    .unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "new\n").unwrap();
    let output = heddle_output(
        &["--output", "json", "checkpoint", "-m", "blocked checkpoint"],
        Some(temp.path()),
    )
    .expect("invoke checkpoint");
    assert!(!output.status.success(), "dirty checkpoint should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode checkpoint refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("dirty checkpoint should emit JSON error envelope");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["kind"] == "dirty_worktree"
            && envelope["error"].as_str().is_some_and(|error| error
                .contains("Save or stash worktree changes before checkpoint")
                && !error.contains("Unsafe:")
                && !error.contains("Preserved:"))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle capture -m \"...\"")
                    && hint.contains("heddle stash push -m \"...\"")),
        "dirty checkpoint should use the shared typed preservation advice: {stderr}"
    );
    assert_eq!(envelope["primary_command"], "heddle commit -m \"...\"");
    assert_eq!(envelope["primary_command_argv"], Value::Null);
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!([
            "heddle commit -m \"...\"",
            "heddle capture -m \"...\"",
            "heddle stash push -m \"...\""
        ])
    );
    assert!(
        envelope["unsafe_condition"]
            .as_str()
            .is_some_and(
                |condition| condition.contains("tracked.txt") && condition.contains("scratch.txt")
            ),
        "dirty checkpoint should expose paths outside the prose error: {envelope}"
    );
    assert!(
        envelope["would_change"]
            .as_str()
            .is_some_and(|would_change| would_change.contains("checkpoint")),
        "dirty checkpoint should expose what would change: {envelope}"
    );
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("Heddle state was left unchanged")),
        "dirty checkpoint should expose what was preserved: {envelope}"
    );
}

#[test]
fn clean_refuses_without_force_with_shared_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("scratch.txt"), "new\n").unwrap();

    let output =
        heddle_output(&["--output", "json", "clean"], Some(temp.path())).expect("invoke clean");
    assert!(!output.status.success(), "clean without force should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clean refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("clean refusal should emit JSON error envelope");
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
    assert!(
        envelope["kind"] == "destructive_requires_force"
            && envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("Refusing to clean")
                    && error.contains("destructive action requires --force"))
            && envelope["unsafe_condition"]
                .as_str()
                .is_some_and(|condition| condition.contains("untracked paths"))
            && envelope["preserved"]
                .as_str()
                .is_some_and(|preserved| preserved.contains("nothing was removed"))
            && envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle clean --dry-run")
                    && hint.contains("heddle clean --force")),
        "clean refusal should use the shared typed force advice: {stderr}"
    );
}

#[test]
fn clone_existing_destination_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let existing = temp.path().join("existing");
    std::fs::create_dir(&existing).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            "not-a-real-remote",
            existing.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke clone refusal");
    assert!(
        !output.status.success(),
        "clone into existing destination should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("clone refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_destination_exists");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("already exists")),
        "clone destination refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle clone")),
        "clone destination refusal should name the recovery command: {stderr}"
    );
}

#[test]
fn clone_invalid_remote_url_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("target");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            "::not-a-valid-remote::",
            target.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke clone refusal");
    assert!(
        !output.status.success(),
        "clone with invalid remote should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode invalid clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !target.exists(),
        "invalid remote rejection must run before destination creation"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("invalid clone remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_invalid_remote_url");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Invalid remote URL")),
        "clone invalid remote refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"].as_str().is_some_and(
            |hint| hint.contains("file:///path/to/repo") && hint.contains("Git clone URL")
        ),
        "clone invalid remote hint should name valid remote shapes: {stderr}"
    );
}

#[test]
fn clone_missing_remote_thread_uses_typed_advice_without_destination_side_effects() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let target = temp.path().join("target");
    std::fs::create_dir(&remote).unwrap();
    heddle(&["init"], Some(&remote)).unwrap();
    std::fs::write(remote.join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(&remote)).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            remote.to_str().unwrap(),
            target.to_str().unwrap(),
            "--thread",
            "missing-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke clone missing thread refusal");
    assert!(
        !output.status.success(),
        "clone with missing remote thread should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode clone missing-thread refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !target.exists(),
        "missing thread refusal must run before destination initialization"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("clone missing thread should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_remote_thread_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Thread 'missing-thread' not found in remote")),
        "clone missing thread refusal should keep full typed advice: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "clone missing thread hint should name thread inspection: {stderr}"
    );
}

#[test]
fn thread_drop_missing_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "drop", "missing-thread"],
        Some(temp.path()),
    )
    .expect("invoke missing thread drop");
    assert!(!output.status.success(), "missing thread drop should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing thread drop refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Thread 'missing-thread' not found")),
        "missing thread drop should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "missing thread drop hint should name thread list: {stderr}"
    );
}

#[test]
fn thread_drop_current_checkout_refuses_instead_of_claiming_missing() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let listed = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        listed["threads"].as_array().is_some_and(|threads| {
            threads
                .iter()
                .any(|thread| thread["name"] == "main" && thread["is_current"] == true)
        }),
        "thread list should present the attached main thread: {listed}"
    );

    let output = heddle_output(
        &["--output", "json", "thread", "drop", "main"],
        Some(temp.path()),
    )
    .expect("invoke current thread drop");
    assert!(!output.status.success(), "current thread drop should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode current thread drop refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("current thread drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "current_thread_not_droppable");
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            error.contains("Thread 'main' is the current checkout thread")
                && !error.contains("not found")
        }),
        "current thread drop should refuse with the real reason: {stderr}"
    );
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());
}

#[test]
fn thread_drop_current_recovery_points_to_create_when_no_sibling() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "drop", "main"],
        Some(temp.path()),
    )
    .expect("invoke current thread drop");
    assert!(!output.status.success(), "current thread drop should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("current thread drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "current_thread_not_droppable");

    // The circular `heddle thread list` hint is gone: with no sibling
    // thread to switch to, the recovery is to create one, switch to it,
    // then retry the drop (heddle#258).
    let hint = envelope["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("heddle thread create <other>"),
        "hint should suggest creating a sibling thread: {stderr}"
    );
    assert!(
        !hint.contains("heddle thread list"),
        "hint must not loop back to the circular `thread list`: {stderr}"
    );
    assert_eq!(envelope["primary_command"], "heddle thread create <other>");

    // Plain `thread drop` keeps its plain retry — the destructive
    // `--delete-thread` flag must NOT leak into the non-destructive mode.
    assert!(
        hint.contains("heddle thread drop main") && !hint.contains("--delete-thread"),
        "plain drop recovery should suggest the plain retry, not the destructive form: {stderr}"
    );

    let templates = envelope["recovery_action_templates"]
        .as_array()
        .expect("recovery_action_templates should be an array");
    let create = templates
        .iter()
        .find(|template| {
            template["argv_template"] == heddle_argv_json(["thread", "create", "<other>"])
        })
        .unwrap_or_else(|| panic!("create recovery template should be present: {stderr}"));
    assert_eq!(create["agent_may_fill"], Value::Bool(true));
    assert!(
        create["argv_template"]
            .as_array()
            .is_some_and(|argv| argv.iter().any(|arg| arg == "<other>")),
        "create template should mark <other> as a fillable slot: {stderr}"
    );
}

/// heddle#258 r2 (cid 3327829289): when the user asks for the destructive
/// `thread drop --delete-thread` on the current (lightweight, no-record)
/// ref, the recovery retry hint must PRESERVE `--delete-thread`. The r1
/// hint hardcoded a bare `heddle thread drop {current}`, which on retry
/// re-enters with `manager.load == None && delete_thread == false` and
/// dead-ends at `thread_not_found` — the user's destructive intent is lost.
#[test]
fn thread_drop_delete_thread_current_recovery_preserves_delete_flag() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "thread",
            "drop",
            "main",
            "--delete-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke destructive current thread drop");
    assert!(
        !output.status.success(),
        "destructive current thread drop should still refuse"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("destructive drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "branch_delete_current");

    let hint = envelope["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("heddle thread drop main --delete-thread"),
        "destructive drop recovery must preserve --delete-thread so a lightweight ref is removed on retry: {stderr}"
    );
    assert!(
        !hint.contains("heddle thread list"),
        "hint must not loop back to the circular `thread list`: {stderr}"
    );
}

/// Same class via the `branch -d` entry point — it rewrites to a
/// `--delete-thread` drop, so its recovery hint must also carry the
/// destructive mode (close-the-class: both entries share one helper).
#[test]
fn branch_delete_current_recovery_preserves_delete_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "branch", "-d", "main"],
        Some(temp.path()),
    )
    .expect("invoke current branch delete");
    assert!(
        !output.status.success(),
        "current branch delete should refuse"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("branch delete should emit JSON envelope");
    assert_eq!(envelope["kind"], "branch_delete_current");

    let hint = envelope["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("--delete-thread"),
        "branch -d recovery must preserve the ref-deleting mode on retry: {stderr}"
    );
}

/// End-to-end: after following the create+switch advice, the SUGGESTED
/// destructive retry must actually remove the lightweight ref (not
/// dead-end at `thread_not_found`).
#[test]
fn thread_drop_delete_thread_recovery_retry_actually_deletes_ref() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Refused while `main` is the current checkout.
    let refused = heddle_output(
        &[
            "--output",
            "json",
            "thread",
            "drop",
            "main",
            "--delete-thread",
        ],
        Some(temp.path()),
    )
    .expect("invoke destructive current thread drop");
    assert!(!refused.status.success());

    // Follow the advice: create a sibling and switch to it.
    heddle(&["thread", "create", "feature"], Some(temp.path())).expect("create a sibling thread");
    heddle(&["thread", "switch", "feature"], Some(temp.path()))
        .expect("switch to the sibling thread");

    // The SUGGESTED retry (mode preserved) must now succeed.
    heddle(
        &["thread", "drop", "main", "--delete-thread"],
        Some(temp.path()),
    )
    .expect("suggested destructive retry should delete the lightweight ref");

    let listed = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    assert!(
        listed["threads"]
            .as_array()
            .is_some_and(|threads| threads.iter().all(|thread| thread["name"] != "main")),
        "the lightweight `main` ref should be gone after the suggested retry: {listed}"
    );
}

#[test]
fn thread_drop_current_recovery_points_to_switch_when_sibling_exists() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).expect("create a sibling thread");

    let output = heddle_output(
        &["--output", "json", "thread", "drop", "main"],
        Some(temp.path()),
    )
    .expect("invoke current thread drop");
    assert!(!output.status.success(), "current thread drop should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("current thread drop should emit JSON envelope");
    assert_eq!(envelope["kind"], "current_thread_not_droppable");

    // A sibling thread exists, so the recovery is to switch to it first
    // (creating a fresh one stays available as a secondary path).
    let hint = envelope["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("heddle thread switch <other>"),
        "hint should suggest switching to a sibling thread first: {stderr}"
    );
    assert!(
        !hint.contains("heddle thread list"),
        "hint must not loop back to the circular `thread list`: {stderr}"
    );
    assert_eq!(envelope["primary_command"], "heddle thread switch <other>");

    let templates = envelope["recovery_action_templates"]
        .as_array()
        .expect("recovery_action_templates should be an array");
    let switch = templates
        .iter()
        .find(|template| {
            template["argv_template"] == heddle_argv_json(["thread", "switch", "<other>"])
        })
        .unwrap_or_else(|| panic!("switch recovery template should be present: {stderr}"));
    assert_eq!(switch["agent_may_fill"], Value::Bool(true));
    // Both recovery paths are exposed so machine callers can choose.
    assert!(
        templates.iter().any(|template| {
            template["argv_template"] == heddle_argv_json(["thread", "create", "<other>"])
        }),
        "create recovery template should also be present: {stderr}"
    );
}

#[test]
fn thread_switch_missing_thread_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "switch", "missing-thread"],
        Some(temp.path()),
    )
    .expect("invoke missing thread switch");
    assert!(
        !output.status.success(),
        "missing thread switch should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing thread switch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread switch should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Thread 'missing-thread' not found")),
        "missing thread switch should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread list")),
        "missing thread switch hint should name thread list: {stderr}"
    );
}

#[test]
fn doctor_uses_recovery_language_without_breaking_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending").unwrap();

    let text = heddle(&["--output", "text", "doctor"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Doctor"),
        "doctor should render a human header: {text}"
    );
    assert!(
        text.contains("Health: uncaptured"),
        "doctor should label the freshly-initialized worktree as uncaptured: {text}"
    );
    assert!(
        text.contains("Next step: heddle commit -m \"...\""),
        "doctor should provide one primary recovery command: {text}"
    );
    assert!(
        text.contains("Verification: 1 Heddle worktree path(s) are not captured")
            && !text.contains("Git overlay health:"),
        "native dirty doctor should not use Git-overlay labels: {text}"
    );
    assert!(
        !text.contains("Next:"),
        "doctor should use the newer next-step label: {text}"
    );

    let json = heddle(&["doctor", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("doctor JSON should parse");
    assert_eq!(
        parsed["health"]["recommended_action"],
        "heddle commit -m \"...\""
    );
}

#[test]
fn profile_env_writes_timings_to_stderr_without_polluting_json_stdout() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_env(
        &["--output", "json", "status"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "1")],
    )
    .expect("status should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "profiled status should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("profiled JSON stdout should still parse");
    assert!(
        stderr.contains("heddle profile:"),
        "profile output should go to stderr: {stderr}"
    );
    assert!(
        stderr.contains("command: status phases"),
        "status should include command-specific phases: {stderr}"
    );
    assert!(
        stderr.contains("command: status worktree"),
        "status should include worktree-specific phases: {stderr}"
    );
    assert!(
        stderr.contains("worktree_status_ms:"),
        "status profile should show worktree scan cost: {stderr}"
    );
    assert!(
        stderr.contains("directories_scanned:"),
        "status profile should show worktree scan counters: {stderr}"
    );
    assert!(
        stderr.contains("command_body_ms:"),
        "top-level profile should show command body cost: {stderr}"
    );
}

#[test]
fn version_verbose_reports_bug_context() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(
        &["--output", "text", "version", "--verbose"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        text.contains("Heddle "),
        "version should identify Heddle: {text}"
    );
    assert!(
        text.contains("Build profile:"),
        "verbose version should show build profile: {text}"
    );
    assert!(
        text.contains("Git binary: not required"),
        "verbose version should show Git-binary independence: {text}"
    );
    assert!(
        text.contains("Repository:"),
        "verbose version should show repository capability: {text}"
    );

    let json = heddle(
        &["version", "--verbose", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("version JSON should parse");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert!(parsed["features"].as_array().is_some());

    let terse_json = heddle(&["version", "--output", "json"], Some(temp.path())).unwrap();
    let terse: Value = serde_json::from_str(&terse_json)
        .expect("version --output json should parse without --verbose");
    assert_eq!(terse["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn start_merge_undo_json_workflow_keeps_machine_streams_clean() {
    fn json_success(args: &[&str], cwd: &std::path::Path) -> Value {
        let output = heddle_output(args, Some(cwd)).expect("invoke heddle");
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(
            output.status.success(),
            "{args:?} should succeed; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stderr.is_empty(),
            "{args:?} JSON success must keep stderr quiet: {stderr}"
        );
        let parsed: Value = serde_json::from_str(stdout)
            .unwrap_or_else(|_| panic!("{args:?} should emit parseable JSON: {stdout}"));
        inject_post_verification_at(cwd, args, parsed)
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    let feature = temp.path().join("feature checkout");
    std::fs::create_dir_all(&repo).unwrap();

    json_success(&["--output", "json", "init"], &repo);
    std::fs::write(
        repo.join("app.txt"),
        "base
",
    )
    .unwrap();
    json_success(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "base",
            "--confidence",
            "0.9",
        ],
        &repo,
    );

    let started = json_success(
        &[
            "--output",
            "json",
            "start",
            "feature/a",
            "--path",
            feature.to_str().expect("utf8 path"),
            "--workspace",
            "solid",
        ],
        &repo,
    );
    assert_eq!(started["name"], "feature/a");
    assert_eq!(
        started["execution_path"].as_str(),
        Some(feature.to_str().expect("utf8 path"))
    );

    std::fs::write(
        feature.join("app.txt"),
        "base
feature
",
    )
    .unwrap();
    json_success(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "feature",
            "--confidence",
            "0.9",
        ],
        &feature,
    );

    let before_merge_preview = json_success(&["--output", "json", "status"], &repo);
    let preview = json_success(
        &["--output", "json", "merge", "feature/a", "--preview"],
        &repo,
    );
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["output_kind"], "merge");
    assert_eq!(preview["preview_only"], true);
    assert_eq!(preview["would_merge"], true);
    assert_eq!(
        preview["recommended_action_template"]["argv_template"],
        heddle_argv_json(["ship", "--thread", "feature/a", "--no-push"])
    );
    assert_schema_declares_runtime_top_level(&["merge", "--preview"], &preview);
    assert_eq!(
        preview["verification"]["verified"], true,
        "merge preview should prove repository verify when no ready-thread workflow gate is present: {preview}"
    );
    let after_merge_preview = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_merge_preview["current_state"], before_merge_preview["current_state"],
        "merge --preview must not advance the current thread: before={before_merge_preview} after={after_merge_preview}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\n",
        "merge --preview must not modify the worktree"
    );

    let merged = json_success(&["--output", "json", "merge", "feature/a"], &repo);
    assert_eq!(merged["status"], "completed");
    assert_eq!(merged["output_kind"], "merge");
    assert_eq!(merged["fast_forward"], true);
    assert_eq!(merged["would_merge"], false);
    assert_eq!(merged["recommended_action"], Value::Null);
    assert_eq!(merged["recommended_action_argv"], Value::Null);
    assert_eq!(
        merged["verification"]["verified"], true,
        "merge apply should prove post-merge repository verify: {merged}"
    );

    let before_repeat_merge = json_success(&["--output", "json", "status"], &repo);
    let repeat_merge = heddle_output(&["--output", "text", "merge", "feature/a"], Some(&repo))
        .expect("invoke repeat merge");
    assert!(
        repeat_merge.status.success(),
        "already-applied merge should be a successful no-op"
    );
    assert!(
        repeat_merge.stderr.is_empty(),
        "already-applied merge should keep stderr quiet: {}",
        String::from_utf8_lossy(&repeat_merge.stderr)
    );
    let repeat_stdout = String::from_utf8_lossy(&repeat_merge.stdout);
    assert!(
        repeat_stdout.contains("Already up to date"),
        "already-applied merge should name the no-op state: {repeat_stdout}"
    );
    let noop_preview = heddle_output(
        &["--output", "text", "merge", "main", "--preview"],
        Some(&repo),
    )
    .expect("invoke no-op self preview");
    assert!(
        noop_preview.status.success(),
        "self merge preview should be a successful no-op"
    );
    let noop_preview_stdout = String::from_utf8_lossy(&noop_preview.stdout);
    assert!(
        noop_preview_stdout.contains("Already up to date"),
        "self merge preview should name the no-op state: {noop_preview_stdout}"
    );
    assert!(
        !noop_preview_stdout.contains("Next:"),
        "self merge preview must not recommend itself: {noop_preview_stdout}"
    );
    let noop_preview_json =
        json_success(&["--output", "json", "merge", "main", "--preview"], &repo);
    assert_eq!(noop_preview_json["output_kind"], "merge");
    assert_eq!(noop_preview_json["recommended_action"], Value::Null);
    assert_eq!(noop_preview_json["next_action"], Value::Null);
    assert_eq!(noop_preview_json["recommended_action_argv"], Value::Null);
    assert_eq!(noop_preview_json["next_action_argv"], Value::Null);
    let after_repeat_merge = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_repeat_merge["current_state"], before_repeat_merge["current_state"],
        "already-applied merge must not advance state: before={before_repeat_merge} after={after_repeat_merge}"
    );

    let listed = json_success(&["--output", "json", "undo", "--list"], &repo);
    assert_eq!(listed["output_kind"], "undo_list");
    assert!(
        listed["batches"]
            .as_array()
            .is_some_and(|batches| !batches.is_empty()),
        "undo --list should expose recent operation batches: {listed}"
    );

    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\nfeature\n",
        "real merge should update the worktree before undo preview"
    );
    let before_undo_preview = json_success(&["--output", "json", "status"], &repo);
    let preview_undo = json_success(&["--output", "json", "undo", "--preview"], &repo);
    assert_eq!(preview_undo["output_kind"], "undo");
    assert_eq!(preview_undo["status"], "preview");
    assert_eq!(preview_undo["action"], "undo");
    assert_eq!(preview_undo["next_action"], Value::Null);
    assert_eq!(preview_undo["recommended_action"], Value::Null);
    assert!(
        preview_undo["message"]
            .as_str()
            .unwrap_or("")
            .contains("Would undo"),
        "undo preview should clearly name the dry run: {preview_undo}"
    );
    let after_undo_preview = json_success(&["--output", "json", "status"], &repo);
    assert_eq!(
        after_undo_preview["current_state"], before_undo_preview["current_state"],
        "undo --preview must not advance or rewind the current thread: before={before_undo_preview} after={after_undo_preview}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "base\nfeature\n",
        "undo --preview must not modify the worktree"
    );
}

#[test]
fn version_verbose_honors_explicit_repo_path() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("explicit repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["--repo", repo.to_str().expect("utf8 path"), "init"], None).unwrap();

    let json = heddle(
        &[
            "--repo",
            repo.to_str().expect("utf8 path"),
            "version",
            "--verbose",
            "--output",
            "json",
        ],
        None,
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("version JSON should parse");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        parsed["repository_root"].as_str(),
        Some(repo.to_str().expect("utf8 path")),
        "version --repo should report the explicitly requested repository: {json}"
    );
}

#[test]
fn ready_text_names_ready_and_already_ready_noop_states() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let first = heddle_output(&["--output", "text", "ready"], Some(temp.path()))
        .expect("invoke ready text");
    assert!(first.status.success(), "ready text should succeed");
    assert!(
        first.stderr.is_empty(),
        "ready text success should keep stderr quiet: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(
        first_stdout.contains("no integration target") && first_stdout.contains("Readiness"),
        "ready text should name the clean no-target state: {first_stdout}"
    );
    assert!(
        first_stdout.contains("integration: none configured")
            && !first_stdout.contains("semantic: no_target")
            && !first_stdout.contains("state: ready"),
        "ready text should translate no-target internals into human workflow language: {first_stdout}"
    );
    assert!(
        !first_stdout.contains("heddle merge main"),
        "ready text must not recommend merging the current thread into itself: {first_stdout}"
    );

    let second = heddle_output(&["--output", "text", "ready"], Some(temp.path()))
        .expect("invoke ready text no-op");
    assert!(second.status.success(), "ready no-op should succeed");
    assert!(
        second.stderr.is_empty(),
        "ready no-op success should keep stderr quiet: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("no integration target") && second_stdout.contains("Readiness"),
        "ready no-op text should explicitly name the clean no-target state: {second_stdout}"
    );
    assert!(
        second_stdout.contains("integration: none configured")
            && !second_stdout.contains("semantic: no_target")
            && !second_stdout.contains("state: ready"),
        "ready no-op text should keep no-target internals out of the human surface: {second_stdout}"
    );
    assert!(
        !second_stdout.contains("heddle merge main"),
        "ready no-op must not recommend merging the current thread into itself: {second_stdout}"
    );
}

#[test]
fn ready_capture_is_visible_and_carries_confidence() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();

    let text = heddle_output(
        &[
            "--output",
            "text",
            "ready",
            "-m",
            "feature work",
            "--confidence",
            "0.77",
        ],
        Some(temp.path()),
    )
    .expect("invoke ready text");
    assert!(text.status.success(), "ready should succeed");
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains("captured: state hd-"),
        "ready text should explicitly name the captured state when it saves work: {stdout}"
    );

    std::fs::write(temp.path().join("followup.txt"), "followup\n").unwrap();
    let json = json_value(
        temp.path(),
        &[
            "ready",
            "-m",
            "followup work",
            "--confidence",
            "0.64",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["ready"], &json);
    assert_eq!(json["output_kind"], "ready");
    assert_eq!(json["captured"], true);
    assert!(
        json["captured_state"]
            .as_str()
            .is_some_and(|state| state.starts_with("hd-")),
        "ready JSON should carry the captured state id: {json}"
    );
}

#[test]
fn ready_refuses_dirty_capture_without_intent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();

    let ready_output = heddle_output(&["ready", "--output", "json"], Some(temp.path()))
        .expect("invoke blocked ready");
    assert!(
        !ready_output.status.success(),
        "blocked ready should exit nonzero"
    );
    let ready = json_value(temp.path(), &["ready", "--output", "json"]);
    assert_eq!(ready["output_kind"], "ready");
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["captured"], false);
    assert_eq!(ready["recommended_action"], "heddle ready -m \"...\"");
    assert_eq!(
        ready["recommended_action_template"]["argv_template"],
        heddle_argv_json(["ready", "-m", "<message>"])
    );
    assert_eq!(
        ready["verification"]["status"], "uncaptured",
        "missing ready intent should keep repository verification honest while overriding the next action: {ready}"
    );
    assert!(
        ready["blockers"]
            .as_array()
            .is_some_and(
                |blockers| blockers.iter().any(|blocker| blocker
                    .as_str()
                    .is_some_and(|text| text.contains("feature.txt")
                        && text.contains("-m/--message/--intent")))
            ),
        "ready should name the dirty path and intent requirement: {ready}"
    );
}

#[test]
fn ready_plain_git_refuses_before_initializing_heddle() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    let ready_output = heddle_output(&["ready", "--output", "json"], Some(temp.path()))
        .expect("invoke blocked plain-Git ready");
    assert!(
        !ready_output.status.success(),
        "plain Git ready should exit nonzero before Heddle adoption"
    );
    let ready = json_value(temp.path(), &["ready", "--output", "json"]);
    assert_eq!(ready["status"], "blocked");
    assert_eq!(ready["output_kind"], "ready");
    assert_eq!(ready["verification"]["status"], "needs_init");
    assert_eq!(ready["captured"], false);
    assert!(
        ready["recommended_action"]
            .as_str()
            .is_some_and(|action| action == "heddle adopt --ref main"),
        "plain Git ready should point at explicit adoption/initialization: {ready}"
    );
    assert_eq!(
        ready["recommended_action_template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "main"])
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "ready must not create .heddle in a plain Git repo"
    );
}

#[test]
fn verify_plain_git_blocker_text_is_not_redundant() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    let output = heddle_output(&["verify", "--output", "text"], Some(temp.path()))
        .expect("invoke strict verify text");
    assert!(
        !output.status.success(),
        "blocked plain Git verify should exit nonzero"
    );
    let verify = String::from_utf8_lossy(&output.stdout);
    assert!(
        verify.contains("Git repo detected")
            && verify.contains("connect this branch with heddle adopt --ref main")
            && verify.contains(".heddle metadata")
            && verify.contains("Git history imported")
            && verify.contains("Git worktree stays clean")
            && verify.contains("Next: heddle adopt --ref main"),
        "plain Git verify should explain first-run adoption in human terms: {verify}"
    );
    assert!(
        !verify.contains("Heddle Heddle")
            && !verify.contains("sidecar")
            && !verify.contains("Mapping:"),
        "plain Git compact verify should not leak internal setup wording: {verify}"
    );

    let status = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        status.contains("Git repo detected")
            && status.contains("connect this branch with heddle adopt --ref main")
            && status.contains(".heddle metadata")
            && status.contains("Git history imported")
            && status.contains("Git worktree stays clean"),
        "plain Git status should make first-run adoption and clean Git status explicit: {status}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "plain Git status/verify must remain observe-only"
    );

    let adopt = heddle(
        &["adopt", "--ref", "main", "--output", "text"],
        Some(temp.path()),
    )
    .expect("adopt should render text");
    assert!(
        adopt.contains("Git worktree: stays clean")
            && adopt.contains(".heddle metadata")
            && adopt.contains("imported Git history")
            && adopt.contains("Git commits inspected")
            && adopt.contains("New Heddle states")
            && !adopt.contains("Heddle changes saved"),
        "adopt text should say first-run metadata/import work leaves Git clean: {adopt}"
    );
}

#[test]
fn verify_prioritizes_dirty_worktree_over_optional_git_only_refs() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "main\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "main seed");
    let status = std::process::Command::new("git")
        .args(["checkout", "-b", "side"])
        .current_dir(temp.path())
        .status()
        .expect("git checkout side should run");
    assert!(status.success(), "git checkout side should succeed");
    std::fs::write(temp.path().join("side.txt"), "side\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "side seed");
    let status = std::process::Command::new("git")
        .args(["checkout", "main"])
        .current_dir(temp.path())
        .status()
        .expect("git checkout main should run");
    assert!(status.success(), "git checkout main should succeed");

    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "dirty\n").unwrap();

    let verify = json_value(temp.path(), &["verify", "--output", "json"]);
    assert_eq!(verify["status"], "dirty_worktree");
    assert_eq!(verify["recommended_action"], "heddle commit -m \"...\"");
    let mapping = verify["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Mapping")
        .unwrap_or_else(|| panic!("verify checks should include Mapping: {verify}"));
    assert_eq!(mapping["status"], "available");
    assert_eq!(mapping["clean"], true);
    let worktree = verify["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Worktree")
        .unwrap_or_else(|| panic!("verify checks should include Worktree: {verify}"));
    assert_eq!(worktree["status"], "dirty_worktree");
    assert_eq!(worktree["clean"], false);
    let clone = verify["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Clone")
        .unwrap_or_else(|| panic!("verify checks should include Clone: {verify}"));
    assert_eq!(
        clone["status"], "not_checked",
        "ordinary dirty work should not read as clone-integrity failure: {verify}"
    );
    assert_eq!(clone["clean"], true);

    let text_output = heddle_output(&["verify", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !text_output.status.success(),
        "dirty verify should exit nonzero"
    );
    let text = String::from_utf8_lossy(&text_output.stdout);
    assert!(
        text.contains("Workspace: changes to save")
            && text.contains("Changes to save: 1 path has unsaved changes"),
        "compact verify should calmly prioritize the actionable dirty worktree, not optional refs: {text}"
    );
    assert!(
        !text.contains("clone verification is blocked"),
        "dirty worktree verify should not imply clone integrity is broken: {text}"
    );
    assert!(
        !text.contains("Blocked: 1 other Git branch tip(s)"),
        "optional Git-only refs should not be the primary blocker: {text}"
    );
}

#[test]
fn verification_blocked_status_and_ready_do_not_claim_actionable_readiness() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();

    let status = json_value(temp.path(), &["status", "--output", "json"]);
    assert_eq!(status["verification"]["status"], "needs_import");
    assert_eq!(status["coordination_status"], "blocked");
    assert_ne!(
        status["thread_state"], "blocked",
        "verification blocker is a health signal carried by coordination_status, not a lifecycle state: {status}"
    );
    assert!(
        status["blockers"]
            .as_array()
            .is_some_and(|blockers| blockers.iter().any(|blocker| blocker
                .as_str()
                .is_some_and(|text| text.contains("Mapping:")))),
        "verify-blocked status should surface verify blockers at the top level: {status}"
    );

    let status_text = heddle(&["status", "--output", "text"], Some(temp.path()))
        .expect("status should render blocked verify text");
    assert!(
        status_text.contains("Git repo detected")
            && status_text.contains("connect this branch with heddle adopt --ref main")
            && status_text.contains(".heddle metadata")
            && status_text.contains("adoption imports Git history")
            && status_text.contains("Git worktree stays clean"),
        "initialized Git-overlay status should explain adoption without internal wording: {status_text}"
    );

    let ready = heddle_output(&["ready", "--output", "text"], Some(temp.path()))
        .expect("ready should render blocked verify output");
    assert!(!ready.status.success(), "blocked ready should exit nonzero");
    let ready_stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(
        ready_stdout.contains("Setup needed")
            && ready_stdout.contains("status: blocked")
            && ready_stdout.contains("checks: not run"),
        "ready should present blocked verify as setup state, not a merge verdict: {ready_stdout}"
    );
    assert!(
        !ready_stdout.contains("merge type: blocked")
            && !ready_stdout.contains("freshness: not checked"),
        "ready should not show fake readiness details while verify is blocked: {ready_stdout}"
    );
    assert!(
        !ready_stdout
            .lines()
            .any(|line| line.trim() == "state: ready"),
        "ready should hide prior ready lifecycle while verify is blocked: {ready_stdout}"
    );

    let threads = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(threads["verification"]["status"], "needs_import");
    assert_eq!(threads["recommended_action"], "heddle adopt --ref main");
    let thread = threads["threads"]
        .as_array()
        .and_then(|threads| threads.iter().find(|thread| thread["name"] == "main"))
        .expect("main thread should be listed");
    assert_eq!(
        thread["thread_health"], "needs_import",
        "thread list should not report a clean/ready thread while repository verification is blocked: {threads}"
    );
    assert_eq!(
        thread["coordination_status"], "blocked",
        "thread list should not advertise merge-ready coordination while repository verification is blocked: {threads}"
    );
    assert!(
        thread["blockers"].as_array().is_some_and(|blockers| {
            blockers.iter().any(|blocker| {
                blocker
                    .as_str()
                    .is_some_and(|text| text.contains("Git branch"))
            })
        }),
        "thread list should carry the verification blocker onto per-thread summaries: {threads}"
    );

    let merge_preview = heddle_output(
        &["merge", "main", "--preview", "--output", "text"],
        Some(temp.path()),
    )
    .expect("merge preview should render blocked verify output");
    assert!(
        !merge_preview.status.success(),
        "merge preview should strictly fail when verification prevents the preview from running"
    );
    assert!(
        merge_preview.stdout.is_empty(),
        "strict blocked preview should not emit a success payload: {}",
        String::from_utf8_lossy(&merge_preview.stdout)
    );
    let merge_stderr = String::from_utf8_lossy(&merge_preview.stderr);
    assert!(
        merge_stderr.contains("Repository verification is blocked; merge preview did not run"),
        "merge preview should name the setup/verify blocker: {merge_stderr}"
    );
    assert!(
        !merge_stderr.contains("Merge is up to date, but repository verify is blocked")
            && !merge_stderr.contains("Already up to date"),
        "blocked merge preview should not claim a merge verdict: {merge_stderr}"
    );
}

// Regression: heddle#306. `status` synthesized `thread_state: "blocked"` from a
// repository health/verification signal while `thread list` reported the
// thread record's lifecycle state, so the two verbs disagreed for the same
// thread at the same instant and `status` emitted a value outside the
// documented lifecycle enum. `thread_state` is lifecycle-only; the blocker
// signal belongs to the documented `coordination_status` health field.
#[test]
fn thread_state_agrees_across_status_and_thread_list_for_blocked_verification() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    // init without adopt → repository verification is blocked (needs_import),
    // a health signal that must not rewrite the thread's lifecycle state.
    heddle(&["init"], Some(temp.path())).unwrap();

    let status = json_value(temp.path(), &["status", "--output", "json"]);
    let threads = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    let thread = threads["threads"]
        .as_array()
        .and_then(|threads| threads.iter().find(|thread| thread["name"] == "main"))
        .expect("main thread should be listed");

    // Same thread, same instant: thread_state must agree across the two verbs.
    assert_eq!(
        status["thread_state"], thread["thread_state"],
        "thread_state must agree across status and thread list:\nstatus={status:#}\nlist={thread:#}"
    );
    // ...and stay a lifecycle value, not the health-derived "blocked".
    assert_ne!(
        status["thread_state"], "blocked",
        "verification/dirty-worktree health is not a lifecycle state: {status:#}"
    );
    // The blocker still surfaces through the documented health field, on both verbs.
    assert_eq!(
        status["coordination_status"], "blocked",
        "verification blocker should surface via status coordination_status: {status:#}"
    );
    assert_eq!(
        thread["coordination_status"], "blocked",
        "verification blocker should surface via thread list coordination_status: {threads:#}"
    );
}

#[test]
fn resolve_without_merge_emits_actionable_json_error() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke heddle resolve");
    assert!(
        !output.status.success(),
        "resolve with no merge should exit non-zero"
    );
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "JSON failure must not pollute stdout: {stdout}"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "no_merge_in_progress");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("No merge in progress")),
        "error should name the missing merge operation: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle status"),
        "resolve no-op should point at operation recovery: {envelope}"
    );

    let text = heddle_output(
        &["--output", "text", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke heddle resolve text");
    assert!(
        !text.status.success(),
        "resolve with no merge should exit non-zero in text mode"
    );
    assert!(
        text.stdout.is_empty(),
        "text failure should not write primary output: {}",
        String::from_utf8_lossy(&text.stdout)
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("Error: No merge in progress")
            && text_stderr.contains("Next: heddle status")
            && text_stderr.contains("heddle status")
            && !text_stderr.contains("object not found"),
        "resolve text recovery should name the operation state directly: {text_stderr}"
    );
}

#[test]
fn resolve_with_no_remaining_conflicts_keeps_full_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo.current_state().unwrap().unwrap().change_id;
    let merge_state = repo.merge_state_manager();
    merge_state
        .start(head, head, None, vec!["tracked.txt".to_string()])
        .unwrap();
    merge_state.resolve("tracked.txt").unwrap();

    let output = heddle_output(
        &["--output", "text", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke resolve with no remaining conflicts");
    assert!(
        !output.status.success(),
        "resolve --all with no unresolved conflicts should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "text failure should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: No conflicts to resolve")
            && stderr.contains("Next: heddle resolve --list")
            && !stderr.contains("Unsafe:")
            && !stderr.contains("Would change:")
            && !stderr.contains("Preserved:")
            && !stderr.contains("Primary recovery:"),
        "typed no-conflicts refusal should keep default text concise: {stderr}"
    );

    let verbose = heddle_output(
        &["-v", "--output", "text", "resolve", "--all", "--ours"],
        Some(temp.path()),
    )
    .expect("invoke resolve with no remaining conflicts in verbose mode");
    let verbose_stderr = String::from_utf8_lossy(&verbose.stderr);
    assert!(
        verbose_stderr.contains("Error: No conflicts to resolve")
            && verbose_stderr.contains("Next: heddle resolve --list")
            && verbose_stderr.contains("Unsafe:")
            && verbose_stderr.contains("Would change:")
            && verbose_stderr.contains("Preserved:")
            && verbose_stderr.contains("Hint:")
            && verbose_stderr.contains("heddle resolve --list"),
        "verbose typed no-conflicts refusal should expose full advice detail: {verbose_stderr}"
    );
}

#[test]
fn heavy_thread_start_explains_non_empty_workspace_recovery() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["init"], Some(&repo)).unwrap();
    std::fs::write(repo.join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(&repo)).unwrap();

    let target = temp.path().join("already-used");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("draft.txt"), "uncaptured").unwrap();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "ux-thread",
            "--path",
            target.to_str().expect("path should be utf8"),
        ],
        Some(&repo),
    )
    .expect("non-empty materialized worktree should fail with guidance");
    assert!(
        !output.status.success(),
        "non-empty materialized worktree should fail"
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("stderr should be utf8");
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("failure should use JSON advice envelope: {err}: {stderr}"));

    assert_eq!(envelope["kind"], "worktree_target_not_empty");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("is not empty")),
        "thread start should name the unsafe target: {envelope}"
    );
    assert_eq!(
        envelope["primary_command"],
        "heddle start <name> --workspace materialized"
    );
    assert!(
        envelope["recovery_commands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command == "heddle capture -m \"...\"")),
        "thread start should preserve capture recovery guidance: {envelope}"
    );
}

#[test]
fn thread_list_groups_threads_by_user_workflow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = sibling_checkout_path(temp.path(), "feature-work");
    heddle(
        &[
            "start",
            "feature-work",
            "--path",
            thread_path.to_str().unwrap(),
            "--task",
            "demo",
        ],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(thread_path.join("feature.txt"), "feature").unwrap();
    heddle(
        &["capture", "-m", "feature", "--confidence", "0.8"],
        Some(&thread_path),
    )
    .unwrap();

    let output = heddle(&["--output", "text", "thread", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Current"),
        "thread list should group current work: {output}"
    );
    assert!(
        output.contains("Ready to merge"),
        "thread list should group mergeable work: {output}"
    );
    assert!(
        output.contains("next step:"),
        "thread list should use consistent next-step copy: {output}"
    );
    assert!(
        !output.contains("    next:"),
        "thread list should not use the older lowercase next label: {output}"
    );
    assert!(
        !output.contains("lifecycle:") && !output.contains("git tip:"),
        "default thread list should keep internal state and Git tips out of the first-run view: {output}"
    );
    let verbose = heddle(
        &["-v", "--output", "text", "thread", "list"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        verbose.contains("lifecycle:"),
        "verbose thread list should keep lifecycle detail available: {verbose}"
    );
}

#[test]
fn default_thread_and_workspace_cap_optional_git_only_refs() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("main.txt"), "main\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "main seed");
    for index in 0..7 {
        let branch = format!("side-{index}");
        let status = std::process::Command::new("git")
            .args(["checkout", "-b", &branch, "main"])
            .current_dir(temp.path())
            .status()
            .expect("git checkout side branch should run");
        assert!(status.success(), "git checkout {branch} should succeed");
        std::fs::write(
            temp.path().join(format!("side-{index}.txt")),
            format!("side {index}\n"),
        )
        .unwrap();
        git_commit_all_for_json_contract(temp.path(), &format!("side {index}"));
        let status = std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(temp.path())
            .status()
            .expect("git checkout main should run");
        assert!(status.success(), "git checkout main should succeed");
    }
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    for (label, args) in [
        ("thread list", &["thread", "list", "--output", "text"][..]),
        ("workspace", &["workspace", "show", "--output", "text"][..]),
    ] {
        let text = heddle(args, Some(temp.path())).unwrap();
        assert!(
            text.contains("Optional Git-only branches"),
            "{label} should still surface optional Git-only refs: {text}"
        );
        let visible = text.matches("[available]").count() + text.matches("(available)").count();
        assert!(
            visible <= 5,
            "{label} should cap optional Git-only refs in default text: {text}"
        );
        assert!(
            text.contains("... 2 more Git-only branch(es)")
                && text.contains("use --output json or -v to inspect all"),
            "{label} should explain hidden optional refs: {text}"
        );
        assert!(
            !text.contains("git tip:"),
            "{label} default text should hide Git tips: {text}"
        );
    }

    let thread_json = json_value(temp.path(), &["thread", "list", "--output", "json"]);
    assert_eq!(
        thread_json["available_git_refs"].as_array().map(Vec::len),
        Some(7),
        "thread JSON should keep all optional Git-only refs: {thread_json}"
    );
    let workspace_json = json_value(temp.path(), &["workspace", "show", "--output", "json"]);
    assert_eq!(
        workspace_json["available_git_refs"]
            .as_array()
            .map(Vec::len),
        Some(7),
        "workspace JSON should keep all optional Git-only refs: {workspace_json}"
    );
}

#[test]
fn output_json_renders_json_without_polluting_machine_stderr() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["status", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        output.status.success(),
        "status --output json should succeed"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout should be JSON when --output json is passed: {stdout}"
    );
    assert!(
        stderr.is_empty(),
        "--output json must not pollute machine stderr: {stderr}"
    );
}

#[test]
fn legacy_global_json_flag_is_not_supported() {
    let output = heddle_output(&["--json", "commands"], None).expect("invoke heddle");
    assert!(!output.status.success(), "legacy --json should be rejected");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("unexpected argument '--json'")
            || stderr.contains("unknown argument '--json'"),
        "clap should explain that --json is no longer accepted: {stderr}"
    );

    let output = heddle_output(&["watch", "--json"], None).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "command-local legacy --json should also be rejected"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("unexpected argument '--json'")
            || stderr.contains("unknown argument '--json'"),
        "clap should explain that watch --json is no longer accepted: {stderr}"
    );
}

#[test]
fn quiet_no_color_and_narrow_text_outputs_preserve_global_contract() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();

    let default_capture = heddle_output(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    assert!(default_capture.status.success());

    std::fs::write(temp.path().join("more.txt"), "more\n").unwrap();
    let quiet_capture =
        heddle_output(&["--quiet", "capture", "-m", "more"], Some(temp.path())).unwrap();
    assert!(quiet_capture.status.success());
    let quiet_stderr = std::str::from_utf8(&quiet_capture.stderr).unwrap();
    assert!(
        quiet_stderr.is_empty(),
        "--quiet must suppress nonessential tips/logs on stderr: {quiet_stderr}"
    );

    let no_color = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(no_color.status.success());
    let stdout = std::str::from_utf8(&no_color.stdout).unwrap();
    let stderr = std::str::from_utf8(&no_color.stderr).unwrap();
    assert!(
        stderr.is_empty(),
        "status text success should keep stderr quiet: {stderr}"
    );
    assert!(
        !stdout.contains('\u{1b}') && !stderr.contains('\u{1b}'),
        "NO_COLOR must override forced color: stdout={stdout:?} stderr={stderr:?}"
    );

    let narrow = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("NO_COLOR", "1"), ("COLUMNS", "30")],
    )
    .unwrap();
    assert!(narrow.status.success());
    let narrow_stdout = std::str::from_utf8(&narrow.stdout).unwrap();
    let narrow_stderr = std::str::from_utf8(&narrow.stderr).unwrap();
    assert!(
        narrow_stderr.is_empty(),
        "narrow text status should not need stderr: {narrow_stderr}"
    );
    assert!(
        narrow_stdout.contains("Heddle status") && narrow_stdout.contains("Verdict:"),
        "narrow status should retain the primary labels: {narrow_stdout}"
    );
    assert!(
        !narrow_stdout.contains('\u{1b}'),
        "NO_COLOR narrow output must not contain ANSI escapes: {narrow_stdout:?}"
    );
}

#[test]
fn narrow_no_color_text_outputs_cover_everyday_read_surfaces() {
    fn assert_text_surface(cwd: &std::path::Path, args: Vec<&str>, needles: &[&str]) {
        let output = heddle_output_with_env(
            &args,
            Some(cwd),
            &[
                ("NO_COLOR", "1"),
                ("CLICOLOR_FORCE", "1"),
                ("COLUMNS", "28"),
            ],
        )
        .unwrap_or_else(|err| panic!("invoke heddle {args:?}: {err}"));
        assert!(
            output.status.success(),
            "narrow text command should succeed for {args:?}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "narrow text success should keep stderr quiet for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains('\u{1b}'),
            "NO_COLOR must suppress ANSI for {args:?}: {stdout:?}"
        );
        assert!(
            !stdout.contains("git+heddle-sidecar") && !stdout.contains("Storage:"),
            "normal text output should avoid storage-model jargon for {args:?}: {stdout}"
        );
        for needle in needles {
            assert!(
                stdout.contains(needle),
                "narrow text output for {args:?} should retain {needle:?}: {stdout}"
            );
        }
    }

    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::create_dir_all(temp.path().join("src/deeply-nested-module")).unwrap();
    std::fs::write(
        temp.path()
            .join("src/deeply-nested-module/very-long-file-name-for-narrow-output.txt"),
        "base\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path()
            .join("src/deeply-nested-module/very-long-file-name-for-narrow-output.txt"),
        "base\nchanged\n",
    )
    .unwrap();

    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "status"],
        &["Heddle status", "Verdict:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "diagnose"],
        &["Doctor", "Health:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "doctor"],
        &["Doctor", "Next step:"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "diff"],
        &["+changed"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "log"],
        &["base"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "show", "HEAD"],
        &["State", "base"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "thread", "list"],
        &["Current"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "workspace", "show"],
        &["Workspace", "main"],
    );
    // The `Repository:` mode preamble is dropped from the default read
    // view (heddle#275); the everyday surface leads with bridge state.
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "bridge", "git", "status"],
        &["Git import"],
    );
    assert_text_surface(
        temp.path(),
        vec!["--quiet", "--output", "text", "fsck", "--bridge"],
        &["repository is valid", "Bridge:"],
    );

    let ready = heddle_output_with_env(
        &["--quiet", "--output", "text", "ready"],
        Some(temp.path()),
        &[
            ("NO_COLOR", "1"),
            ("CLICOLOR_FORCE", "1"),
            ("COLUMNS", "28"),
        ],
    )
    .expect("invoke ready narrow text");
    assert!(
        !ready.status.success(),
        "blocked ready should exit nonzero while still rendering narrow text"
    );
    assert!(ready.stderr.is_empty(), "ready should keep stderr quiet");
    let ready_stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(
        !ready_stdout.contains('\u{1b}') && ready_stdout.contains("Readiness"),
        "ready narrow text should be no-color and retain labels: {ready_stdout}"
    );
    assert!(
        !ready_stdout.contains("heddle merge main"),
        "ready narrow text must avoid stale self-merge guidance: {ready_stdout}"
    );
}

#[test]
fn default_run_does_not_leak_info_traces() {
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["init"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "init should succeed");

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        !stderr.contains("INFO"),
        "default verbosity should suppress INFO traces (got: {stderr:?})"
    );
}

#[test]
fn verbose_flag_re_enables_info_traces() {
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["-v", "init"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "init -v should succeed");

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("INFO"),
        "-v should restore INFO-level traces (got: {stderr:?})"
    );
}

#[test]
fn missing_repo_status_emits_hint_in_text_mode() {
    let temp = TempDir::new().unwrap();
    let output =
        heddle_output(&["--output", "text", "status"], Some(temp.path())).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on non-repo dir should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("Error:"),
        "stderr should carry an Error: line: {stderr}"
    );
    assert!(
        stderr.contains("repository not found"),
        "stderr should name the actual failure: {stderr}"
    );
    assert!(
        stderr.contains("Next: heddle init"),
        "stderr should suggest `heddle init`: {stderr}"
    );
    assert!(
        stderr.contains(temp.path().to_str().expect("temp path utf8")),
        "stderr should include the path that would be initialized: {stderr}"
    );
}

#[test]
fn missing_repo_status_emits_structured_error_in_json_mode() {
    let temp = TempDir::new().unwrap();
    let output =
        heddle_output(&["--output", "json", "status"], Some(temp.path())).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on non-repo dir should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a single-line JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "repository_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .unwrap_or("")
            .contains("repository not found"),
        "envelope.error should name the failure: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains(temp.path().to_str().expect("temp path utf8")),
        "envelope.hint should suggest initializing the requested path: {envelope}"
    );
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["init", temp.path().to_str().expect("temp path utf8")])
    );
}

#[test]
fn missing_repo_path_emits_actionable_json_error_envelope() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing-repo");
    let output = heddle_output(
        &[
            "--repo",
            missing.to_str().expect("path should be utf8"),
            "--output",
            "json",
            "status",
        ],
        None,
    )
    .expect("invoke heddle");
    assert!(
        !output.status.success(),
        "status on a missing --repo path should exit non-zero"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "JSON failure must not pollute stdout: {stdout}"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a single-line JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "path_not_found");
    assert!(
        envelope["hint"].as_str().unwrap_or("").contains("--repo"),
        "missing path errors should point at --repo recovery: {envelope}"
    );
}

#[test]
fn global_flags_only_renders_curated_help_not_clap_error() {
    // The user typed `heddle --output text` with no subcommand. Without the
    // intercept, clap would dump a 60+ verb wall of text. With it, the
    // contract-curated native loop renders cleanly.
    let output = heddle_output(&["--output", "text"], None).expect("invoke heddle");
    assert!(
        output.status.success(),
        "global-flags-only invocation should print help and exit 0"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stdout.contains("Heddle") && stdout.contains("Common loop:"),
        "curated help should render: stdout={stdout}"
    );
    assert!(
        !stdout.contains("compatibility"),
        "default help should not frame Git adapter commands as compatibility: {stdout}"
    );
    for verb in [
        "status", "diff", "commit", "start", "ready", "merge", "ship",
    ] {
        assert!(
            stdout.contains(&format!("\n  {verb}")),
            "core-loop verb `{verb}` should be on the curated surface: {stdout}"
        );
    }
    for verb in [
        "review", "discuss", "context", "goto", "thread", "bridge", "push", "pull", "doctor",
        "verify", "init", "adopt", "clone", "log", "show",
    ] {
        assert!(
            !stdout.contains(&format!("\n  {verb}")),
            "non-core verb `{verb}` should stay behind advanced/topic help: {stdout}"
        );
    }
    assert!(
        stdout.contains("Nearby: `heddle undo`, `heddle verify`, `heddle push`, `heddle pull`.")
            && stdout.contains("Start here: `heddle init`, `heddle adopt`, or `heddle clone`."),
        "default help should keep adjacent commands discoverable without expanding the first-screen loop: {stdout}"
    );
    assert!(
        stdout.contains("Existing Git: heddle status -> heddle adopt -> heddle verify -> heddle commit -m \"...\" -> heddle push")
            && stdout
                .contains("Isolated work: heddle start <name> --path ../<name> -> heddle ready -> heddle merge --preview -> heddle ship"),
        "default help should connect first-run adoption and isolated work to the same product loop: {stdout}"
    );
    assert!(
        !stdout.contains("error: 'heddle' requires a subcommand"),
        "clap's missing-subcommand error must not surface: stdout={stdout}"
    );
    assert!(
        !stderr.contains("error: 'heddle' requires a subcommand"),
        "clap's missing-subcommand error must not surface on stderr: stderr={stderr}"
    );
}

#[test]
fn global_flags_only_json_renders_command_catalog_for_agents() {
    let output = heddle_output(&["--output", "json"], None).expect("invoke heddle");
    assert!(
        output.status.success(),
        "JSON global-flags-only invocation should print the catalog and exit 0"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.trim().is_empty(),
        "catalog discovery should not emit stderr noise: {stderr}"
    );
    assert!(
        !stdout.contains("Native loop:"),
        "JSON discovery should not return prose help: {stdout}"
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|_| panic!("stdout should be command catalog JSON: {stdout}"));
    assert_eq!(parsed["kind"], "command_catalog");
    assert!(
        parsed["commands"].as_array().is_some_and(|commands| {
            commands.iter().any(|command| {
                command["display"] == "commands"
                    && command["supports_json"] == true
                    && command["side_effect_class"] == "observe_only"
            })
        }),
        "catalog JSON should expose the command contract table: {parsed}"
    );
}

#[test]
fn advanced_help_does_not_repeat_everyday_human_path() {
    let advanced = heddle(&["help", "advanced"], None).expect("advanced help should render");
    assert!(
        advanced.contains(
            "Advanced commands for power users, agents, automation, Git interop, and recovery."
        ),
        "advanced help should explain why this surface exists: {advanced}"
    );
    assert!(
        !advanced.contains("compatibility"),
        "advanced help should not frame Git adapter commands as compatibility: {advanced}"
    );
    assert!(
        !advanced.contains("see `heddle help advanced`"),
        "advanced help should not be self-referential: {advanced}"
    );
    for verb in ["commit", "ship", "push"] {
        assert!(
            !advanced.contains(&format!("\n  {verb}")),
            "`{verb}` is an everyday path and should not be duplicated in advanced help: {advanced}"
        );
    }

    let push_help = heddle(&["push", "--help"], None).expect("push help should render");
    assert!(
        push_help.contains("Remote name, local path, URL, or hosted address"),
        "push help should match Git-overlay and hosted reality, not only host:port remotes: {push_help}"
    );
    let pull_help = heddle(&["pull", "--help"], None).expect("pull help should render");
    assert!(
        pull_help.contains("Remote name, local path, URL, or hosted address"),
        "pull help should match Git-overlay and hosted reality, not only host:port remotes: {pull_help}"
    );

    let operation_ids =
        heddle(&["help", "operation-ids"], None).expect("operation ids help should render");
    assert!(
        operation_ids.contains("supports_op_id: true")
            && operation_ids.contains("op_id_behavior: explicit_replay")
            && operation_ids.contains("generated_resume")
            && operation_ids.contains("reserved")
            && operation_ids.contains("heddle commands --output json"),
        "operation-id help should defer to the command contract table: {operation_ids}"
    );

    let capture_help = heddle(&["capture", "--help"], None).expect("capture help should render");
    assert!(
        !capture_help.contains("HEDDLE_SESSION_ID")
            && !capture_help.contains("HEDDLE_SESSION_SEGMENT"),
        "capture help should not advertise unimplemented environment variables: {capture_help}"
    );
    for hidden in [
        "--agent-provider",
        "--agent-model",
        "--agent-session",
        "--agent-segment",
        "--policy",
        "--no-policy",
        "--no-agent",
        "--split",
    ] {
        assert!(
            !capture_help.contains(hidden),
            "capture help should keep advanced attribution/split controls out of the first-run surface: {capture_help}"
        );
    }

    let start_help = heddle(&["start", "--help"], None).expect("start help should render");
    for hidden in [
        "--agent-provider",
        "--agent-model",
        "--print-cd-path",
        "--daemon",
        "--no-daemon",
        "--shared-target",
        "FUSE",
        "heddled",
    ] {
        assert!(
            !start_help.contains(hidden),
            "start help should keep advanced checkout machinery out of the first-run surface: {start_help}"
        );
    }
    assert!(
        start_help.contains("Copy full files into an isolated checkout")
            && start_help.contains("Create a disk checkout with shared extents"),
        "start help should describe workspace modes in human language: {start_help}"
    );

    let clone_help = heddle(&["clone", "--help"], None).expect("clone help should render");
    for hidden in ["--lazy", "--filter", "v0.3.1", "blob:none"] {
        assert!(
            !clone_help.contains(hidden),
            "clone help should not lead with planned partial-clone machinery: {clone_help}"
        );
    }

    let promote_help =
        heddle(&["thread", "promote", "--help"], None).expect("thread promote help should render");
    assert!(
        !promote_help.contains("heavy checkout"),
        "thread promote help should use product-facing workspace language: {promote_help}"
    );

    let try_help = heddle(&["try", "--help"], None).expect("try help should render");
    assert!(
        try_help.contains("Defaults to `materialized`")
            && try_help.contains("auto")
            && try_help.contains("virtualized")
            && try_help.contains("solid")
            && !try_help.contains("Defaults to `heavy`"),
        "try help should use current workspace mode terms: {try_help}"
    );

    let attempt_help = heddle(&["attempt", "--help"], None).expect("attempt help should render");
    assert!(
        attempt_help.contains("Defaults to `materialized`")
            && attempt_help.contains("auto")
            && attempt_help.contains("virtualized")
            && attempt_help.contains("solid")
            && !attempt_help.contains("Defaults to `heavy`"),
        "attempt help should use current workspace mode terms: {attempt_help}"
    );
}

#[test]
fn thread_show_hides_agent_internals_until_verbose() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    heddle(
        &[
            "actor",
            "spawn",
            "--thread",
            "main",
            "--provider",
            "openai",
            "--model",
            "codex",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let text = heddle(
        &["thread", "show", "main", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    for hidden in [
        "Actor:",
        "Session:",
        "Heddle session:",
        "Harness:",
        "Thinking:",
        "Report flush:",
        "Attach:",
        "Usage:",
    ] {
        assert!(
            !text.contains(hidden),
            "non-verbose thread show should hide agent internals `{hidden}`: {text}"
        );
    }
    assert!(
        !text.contains("Base root:") && !text.contains("Base tree:"),
        "non-verbose thread show should not render raw base tree hashes: {text}"
    );
    for hidden in [
        "Base:",
        "Current:",
        "Git tip:",
        "History:",
        "Lifecycle:",
        "Last activity:",
        "Recent saved states",
    ] {
        assert!(
            !text.contains(hidden),
            "non-verbose thread show should hide history/detail field `{hidden}`: {text}"
        );
    }
    assert!(
        text.contains("Thread: main") && text.contains("(current)") && text.contains("Status:"),
        "non-verbose thread show should keep the workflow state visible: {text}"
    );

    let verbose = heddle(
        &["-v", "thread", "show", "main", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        verbose.contains("Actor:") && verbose.contains("Session:") && verbose.contains("Attach:"),
        "verbose thread show should expose agent internals for debugging: {verbose}"
    );
    assert!(
        verbose.contains("Base:")
            && verbose.contains("Current:")
            && verbose.contains("Lifecycle:")
            && verbose.contains("Last activity:")
            && verbose.contains("Recent saved states"),
        "verbose thread show should keep state IDs, lifecycle, activity, and history available: {verbose}"
    );
}

#[test]
fn merge_preview_blocks_uncaptured_isolated_source_checkout() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "dirty-feature");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    heddle(
        &["start", "feature/dirty-source", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(checkout.join("feature.txt"), "uncaptured\n").unwrap();

    let preview = heddle_output(
        &[
            "merge",
            "feature/dirty-source",
            "--preview",
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !preview.status.success(),
        "dirty source preview should fail closed instead of returning a successful blocked payload"
    );
    assert!(
        preview.stdout.is_empty(),
        "JSON-mode refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let stderr = String::from_utf8_lossy(&preview.stderr);
    let preview: Value =
        serde_json::from_str(&stderr).expect("dirty source preview should emit JSON envelope");
    assert_eq!(preview["kind"], "source_thread_uncaptured_work");
    assert_json_recovery_advice_fields(&preview, "dirty source merge preview");
    assert!(
        preview["error"]
            .as_str()
            .is_some_and(|message| message.contains("merge preview did not run")),
        "dirty source preview should not claim an up-to-date merge: {preview}"
    );
    assert!(
        preview["primary_command"]
            .as_str()
            .is_some_and(|action| action.contains("ready -m \"Save source work\"")),
        "dirty source preview should point back to ready capture: {preview}"
    );
    assert!(
        preview["unsafe_condition"]
            .as_str()
            .is_some_and(|condition| condition.contains("feature.txt")),
        "dirty source preview should list uncaptured source paths: {preview}"
    );

    let text = heddle_output(
        &[
            "merge",
            "feature/dirty-source",
            "--preview",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !text.status.success(),
        "text dirty source preview should fail closed"
    );
    let text = String::from_utf8_lossy(&text.stderr);
    assert!(
        text.contains("merge preview did not run")
            && text.contains("uncaptured path(s): feature.txt")
            && text.contains("Next:")
            && text.contains("ready -m \"Save source work\"")
            && !text.contains("Already up to date"),
        "text preview should fail closed on source checkout dirtiness: {text}"
    );
}

#[test]
fn isolated_thread_status_and_diff_report_untracked_only_work() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "untracked-only");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    heddle(
        &["start", "feature/untracked-only", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::create_dir_all(checkout.join("docs")).unwrap();
    std::fs::write(checkout.join("docs/new.md"), "new isolated work\n").unwrap();

    let status = json_value(&checkout, &["status", "--output", "json"]);
    assert_eq!(
        status["changed_path_count"], 1,
        "isolated status must not report clean when the only change is a new file: {status}"
    );
    assert_eq!(
        status["worktree_changed_path_count"], 1,
        "isolated status should expose dirty worktree path count separately: {status}"
    );
    assert_eq!(
        status["thread_changed_path_count"], 0,
        "unsaved isolated work is not yet captured thread delta: {status}"
    );
    assert_eq!(
        status["changes"]["added"],
        serde_json::json!(["docs/new.md"]),
        "isolated status should surface untracked-only work as added: {status}"
    );
    assert_eq!(
        status["recommended_action"], "heddle commit -m \"...\"",
        "untracked-only isolated work should point to capture/commit before readiness: {status}"
    );

    let diff = json_value(&checkout, &["diff", "--name-only", "--output", "json"]);
    assert_eq!(
        diff["changed_path_count"], 1,
        "isolated diff must share the same worktree observation as status: {diff}"
    );
    assert_eq!(
        diff["changes"]["added"][0]["path"], "docs/new.md",
        "isolated diff should list the new file under the added category without needing a tracked-file edit: {diff}"
    );
}

#[test]
fn isolated_thread_capture_points_to_ready_not_checkpoint_tip() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "feature-checkout");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    heddle(
        &["start", "feature/capture-next", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();

    let capture = heddle(
        &["capture", "-m", "feature work", "--output", "text"],
        Some(&checkout),
    )
    .unwrap();
    assert!(
        capture.contains("Next:") && capture.contains("heddle ready"),
        "isolated feature capture should point to readiness: {capture}"
    );
    assert!(
        !capture.contains("checkpoint"),
        "isolated feature capture should not nudge toward a Git checkpoint: {capture}"
    );

    let ready = heddle(&["ready", "--output", "text"], Some(&checkout)).unwrap();
    assert!(
        ready.contains("Next:")
            && ready.contains("heddle --repo")
            && ready.contains("merge feature/capture-next --preview"),
        "ready should use a shared next-action label that runs from the isolated checkout: {ready}"
    );
    assert!(
        !ready.contains("\nnext:"),
        "ready should not use the older lowercase next label: {ready}"
    );

    let checkout_status = json_value(&checkout, &["status", "--output", "json"]);
    assert_eq!(
        checkout_status["recommended_action"],
        format!(
            "heddle --repo {} merge feature/capture-next --preview",
            temp.path().display()
        )
    );
    assert_eq!(
        checkout_status["recommended_action_template"]["argv_template"],
        heddle_argv_json([
            "--repo",
            temp.path().to_str().expect("repo path utf8"),
            "merge",
            "feature/capture-next",
            "--preview",
        ]),
        "status inside an isolated checkout should emit a runnable parent-repo merge action: {checkout_status}"
    );
    assert_eq!(
        checkout_status["verification"]["recommended_action"],
        checkout_status["recommended_action"],
        "status verification should not keep the parent-repo merge action in raw, non-contextual form: {checkout_status}"
    );
    assert_eq!(
        checkout_status["verification"]["recommended_action_template"]["argv_template"],
        checkout_status["recommended_action_template"]["argv_template"],
        "status verification argv should match the contextual top-level merge action: {checkout_status}"
    );
    let checkout_thread_show = json_value(
        &checkout,
        &["thread", "show", "feature/capture-next", "--output", "json"],
    );
    assert_eq!(
        checkout_thread_show["recommended_action"], checkout_status["recommended_action"],
        "thread show inside an isolated checkout should emit the same runnable parent-repo merge action: {checkout_thread_show}"
    );
    assert_eq!(
        checkout_thread_show["verification"]["recommended_action"],
        checkout_thread_show["recommended_action"],
        "thread show verification should match its contextual top-level merge action: {checkout_thread_show}"
    );
    let checkout_workspace = json_value(&checkout, &["workspace", "show", "--output", "json"]);
    assert_eq!(
        checkout_workspace["recommended_action"], checkout_status["recommended_action"],
        "workspace show inside an isolated checkout should emit the same runnable parent-repo merge action: {checkout_workspace}"
    );
    assert_eq!(
        checkout_workspace["verification"]["recommended_action"],
        checkout_workspace["recommended_action"],
        "workspace verification should match its contextual top-level merge action: {checkout_workspace}"
    );
    let checkout_status_text = heddle(&["status", "--output", "text"], Some(&checkout)).unwrap();
    assert!(
        checkout_status_text.contains("heddle --repo")
            && checkout_status_text.contains("merge feature/capture-next --preview"),
        "status text inside an isolated checkout should point to the parent repo: {checkout_status_text}"
    );

    let preview = heddle(
        &[
            "merge",
            "feature/capture-next",
            "--preview",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        preview.contains("Next:")
            && preview.contains("heddle ship --thread feature/capture-next --no-push"),
        "merge preview should use the shared next-action label: {preview}"
    );
    assert!(
        !preview.contains("recommended action:"),
        "merge preview should not use a separate lowercase recommendation label: {preview}"
    );

    let contextual_preview = json_value(
        &checkout,
        &[
            "--repo",
            temp.path().to_str().expect("repo path utf8"),
            "merge",
            "feature/capture-next",
            "--preview",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        contextual_preview["recommended_action"],
        format!(
            "heddle --repo {} ship --thread feature/capture-next --no-push",
            temp.path().display()
        ),
        "merge preview invoked from an isolated checkout must preserve parent repo context: {contextual_preview}"
    );
    assert_eq!(
        contextual_preview["recommended_action_template"]["argv_template"],
        heddle_argv_json([
            "--repo",
            temp.path().to_str().expect("repo path utf8"),
            "ship",
            "--thread",
            "feature/capture-next",
            "--no-push",
        ]),
        "merge preview argv must be directly runnable from the isolated checkout: {contextual_preview}"
    );

    let checkout_after_preview = json_value(&checkout, &["status", "--output", "json"]);
    assert_eq!(
        checkout_after_preview["recommended_action"],
        format!(
            "heddle --repo {} ship --thread feature/capture-next --no-push",
            temp.path().display()
        )
    );
    assert_eq!(
        checkout_after_preview["recommended_action_template"]["argv_template"],
        heddle_argv_json([
            "--repo",
            temp.path().to_str().expect("repo path utf8"),
            "ship",
            "--thread",
            "feature/capture-next",
            "--no-push",
        ]),
        "status inside an isolated checkout should emit a runnable parent-repo ship action after preview: {checkout_after_preview}"
    );
    assert_eq!(
        checkout_after_preview["verification"]["recommended_action"],
        checkout_after_preview["recommended_action"],
        "status verification should match the contextual parent-repo ship action after preview: {checkout_after_preview}"
    );
    assert_eq!(
        checkout_after_preview["verification"]["recommended_action_template"]["argv_template"],
        checkout_after_preview["recommended_action_template"]["argv_template"],
        "status verification argv should match the contextual parent-repo ship action after preview: {checkout_after_preview}"
    );

    let ship = heddle(
        &[
            "ship",
            "--thread",
            "feature/capture-next",
            "--no-push",
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        ship.contains("landed: on parent") && ship.contains("push: not pushed"),
        "ship should report the landed value state and push state: {ship}"
    );
    assert!(
        !ship.contains("completed:")
            && !ship.contains("up to date:")
            && !ship.contains("integrated: yes"),
        "ship should not render step accounting as the primary human output: {ship}"
    );
    assert!(
        ship.contains("Next:") && ship.contains("heddle thread cleanup --merged --dry-run"),
        "ship should surface the safe cleanup path for merged isolated checkouts: {ship}"
    );

    let list = heddle(&["thread", "list", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        list.contains("feature/capture-next")
            && list.contains("lifecycle: merged")
            && list.contains("next step: heddle thread cleanup --merged --dry-run"),
        "thread list should make merged checkout cleanup discoverable: {list}"
    );

    let show = heddle(
        &["thread", "show", "feature/capture-next", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        show.contains("Lifecycle: merged")
            && show.contains("Next step: heddle thread cleanup --merged --dry-run")
            && !show.contains("\nSync:"),
        "thread show should make merged checkout cleanup discoverable: {show}"
    );

    let cleanup = heddle(
        &["thread", "cleanup", "--merged", "--output", "text"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        cleanup.contains("dropped 1 merged thread(s)"),
        "cleanup should confirm that the merged thread was removed from active surfaces: {cleanup}"
    );
    let list_after_cleanup =
        heddle(&["thread", "list", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        !list_after_cleanup.contains("feature/capture-next"),
        "default thread list should not keep showing a merged thread after cleanup: {list_after_cleanup}"
    );
}

#[test]
fn workspace_bare_command_defaults_to_show() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(&["--output", "text", "workspace"], Some(temp.path()))
        .expect("bare workspace should render the default workspace view");
    assert!(
        text.contains("Workspace:")
            && text.contains("Current thread")
            && text.contains("Visible threads:"),
        "bare workspace should behave like workspace show, not print subcommand help: {text}"
    );
    assert!(
        !text.contains("git tip:")
            && !text.contains("last activity:")
            && !text.contains("    next:"),
        "default workspace view should hide detail rows and use shared next-step copy: {text}"
    );

    let verbose_text = heddle(&["-v", "--output", "text", "workspace"], Some(temp.path()))
        .expect("verbose bare workspace should render detail rows");
    assert!(
        verbose_text.contains("last activity:"),
        "verbose workspace should keep activity detail available: {verbose_text}"
    );

    let json = heddle(&["--output", "json", "workspace"], Some(temp.path()))
        .expect("bare workspace should support JSON through the default show view");
    let parsed: Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("bare workspace JSON should parse: {json}"));
    assert!(
        parsed["repository_capability"].as_str().is_some(),
        "workspace JSON should identify repository capability: {json}"
    );
    assert!(
        parsed["groups"].is_array(),
        "workspace JSON should expose groups: {json}"
    );
}

#[test]
fn command_catalog_exposes_public_surface_for_agents() {
    let json = heddle(&["commands", "--output", "json"], None)
        .expect("command catalog JSON should succeed");
    let parsed: Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("command catalog JSON should parse: {json}"));
    assert_eq!(
        parsed["executable_path"],
        env!("CARGO_BIN_EXE_heddle"),
        "catalog should tell agents which binary produced replayable argv: {json}"
    );
    let commands = parsed["commands"]
        .as_array()
        .expect("commands should be an array");
    assert!(
        commands.len() > 40,
        "catalog should enumerate the public command tree: {json}"
    );
    let status = commands
        .iter()
        .find(|entry| entry["display"] == "status")
        .expect("status command should be cataloged");
    assert_eq!(status["tier"], "everyday");
    assert_eq!(status["surface"], "native");
    assert_eq!(status["help_visibility"], "everyday");
    assert_eq!(status["help_rank"], 10);
    assert_eq!(status["canonical_command"], Value::Null);
    assert_eq!(status["canonical_action"], Value::Null);
    let ready = commands
        .iter()
        .find(|entry| entry["display"] == "ready")
        .expect("ready command should be cataloged");
    assert!(
        ready["summary"]
            .as_str()
            .is_some_and(|summary| summary.starts_with("Prepare this thread")
                && !summary.contains("Automation/workflow command")),
        "catalog summaries should use product language, not internal clap framing: {ready}"
    );
    let adopt = commands
        .iter()
        .find(|entry| entry["display"] == "adopt")
        .expect("adopt command should be cataloged");
    assert!(
        adopt["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alias| alias == "import"),
        "runtime aliases should be exposed by the command catalog: {adopt}"
    );
    assert_eq!(
        adopt["command_action"]["action"],
        "heddle adopt --ref <branch>"
    );
    assert_eq!(adopt["command_action"]["executable"], false);
    assert_eq!(adopt["command_action"]["argv"], Value::Null);
    assert_eq!(
        adopt["command_action"]["template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "<branch>"])
    );
    assert_eq!(
        adopt["command_action"]["template"]["required_inputs"],
        serde_json::json!(["branch"])
    );
    assert_eq!(adopt["command_action"]["template"]["agent_may_fill"], true);
    for display in ["init", "adopt", "clone"] {
        let entry = commands
            .iter()
            .find(|entry| entry["display"] == display)
            .unwrap_or_else(|| panic!("{display} command should be cataloged"));
        assert_eq!(
            entry["supports_op_id"], true,
            "{display} should advertise first-contact op-id replay support: {entry}"
        );
        assert_eq!(
            entry["persists_op_id"], false,
            "{display} should not advertise generated op-id persistence: {entry}"
        );
        assert_eq!(
            entry["op_id_behavior"], "explicit_replay",
            "{display} should make its op-id contract explicit: {entry}"
        );
        assert_eq!(
            entry["op_id_store_scope"], "bootstrap",
            "{display} should advertise bootstrap op-id scope: {entry}"
        );
    }
    let push = commands
        .iter()
        .find(|entry| entry["display"] == "push")
        .expect("push command should be cataloged");
    assert_eq!(push["command_action"]["action"], "heddle push");
    assert_eq!(push["command_action"]["executable"], true);
    assert_eq!(push["command_action"]["argv"], heddle_argv_json(["push"]));
    assert_eq!(push["command_action"]["template"], Value::Null);
    let commit = commands
        .iter()
        .find(|entry| entry["display"] == "commit")
        .expect("commit command should be cataloged");
    assert_eq!(
        commit["command_action"]["action"],
        "heddle commit -m <message>"
    );
    assert_eq!(commit["command_action"]["executable"], false);
    assert_eq!(commit["command_action"]["argv"], Value::Null);
    assert_eq!(
        commit["command_action"]["template"]["argv_template"],
        heddle_argv_json(["commit", "-m", "<message>"])
    );
    assert_eq!(
        commit["command_action"]["template"]["required_inputs"],
        serde_json::json!(["message"])
    );
    assert_eq!(commit["command_action"]["template"]["agent_may_fill"], true);
    let merge = commands
        .iter()
        .find(|entry| entry["display"] == "merge")
        .expect("merge command should be cataloged");
    assert_eq!(
        merge["command_action"]["action"],
        "heddle merge <thread> --preview"
    );
    assert_eq!(merge["command_action"]["argv"], Value::Null);
    assert_eq!(
        merge["command_action"]["template"]["argv_template"],
        heddle_argv_json(["merge", "<thread>", "--preview"])
    );
    let checkpoint = commands
        .iter()
        .find(|entry| entry["display"] == "checkpoint")
        .expect("checkpoint command should be cataloged");
    assert_eq!(checkpoint["surface"], "native");
    assert_eq!(checkpoint["help_visibility"], "advanced");
    assert_eq!(checkpoint["canonical_command"], Value::Null);
    assert_eq!(checkpoint["canonical_action"], Value::Null);
    assert_eq!(checkpoint["command_action"]["action"], "heddle checkpoint");
    assert_eq!(
        checkpoint["command_action"]["argv"],
        heddle_argv_json(["checkpoint"])
    );
    let branch = commands
        .iter()
        .find(|entry| entry["display"] == "branch")
        .expect("branch command should be cataloged");
    assert_eq!(branch["canonical_action"]["command"], "thread");
    assert_eq!(branch["canonical_action"]["kind"], "command_family");
    assert_eq!(branch["canonical_action"]["executable"], false);
    assert_eq!(branch["canonical_action"]["argv"], Value::Null);
    assert_eq!(branch["canonical_action"]["template"], Value::Null);
    let bridge_import = commands
        .iter()
        .find(|entry| entry["display"] == "bridge git import")
        .expect("bridge git import should be cataloged");
    assert_eq!(bridge_import["canonical_action"]["command"], "adopt");
    assert_eq!(bridge_import["canonical_action"]["kind"], "workflow");
    assert_eq!(
        bridge_import["canonical_action"]["template"]["argv_template"],
        heddle_argv_json(["adopt", "--ref", "<branch>"])
    );
    let stash_pop = commands
        .iter()
        .find(|entry| entry["display"] == "stash pop")
        .expect("stash pop command should be cataloged");
    assert_eq!(stash_pop["canonical_action"]["command"], "undo");
    assert_eq!(stash_pop["canonical_action"]["kind"], "conceptual_home");
    assert_eq!(stash_pop["canonical_action"]["executable"], false);
    let stash_push = commands
        .iter()
        .find(|entry| entry["display"] == "stash push")
        .expect("stash push command should be cataloged");
    assert_eq!(stash_push["surface"], "git_adapter");
    assert_eq!(stash_push["help_visibility"], "git_adapter");
    assert_eq!(stash_push["canonical_action"]["command"], "capture");
    assert_eq!(stash_push["canonical_action"]["kind"], "workflow");
    assert_eq!(stash_push["canonical_action"]["executable"], false);
    assert_eq!(
        stash_push["canonical_action"]["template"]["argv_template"],
        heddle_argv_json(["capture", "-m", "<message>"])
    );
    assert!(
        commands
            .iter()
            .all(|entry| entry["surface"] != "compatibility"
                && entry["help_visibility"] != "compatibility"),
        "catalog JSON should not leak the old compatibility surface: {json}"
    );
    assert!(
        status["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["long"] == "short" && option["short"] == "s"),
        "status options should include --short/-s: {status}"
    );
    assert!(
        parsed["global_options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["long"] == "output"),
        "catalog should expose global --output: {json}"
    );
    assert!(
        parsed["global_options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|option| option["long"] != "op-id" && option["id"] != "op_id"),
        "catalog should not imply --op-id is accepted by every command; use per-command op_id_behavior: {json}"
    );
    let commit = commands
        .iter()
        .find(|entry| entry["display"] == "commit")
        .expect("commit should be cataloged");
    assert!(
        commit["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["long"] == "op-id" && option["global"] == true),
        "op-id capable commands should expose --op-id as a per-command option: {commit}"
    );
    let status = commands
        .iter()
        .find(|entry| entry["display"] == "status")
        .expect("status should be cataloged");
    assert!(
        status["options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|option| option["long"] != "op-id"),
        "observe-only commands should not expose --op-id as accepted: {status}"
    );
    let commands_entry = commands
        .iter()
        .find(|entry| entry["display"] == "commands")
        .expect("commands should be cataloged");
    for option in ["command", "tier", "mutating", "supports-op-id"] {
        assert!(
            commands_entry["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|catalog_option| catalog_option["long"] == option),
            "commands catalog entry should advertise --{option}: {commands_entry}"
        );
    }

    let text = heddle(&["commands", "--output", "text"], None)
        .expect("command catalog text should succeed");
    assert!(
        text.contains("Command catalog")
            && text.contains("Native loop:")
            && text.contains("Power surfaces:")
            && text.contains("Git interop:")
            && text.contains("Automation and admin:")
            && text.contains("commands"),
        "command catalog text should be scannable: {text}"
    );
    assert!(
        !text.contains("compatibility"),
        "command catalog text should not leak the old compatibility wording: {text}"
    );
    assert!(
        !text.contains("Automation/workflow command:"),
        "command catalog text should share the cleaned help summaries: {text}"
    );
}

#[test]
fn command_catalog_filters_bound_agent_queries() {
    let merge_json = heddle(
        &["commands", "--output", "json", "--command", "merge"],
        None,
    )
    .expect("filtered command catalog JSON should succeed");
    let merge_catalog: Value = serde_json::from_str(&merge_json)
        .unwrap_or_else(|_| panic!("filtered command catalog JSON should parse: {merge_json}"));
    let merge_commands = merge_catalog["commands"]
        .as_array()
        .expect("commands should be an array");
    assert_eq!(
        merge_commands.len(),
        1,
        "merge filter should be exact: {merge_json}"
    );
    assert_eq!(merge_commands[0]["display"], "merge");

    let thread_json = heddle(
        &["commands", "--output", "json", "--command", "thread"],
        None,
    )
    .expect("thread family command catalog JSON should succeed");
    let thread_catalog: Value = serde_json::from_str(&thread_json)
        .unwrap_or_else(|_| panic!("thread catalog JSON should parse: {thread_json}"));
    let thread_commands = thread_catalog["commands"]
        .as_array()
        .expect("commands should be an array");
    assert!(
        thread_commands.len() > 1
            && thread_commands.iter().all(|command| command["path"]
                .as_array()
                .is_some_and(|path| { path.first().is_some_and(|part| part == "thread") })),
        "command prefix filter should include only the requested family: {thread_json}"
    );

    let everyday_json = heddle(
        &["commands", "--output", "json", "--tier", "everyday"],
        None,
    )
    .expect("tier-filtered command catalog JSON should succeed");
    let everyday_catalog: Value = serde_json::from_str(&everyday_json)
        .unwrap_or_else(|_| panic!("everyday catalog JSON should parse: {everyday_json}"));
    let everyday_commands = everyday_catalog["commands"]
        .as_array()
        .expect("commands should be an array");
    assert!(
        everyday_commands
            .iter()
            .all(|command| command["tier"] == "everyday"),
        "tier filter should exclude non-everyday commands: {everyday_json}"
    );
    assert!(
        everyday_commands
            .iter()
            .any(|command| command["display"] == "status"),
        "tier filter should still include everyday commands: {everyday_json}"
    );

    let replay_json = heddle(
        &[
            "commands",
            "--output",
            "json",
            "--mutating",
            "--supports-op-id",
        ],
        None,
    )
    .expect("side-effect/op-id-filtered command catalog JSON should succeed");
    let replay_catalog: Value = serde_json::from_str(&replay_json)
        .unwrap_or_else(|_| panic!("replay catalog JSON should parse: {replay_json}"));
    let replay_commands = replay_catalog["commands"]
        .as_array()
        .expect("commands should be an array");
    assert!(
        replay_commands
            .iter()
            .all(|command| command["mutates"] == true && command["supports_op_id"] == true),
        "mutating/op-id filters should only include replay-safe mutating commands: {replay_json}"
    );
    assert!(
        replay_commands
            .iter()
            .any(|command| command["display"] == "commit")
            && replay_commands
                .iter()
                .all(|command| command["display"] != "status"),
        "mutating/op-id filters should include commit and exclude observe-only status: {replay_json}"
    );
}

#[test]
fn git_dependencies_help_topic_explains_no_git_contract() {
    let help = heddle(&["help", "git-dependencies"], None)
        .expect("git-dependencies help topic should render");
    assert!(
        help.contains("without `git` on PATH")
            && help.contains("Git-compatible, not Git-binary-dependent")
            && help.contains("must not spawn a `git` process")
            && help.contains("tool that started it")
            && help.contains("Unsupported native Git-overlay capabilities")
            && help.contains("merge --git-commit")
            && help.contains("heddle commands --output json"),
        "git-dependencies topic should explain supported paths and zero-git runtime behavior: {help}"
    );
}

#[test]
fn remotes_help_topic_is_available_from_default_topic_list() {
    let help = heddle(&["help", "remotes"], None).expect("remotes help topic should render");
    assert!(
        help.contains("heddle remote add origin <url-or-path>")
            && help.contains("heddle push")
            && help.contains("heddle verify"),
        "remotes topic should explain the remote loop: {help}"
    );
}

#[test]
fn unknown_flag_alone_still_routes_to_clap_error() {
    // The intercept must NOT swallow real parse errors — typing
    // `heddle --invalid-flag` should still surface the clap error so the
    // typo is obvious.
    let output = heddle_output(&["--invalid-flag"], None).expect("invoke heddle");
    assert!(
        !output.status.success(),
        "unknown flag should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--invalid-flag"),
        "clap should name the offending flag: stderr={stderr}"
    );
}

#[test]
fn start_emits_cd_hint_in_text_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "start", "scratch-thread"],
        Some(temp.path()),
    )
    .expect("start scratch-thread");
    assert!(
        output.contains("Path:"),
        "text-mode start should print the checkout path: {output}"
    );
    assert!(
        output.contains("Heddle-managed checkout") && output.contains("no .git directory"),
        "text-mode start should make isolated checkouts explicit for Git users: {output}"
    );
    assert!(
        output.contains("Run this to switch shells:"),
        "text-mode start should suggest the cd command: {output}"
    );
    assert!(
        output.contains("    cd "),
        "the cd hint should include the literal `cd` invocation: {output}"
    );
}

#[test]
fn cd_hint_quotes_paths_with_spaces() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let checkout = sibling_checkout_path(temp.path(), "scratch dir");
    let checkout_str = checkout.to_str().expect("utf-8 path");
    let output = heddle(
        &[
            "--output",
            "text",
            "start",
            "spaced-thread",
            "--path",
            checkout_str,
        ],
        Some(temp.path()),
    )
    .expect("start with spaced path");

    let quoted = format!("'{checkout_str}'");
    assert!(
        output.contains(&format!("    cd {quoted}")),
        "cd hint must single-quote paths with spaces: {output}"
    );
}

#[test]
fn start_absolute_parent_path_is_normalized_in_text_and_cd_output() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let path_name = format!(
        "{}-normalized-thread",
        temp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
    );
    let thread_name = "normalized-thread";
    let explicit = temp.path().join("..").join(&path_name);
    let explicit_str = explicit.to_str().expect("utf-8 path");
    let output = heddle(
        &[
            "--output",
            "text",
            "start",
            thread_name,
            "--path",
            explicit_str,
        ],
        Some(temp.path()),
    )
    .expect("start with absolute parent path");
    assert!(
        !output.contains("/../"),
        "text start output should report normalized paths: {output}"
    );
    assert!(
        output.contains("Heddle-managed checkout") && output.contains("no .git directory"),
        "text start output should disclose that isolated checkouts are Heddle-managed: {output}"
    );

    let cd_thread_name = "normalized-thread-cd";
    let cd_path_name = format!("{path_name}-cd");
    let cd_explicit = temp.path().join("..").join(&cd_path_name);
    let cd_explicit_str = cd_explicit.to_str().expect("utf-8 path");
    let print_cd = heddle_output(
        &[
            "start",
            cd_thread_name,
            "--path",
            cd_explicit_str,
            "--print-cd-path",
        ],
        Some(temp.path()),
    )
    .expect("start --print-cd-path");
    assert!(print_cd.status.success(), "print-cd-path should succeed");
    let stdout = std::str::from_utf8(&print_cd.stdout).unwrap();
    assert!(
        !stdout.contains("/../"),
        "--print-cd-path should report the same normalized path: {stdout:?}"
    );

    for checkout in [
        temp.path().parent().unwrap().join(&path_name),
        temp.path().parent().unwrap().join(&cd_path_name),
    ] {
        if checkout.exists() {
            std::fs::remove_dir_all(checkout).unwrap();
        }
    }
}

#[test]
fn start_print_cd_path_returns_only_the_path() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle_output(
        &["start", "scratch-cd", "--print-cd-path"],
        Some(temp.path()),
    )
    .expect("start --print-cd-path");
    assert!(
        output.status.success(),
        "start --print-cd-path should succeed"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let trimmed = stdout.trim();
    assert!(
        trimmed.contains("scratch-cd"),
        "stdout should be a path referencing the new thread name: {stdout:?}"
    );
    // Pure-path output: no embedded JSON, no labels, no extra prose.
    assert!(
        !trimmed.contains('{'),
        "stdout must not contain JSON when --print-cd-path is set: {stdout:?}"
    );
    assert!(
        !trimmed.contains("Path:"),
        "stdout must not contain the human label when --print-cd-path is set: {stdout:?}"
    );
    assert_eq!(
        trimmed.lines().count(),
        1,
        "stdout should be a single line: {stdout:?}"
    );
}

#[test]
fn unknown_state_id_hints_at_heddle_log_across_state_readers() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for args in [
        vec!["--output", "text", "goto", "hd-nonexistent"],
        vec!["--output", "text", "show", "hd-nonexistent"],
        vec!["--output", "text", "diff", "hd-nonexistent", "HEAD"],
    ] {
        let output = heddle_output(&args, Some(temp.path()))
            .unwrap_or_else(|err| panic!("invoke heddle {args:?}: {err}"));
        assert!(
            !output.status.success(),
            "missing state should exit non-zero for {args:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "missing-state failures should not write primary output for {args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(
            stderr.contains("State not found"),
            "stderr should carry the original error for {args:?}: {stderr}"
        );
        assert!(
            stderr.contains("Next: heddle log"),
            "stderr should suggest `heddle log` for {args:?}: {stderr}"
        );
    }
}

#[test]
fn unknown_thread_hints_at_heddle_thread_list() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "text", "thread", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke heddle thread show");
    assert!(
        !output.status.success(),
        "thread show on a missing thread should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("Thread 'missing' not found"),
        "stderr should carry the original error: {stderr}"
    );
    assert!(
        stderr.contains("Next: heddle thread list"),
        "stderr should suggest `heddle thread list`: {stderr}"
    );

    let json = heddle_output(
        &["--output", "json", "thread", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke heddle thread show json");
    assert!(
        !json.status.success(),
        "thread show on a missing thread should exit non-zero"
    );
    assert!(
        json.stdout.is_empty(),
        "JSON-mode missing thread show refusal must keep stdout quiet"
    );
    let stderr = std::str::from_utf8(&json.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing thread show should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Thread 'missing' not found")),
        "missing thread show should include typed recovery detail: {stderr}"
    );
}

#[test]
fn merge_missing_thread_uses_thread_list_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["merge", "missing", "--preview", "--output", "json"],
        Some(temp.path()),
    )
    .expect("invoke missing merge source");
    assert!(
        !output.status.success(),
        "merge on a missing thread should exit non-zero"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing merge source refusal must keep stdout quiet"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing merge source should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_not_found");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["thread", "list"]),
        "missing merge source should recover through thread discovery: {envelope}"
    );
}

#[test]
fn help_for_verb_prefixes_usage_with_heddle() {
    // `heddle help status` falls through to status's clap-derived help.
    // The Usage line MUST start with `Usage: heddle status` — saying just
    // `Usage: status` would suggest the user can run `status` standalone.
    for verb in ["status", "capture", "log", "merge", "undo", "start", "init"] {
        let output =
            heddle(&["help", verb], None).unwrap_or_else(|err| panic!("heddle help {verb}: {err}"));
        assert!(
            output.contains(&format!("Usage: heddle {verb}")),
            "`heddle help {verb}` must prefix the Usage line with `heddle`: {output}"
        );
    }
}

#[test]
fn help_for_verb_includes_visible_global_flags() {
    let topic = heddle(&["help", "status"], None).expect("heddle help status should render");
    let direct = heddle(&["status", "--help"], None).expect("heddle status --help should render");
    for flag in ["--output <OUTPUT>", "--repo <PATH>", "--quiet", "--verbose"] {
        assert!(
            topic.contains(flag),
            "`heddle help status` should include global flag `{flag}`: {topic}"
        );
        assert!(
            direct.contains(flag),
            "`heddle status --help` should include global flag `{flag}`: {direct}"
        );
    }
}

#[test]
fn op_id_help_is_visible_only_for_supported_commands() {
    let commit = heddle(&["commit", "--help"], None).expect("heddle commit --help should render");
    assert!(
        commit.contains("--op-id <UUID>"),
        "op-id capable command help should expose --op-id: {commit}"
    );
    assert!(
        commit.contains("with nothing staged it commits all worktree paths")
            && commit.contains("with staged paths it commits only the index")
            && commit.contains("--all"),
        "commit help should explain all-worktree and staged-index semantics for Git users: {commit}"
    );

    let init = heddle(&["help", "init"], None).expect("heddle help init should render");
    assert!(
        init.contains("--op-id <UUID>"),
        "first-contact mutator help should expose --op-id: {init}"
    );

    let status = heddle(&["status", "--help"], None).expect("heddle status --help should render");
    assert!(
        !status.contains("--op-id"),
        "observe-only command help should not advertise --op-id: {status}"
    );
}

#[test]
fn public_command_paths_have_all_required_help_entrypoints() {
    let paths = public_command_paths();
    assert!(
        paths.len() > 40,
        "public help coverage should enumerate the real command tree, got {paths:?}"
    );

    for path in paths {
        let display = path.join(" ");

        let mut help_args: Vec<&str> = Vec::with_capacity(path.len() + 1);
        help_args.push("help");
        help_args.extend(path.iter().map(String::as_str));
        let output = heddle_output(&help_args, None)
            .unwrap_or_else(|err| panic!("heddle help {display} should run: {err}"));
        assert!(
            output.status.success(),
            "heddle help {display} should exit 0: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "heddle help {display} must write help to stdout only: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.trim().is_empty() && !stdout.contains("no topic or command"),
            "heddle help {display} should render useful command help: {stdout}"
        );

        for flag in ["--help", "-h"] {
            let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
            args.push(flag);
            let output = heddle_output(&args, None)
                .unwrap_or_else(|err| panic!("heddle {display} {flag} should run: {err}"));
            assert!(
                output.status.success(),
                "heddle {display} {flag} should exit 0: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "heddle {display} {flag} must write help to stdout only: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Usage:") && stdout.contains("heddle"),
                "heddle {display} {flag} should render command usage: {stdout}"
            );
        }
    }
}

#[test]
fn public_command_paths_have_command_contract_metadata() {
    let catalog = cli::cli::commands::build_command_catalog();
    let catalog_paths = catalog
        .commands
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<std::collections::BTreeSet<_>>();

    for path in public_command_paths() {
        assert!(
            catalog_paths.contains(&path),
            "public command `{}` must have command contract metadata",
            path.join(" ")
        );
    }
}

fn public_command_paths() -> Vec<Vec<String>> {
    fn walk(command: &clap::Command, prefix: &mut Vec<String>, paths: &mut Vec<Vec<String>>) {
        for subcommand in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
            prefix.push(subcommand.get_name().to_string());
            paths.push(prefix.clone());
            walk(subcommand, prefix, paths);
            prefix.pop();
        }
    }

    let command = Cli::command();
    let mut paths = Vec::new();
    walk(&command, &mut Vec::new(), &mut paths);
    paths
}

#[test]
fn everyday_commands_have_all_required_help_entrypoints() {
    let everyday = cli::cli::commands::root_commands_for_help_visibility("everyday");
    let everyday_set = everyday
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();

    for verb in [
        "status", "diff", "commit", "start", "ready", "merge", "ship", "undo", "verify", "doctor",
    ] {
        assert!(
            everyday_set.contains(verb),
            "`{verb}` must remain an everyday front-door command"
        );
    }

    for verb in everyday {
        let topic = heddle(&["help", verb], None)
            .unwrap_or_else(|err| panic!("heddle help {verb} should succeed: {err}"));
        assert!(
            !topic.trim().is_empty() && !topic.contains("no topic"),
            "heddle help {verb} should render useful help: {topic}"
        );

        for flag in ["--help", "-h"] {
            let output = heddle(&[verb, flag], None)
                .unwrap_or_else(|err| panic!("heddle {verb} {flag} should succeed: {err}"));
            assert!(
                output.contains("Usage:") && output.contains("heddle") && output.contains(verb),
                "heddle {verb} {flag} should render command help with usage: {output}"
            );
        }
    }
}

#[test]
fn context_get_honors_user_config_principal_not_unknown() {
    // Regression: `heddle context set` / `context get` used to route through
    // `repo.get_attribution()`, which only consults env + repo config.
    // A user with `[principal]` only in `~/.config/heddle/config.toml` saw
    // every annotation surface as `Unknown <unknown@example.com>`. After
    // the migration to `resolve_attribution`, the user-config principal
    // wins as it does for `heddle capture`.
    let temp = TempDir::new().unwrap();
    let user_cfg = temp.path().with_extension("ada-user-config.toml");
    std::fs::write(
        &user_cfg,
        "[principal]\nname = \"Ada\"\nemail = \"ada@example.com\"\n",
    )
    .unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    heddle_output_with_env(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", user_cfg.to_str().unwrap())],
    )
    .unwrap();
    heddle_output_with_env(
        &[
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "file",
            "--kind",
            "rationale",
            "-m",
            "entry point",
        ],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", user_cfg.to_str().unwrap())],
    )
    .unwrap();

    let context = heddle_output_with_env(
        &["--output", "text", "context", "get", "--path", "main.rs"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", user_cfg.to_str().unwrap())],
    )
    .expect("context get");
    assert!(
        context.status.success(),
        "context get should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&context.stdout),
        String::from_utf8_lossy(&context.stderr)
    );
    let output = String::from_utf8_lossy(&context.stdout);
    assert!(
        output.contains("by: Ada <ada@example.com>"),
        "context get should attribute the annotation to the user-config principal: {output}"
    );
    assert!(
        !output.contains("Unknown <unknown@example.com>"),
        "context get must not fall back to Unknown when user config has a principal: {output}"
    );
}

#[test]
fn context_invalid_scope_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "symbol:",
            "-m",
            "empty symbol",
        ],
        Some(temp.path()),
    )
    .expect("invoke invalid context scope");
    assert!(
        !output.status.success(),
        "invalid context scope should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode context scope refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("context scope refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "context_symbol_name_required");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Symbol name must not be empty")),
        "context scope refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("symbol:<name>")),
        "context scope hint should explain the valid symbol form: {stderr}"
    );
}

#[test]
fn discuss_resolve_conditional_options_use_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for (args, expected_kind, expected_error, expected_hint) in [
        (
            vec![
                "--output",
                "json",
                "discuss",
                "resolve",
                "d1",
                "--mode",
                "into-annotation",
            ],
            "discuss_resolve_missing_annotation_kind",
            "--annotation-kind is required for into-annotation",
            "--annotation-kind",
        ),
        (
            vec![
                "--output",
                "json",
                "discuss",
                "resolve",
                "d1",
                "--mode",
                "into-annotation",
                "--annotation-kind",
                "rationale",
            ],
            "discuss_resolve_missing_annotation_content",
            "--annotation-content is required for into-annotation",
            "--annotation-content",
        ),
        (
            vec![
                "--output", "json", "discuss", "resolve", "d1", "--mode", "dismiss",
            ],
            "discuss_resolve_missing_dismiss_reason",
            "--reason is required for dismiss",
            "--reason",
        ),
    ] {
        let output = heddle_output(&args, Some(temp.path())).expect("invoke discuss resolve");
        assert!(
            !output.status.success(),
            "conditional discuss resolve option should fail"
        );
        assert!(
            output.stdout.is_empty(),
            "JSON-mode discuss refusal must keep stdout quiet: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("discuss refusal should emit JSON envelope");
        assert_eq!(envelope["kind"], expected_kind);
        assert_json_recovery_advice_fields(&envelope, stderr);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(expected_error)),
            "discuss refusal should keep the centralized error: {stderr}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains(expected_hint)),
            "discuss refusal hint should name the missing flag: {stderr}"
        );
    }
}

#[test]
fn review_sign_malformed_symbols_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "review",
            "sign",
            "HEAD",
            "--kind",
            "read",
            "--symbols",
            "main.rs",
            "--public-key",
            "00",
            "--signature",
            "00",
            "--signed-at-unix",
            "0",
        ],
        Some(temp.path()),
    )
    .expect("invoke review sign");
    assert!(!output.status.success(), "malformed symbol should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode review refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("review refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "review_symbols_malformed");
    assert_json_recovery_advice_fields(&envelope, stderr);
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("--symbols expects 'file:symbol', got 'main.rs'")),
        "review refusal should keep the centralized error: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--symbols")),
        "review refusal hint should name the valid flag form: {stderr}"
    );
}

#[test]
fn thread_absorb_missing_parent_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "orphan"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "thread", "absorb", "orphan"],
        Some(temp.path()),
    )
    .expect("invoke thread absorb");
    assert!(
        !output.status.success(),
        "absorb without a parent should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode absorb refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("absorb refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_absorb_parent_required");
    assert_json_recovery_advice_fields(&envelope, stderr);
    assert!(
        envelope["error"].as_str().is_some_and(
            |error| error.contains("Thread 'orphan' has no recorded parent; pass --into")
        ),
        "absorb refusal should keep the centralized error: {stderr}"
    );
    assert!(
        envelope["primary_command"]
            .as_str()
            .is_some_and(|command| command == "heddle thread absorb orphan --into <parent-thread>"),
        "absorb refusal should name the exact retry command: {stderr}"
    );
}

#[test]
fn integration_invalid_harness_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "integration",
            "install",
            "unknown-harness",
        ],
        Some(temp.path()),
    )
    .expect("invoke integration install");
    assert!(!output.status.success(), "unsupported harness must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "integration_harness_unsupported");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("codex")
                && hint.contains("claude-code")
                && hint.contains("opencode")),
        "typed advice should name supported harnesses: {stderr}"
    );
}

#[test]
fn integration_codex_repo_scope_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "integration",
            "install",
            "codex",
            "--scope",
            "repo",
        ],
        Some(temp.path()),
    )
    .expect("invoke integration install");
    assert!(
        !output.status.success(),
        "codex repo-scope install must refuse"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "integration_codex_scope_invalid");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--scope user")),
        "typed advice should name user-scope recovery: {stderr}"
    );
}

#[test]
fn agent_serve_background_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(&["--output", "json", "agent", "serve"], Some(temp.path()))
        .expect("invoke agent serve");
    assert!(
        !output.status.success(),
        "background agent serve must refuse"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_background_unimplemented");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("agent serve --foreground")),
        "typed advice should name foreground recovery: {stderr}"
    );
}

#[test]
fn agent_stop_invalid_pidfile_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let sockets = temp.path().join(".heddle/sockets");
    std::fs::create_dir_all(&sockets).expect("create sockets dir");
    std::fs::write(sockets.join("grpc.pid"), "not-a-heddle-pidfile\n").expect("write pidfile");

    let output = heddle_output(&["--output", "json", "agent", "stop"], Some(temp.path()))
        .expect("invoke agent stop");
    assert!(!output.status.success(), "invalid pidfile must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_pidfile_invalid");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("pidfile")),
        "typed advice should explain pidfile recovery: {stderr}"
    );
}

#[test]
fn agent_heartbeat_missing_session_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "heartbeat",
            "--session",
            "missing-session",
        ],
        Some(temp.path()),
    )
    .expect("invoke agent heartbeat");
    assert!(!output.status.success(), "missing session must refuse");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode refusal must not write stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value =
        serde_json::from_str(&stderr).expect("stderr should be JSON error envelope");
    assert_eq!(envelope["kind"], "agent_session_not_found");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("Reserve the thread again")),
        "typed advice should name reservation recovery: {stderr}"
    );
}

#[test]
fn agent_api_json_outputs_match_registered_schemas_and_include_verification() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    let status = json_value(temp.path(), &["agent", "status", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["agent", "status"], &status);
    assert_eq!(status["output_kind"], "agent_status");
    assert!(
        status["verification"].is_object(),
        "agent status should report repository verify: {status}"
    );
    assert!(
        status["pid_path"]
            .as_str()
            .is_some_and(|path| path.starts_with(temp.path().to_str().unwrap())),
        "agent status should use the requested repo for daemon paths: {status}"
    );

    let stop = json_value(temp.path(), &["agent", "stop", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["agent", "stop"], &stop);
    assert_eq!(stop["output_kind"], "agent_stop");
    assert_eq!(stop["stopped"], false);
    assert!(
        stop["verification"].is_object(),
        "agent stop success should report repository verify: {stop}"
    );

    let reserve = json_value(
        temp.path(),
        &[
            "agent",
            "reserve",
            "--thread",
            "main",
            "--task",
            "agent schema flow",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["agent", "reserve"], &reserve);
    assert!(
        reserve["reservation"]["session_id"].as_str().is_some(),
        "agent reserve should emit a reservation envelope: {reserve}"
    );
    assert!(
        reserve["verification"].is_object(),
        "agent reserve should prove post-mutation verify: {reserve}"
    );
    let session = reserve["reservation"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    std::fs::write(temp.path().join("agent.txt"), "agent work\n").expect("write work");
    let capture = json_value(
        temp.path(),
        &[
            "agent",
            "capture",
            "--session",
            &session,
            "-m",
            "agent capture",
            "--confidence",
            "0.8",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["agent", "capture"], &capture);
    assert_eq!(capture["status"], "captured");
    assert!(
        capture["verification"].is_object(),
        "agent capture should reuse the capture verify contract: {capture}"
    );

    let ready = json_value(
        temp.path(),
        &["agent", "ready", "--session", &session, "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["agent", "ready"], &ready);
    assert!(
        ready["verification"].is_object(),
        "agent ready should reuse the ready verify contract: {ready}"
    );

    let heartbeat = json_value(
        temp.path(),
        &[
            "agent",
            "heartbeat",
            "--session",
            &session,
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["agent", "heartbeat"], &heartbeat);
    assert_eq!(heartbeat["reservation"]["status"], "active");
    assert!(
        heartbeat["verification"].is_object(),
        "agent heartbeat should prove post-mutation verify: {heartbeat}"
    );

    let list = json_value(temp.path(), &["agent", "list", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["agent", "list"], &list);
    assert!(
        list["reservations"]
            .as_array()
            .is_some_and(|reservations| !reservations.is_empty()),
        "agent list should be an enveloped collection: {list}"
    );
    assert!(
        list["verification"].is_object(),
        "agent list should report repository verify: {list}"
    );

    let release = json_value(
        temp.path(),
        &[
            "agent",
            "release",
            "--session",
            &session,
            "--status",
            "complete",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["agent", "release"], &release);
    assert_eq!(release["reservation"]["status"], "complete");
    assert!(
        release["verification"].is_object(),
        "agent release should prove post-mutation verify: {release}"
    );
}

#[test]
fn agent_daemon_status_honors_global_repo_argument() {
    let cwd_repo = TempDir::new().unwrap();
    let target_repo = TempDir::new().unwrap();
    heddle(&["init"], Some(cwd_repo.path())).expect("init cwd repo");
    heddle(&["init"], Some(target_repo.path())).expect("init target repo");

    let target_arg = target_repo.path().to_str().expect("utf8 target path");
    let output = heddle(
        &["--repo", target_arg, "agent", "status", "--output", "json"],
        Some(cwd_repo.path()),
    )
    .expect("agent status with --repo should run");
    let status: Value = serde_json::from_str(&output).expect("agent status JSON should parse");
    assert_eq!(status["output_kind"], "agent_status");
    let pid_path = status["pid_path"]
        .as_str()
        .expect("agent status should include pid_path");
    assert!(
        pid_path.starts_with(target_arg),
        "agent status must inspect the global --repo target, not cwd: {status}"
    );
    assert!(
        status["verification"]["repository_mode"]
            .as_str()
            .is_some_and(|mode| mode == "native-heddle"),
        "agent status should verify the target repo: {status}"
    );
}

#[test]
fn agent_reserve_reports_path_for_existing_materialized_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    std::fs::write(temp.path().join("app.txt"), "base\n").expect("write base file");
    heddle(&["commit", "--all", "-m", "base"], Some(temp.path())).expect("commit base");

    let thread_path = sibling_checkout_path(temp.path(), "agent-materialized");
    let thread_path_arg = thread_path.to_str().expect("utf8 thread path");
    let started = json_value(
        temp.path(),
        &[
            "start",
            "agent-materialized",
            "--path",
            thread_path_arg,
            "--output",
            "json",
        ],
    );
    let execution_path = started["execution_path"]
        .as_str()
        .expect("start --path should report execution_path")
        .to_string();

    let reserve = json_value(
        temp.path(),
        &[
            "agent",
            "reserve",
            "--thread",
            "agent-materialized",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        reserve["reservation"]["path"].as_str(),
        Some(execution_path.as_str()),
        "agent reserve should return the existing materialized thread execution path: {reserve}"
    );
}

#[test]
fn index_json_emits_one_value_even_for_hidden_compat_alias() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let index = json_value(temp.path(), &["index", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["index"], &index);
    assert_schema_declares_runtime_top_level(&["maintenance", "index"], &index);
    assert_eq!(index["output_kind"], "index");
    assert!(
        index["present"].as_bool().is_some(),
        "index JSON should report presence: {index}"
    );
    assert!(
        index["file_entries"].as_u64().is_some(),
        "index JSON should include file entry count: {index}"
    );

    let dump = json_value(temp.path(), &["index", "--dump", "--output", "json"]);
    assert_eq!(dump["output_kind"], "index");
    assert!(
        dump["dump"]
            .as_str()
            .is_some_and(|value| value.contains("WorktreeIndex")),
        "index --dump JSON should carry dump text inside JSON, not stdout prose: {dump}"
    );
}

#[test]
fn default_output_is_text_and_json_requires_explicit_flag() {
    // Persona feedback (heddle#???): the old `--output auto` mode
    // emitted JSON whenever stdout wasn't a TTY (pipes, subprocesses,
    // `| less`). That surprised every interactive user. The contract
    // is now: default = text, `--output json` for JSON, no
    // auto-switching.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending").unwrap();

    let default = heddle_output(&["status"], Some(temp.path())).expect("invoke default status");
    assert!(default.status.success(), "default status should succeed");
    let default_stdout = String::from_utf8_lossy(&default.stdout);
    assert!(
        default_stdout.contains("Heddle status"),
        "default status should render text, not JSON: {default_stdout}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&default_stdout).is_err(),
        "default status must not be JSON-parseable (would prove the old auto-mode regressed): {default_stdout}"
    );

    let json = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke explicit-json status");
    assert!(json.status.success(), "explicit-json status should succeed");
    let json_stdout = String::from_utf8_lossy(&json.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|_| panic!("--output json should emit JSON: {json_stdout}"));
    assert_eq!(parsed["thread_health"], "uncaptured");
    assert_eq!(parsed["changed_path_count"], 1);
    assert_eq!(parsed["changes"]["added"].as_array().map(Vec::len), Some(1));
}

#[test]
fn daemon_status_json_matches_command_catalog_when_absent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["daemon", "status", "--output", "json"], Some(temp.path()))
        .expect("invoke daemon status");
    assert!(
        output.status.success(),
        "daemon status should be a successful probe even when absent; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "successful JSON daemon status should keep stderr quiet: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("daemon status JSON should parse: {err}: {stdout}"));
    assert_eq!(parsed["status"], "not_running");
    assert_eq!(parsed["running"], false);
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["mount_count"], 0);
    assert_eq!(parsed["materialized_count"], 0);
    assert!(parsed["endpoint_path"].as_str().is_some());
    assert!(parsed["materialized_threads"].as_array().is_some());
}

#[test]
fn actor_explain_json_detects_harness_without_active_actor() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_env(
        &["actor", "explain", "--output", "json"],
        Some(temp.path()),
        &[
            ("CODEX_THREAD_ID", "thread-cold-agent"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
            ("HEDDLE_PRINCIPAL_NAME", "Cold Agent"),
            ("HEDDLE_PRINCIPAL_EMAIL", "agent@example.com"),
        ],
    )
    .expect("invoke actor explain");
    assert!(
        output.status.success(),
        "actor explain should be a successful identity probe without an active actor; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "successful JSON actor explain should keep stderr quiet: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("actor explain JSON should parse: {err}: {stdout}"));
    assert_eq!(parsed["attached"], false);
    assert!(parsed.get("active_actor").is_none());
    assert_eq!(parsed["detected"]["harness"], "codex");
    assert_eq!(parsed["detected"]["provider"], "openai");
    assert_eq!(parsed["detected"]["model"], "gpt-5.3-codex");
    assert_eq!(parsed["detected"]["thinking_level"], "high");
    assert!(parsed["detected"].get("policy").is_none());
    assert!(parsed["detected"].get("native_parent_actor_key").is_none());
    assert!(parsed["detected"].get("native_instance_key").is_none());
    assert!(parsed["environment"].get("agent_provider").is_none());
    assert!(parsed["environment"].get("agent_model").is_none());
    assert!(parsed["environment"].get("agent_policy").is_none());
    assert_eq!(parsed["environment"]["principal_name"], "Cold Agent");
    assert_eq!(
        parsed["environment"]["principal_email"],
        "agent@example.com"
    );
    assert!(
        parsed["environment"]["signals"]
            .as_array()
            .expect("signals should be array")
            .iter()
            .any(|signal| signal == "CODEX_THREAD_ID"),
        "actor explain should name detected signal keys without leaking unrelated values: {parsed}"
    );
    // On-thread context (fresh `init` leaves HEAD attached to a thread):
    // `--no-thread` attaches the detected identity to the current thread.
    assert_eq!(
        parsed["recommended_action"],
        "heddle actor spawn --no-thread --provider openai --model gpt-5.3-codex"
    );
    assert_eq!(
        parsed["recommended_action_template"]["argv_template"],
        heddle_argv_json([
            "actor",
            "spawn",
            "--no-thread",
            "--provider",
            "openai",
            "--model",
            "gpt-5.3-codex"
        ]),
        "actor explain should expose replayable argv for the detected spawn action: {parsed}"
    );
    assert_schema_declares_runtime_top_level(&["actor", "explain"], &parsed);
    assert!(
        parsed.get("verification").is_some(),
        "actor explain should prove repository verify for agents: {parsed}"
    );
}

#[test]
fn actor_explain_detached_head_recommends_minting_spawn_not_no_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    let base = Repository::open(temp.path())
        .unwrap()
        .current_state()
        .unwrap()
        .unwrap()
        .change_id
        .to_string();
    std::fs::write(temp.path().join("tracked.txt"), "next\n").unwrap();
    heddle(&["capture", "-m", "next"], Some(temp.path())).unwrap();
    // `goto` to an earlier state detaches HEAD — there is no current thread
    // to attach an actor to, so `--no-thread` would fail.
    heddle(&["goto", &base, "--force"], Some(temp.path())).unwrap();
    assert!(
        matches!(
            Repository::open(temp.path()).unwrap().head_ref().unwrap(),
            refs::Head::Detached { .. }
        ),
        "goto should leave HEAD detached for this test"
    );

    let output = heddle_output_with_env(
        &["actor", "explain", "--output", "json"],
        Some(temp.path()),
        &[
            ("CODEX_THREAD_ID", "thread-cold-agent"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
        ],
    )
    .expect("invoke actor explain");
    assert!(
        output.status.success(),
        "actor explain should succeed on detached HEAD; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("actor explain JSON should parse: {err}: {stdout}"));
    assert_eq!(parsed["attached"], false);
    // Detached HEAD: recommend the minting form (mints a dedicated thread),
    // NOT `--no-thread`, which cannot succeed without a current thread.
    assert_eq!(
        parsed["recommended_action"], "heddle actor spawn --provider openai --model gpt-5.3-codex",
        "detached HEAD should recommend the thread-minting spawn form: {parsed}"
    );
    assert!(
        !parsed["recommended_action"]
            .as_str()
            .expect("recommended_action should be a string")
            .contains("--no-thread"),
        "detached HEAD must not recommend `--no-thread`: {parsed}"
    );
    assert_eq!(
        parsed["recommended_action_template"]["argv_template"],
        heddle_argv_json([
            "actor",
            "spawn",
            "--provider",
            "openai",
            "--model",
            "gpt-5.3-codex"
        ]),
        "actor explain should expose replayable argv for the minting spawn action: {parsed}"
    );
}

#[test]
fn actor_and_session_json_outputs_match_registered_schemas() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let actor_list = json_value(temp.path(), &["actor", "list", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["actor", "list"], &actor_list);
    assert!(
        actor_list["actors"].as_array().is_some(),
        "actor list should emit an envelope with an actors array: {actor_list}"
    );
    assert!(actor_list.get("verification").is_some());

    let actor_spawn = json_value(
        temp.path(),
        &[
            "actor",
            "spawn",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["actor", "spawn"], &actor_spawn);
    assert!(actor_spawn.get("actor").is_some());
    assert!(actor_spawn.get("verification").is_some());
    assert!(actor_spawn["actor"].get("native_actor_key").is_none());
    assert!(actor_spawn["actor"].get("heddle_session_id").is_none());
    assert!(actor_spawn["actor"].get("probe_source").is_some());
    let actor_session = actor_spawn["actor"]["session_id"]
        .as_str()
        .expect("actor spawn should return session id");
    let actor_list_text = heddle(&["actor", "list", "--output", "text"], Some(temp.path()))
        .expect("actor list text should render");
    assert!(
        actor_list_text.contains("actor: openai/gpt-5")
            && actor_list_text.contains("detected: explicit_payload"),
        "actor list text should surface model provenance, not just a session id: {actor_list_text}"
    );

    let actor_show = json_value(
        temp.path(),
        &["actor", "show", actor_session, "--output", "json"],
    );
    assert_schema_declares_runtime_top_level(&["actor", "show"], &actor_show);
    assert_eq!(actor_show["actor"]["session_id"], actor_session);
    assert!(
        actor_show["actor"]["actor_chain"].as_array().is_some(),
        "actor show JSON should expose the same chain field as spawn/text: {actor_show}"
    );

    let actor_done = json_value(
        temp.path(),
        &[
            "actor",
            "done",
            "--session",
            actor_session,
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["actor", "done"], &actor_done);
    assert_eq!(actor_done["status"], "complete");
    assert!(actor_done.get("verification").is_some());

    let auto_actor_output = heddle_output_with_env(
        &["actor", "spawn", "--output", "json"],
        Some(temp.path()),
        &[
            ("CODEX_THREAD_ID", "thread-auto-spawn"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
        ],
    )
    .expect("auto actor spawn should run");
    assert!(
        auto_actor_output.status.success(),
        "auto actor spawn should succeed: {}",
        String::from_utf8_lossy(&auto_actor_output.stderr)
    );
    let auto_actor: Value =
        serde_json::from_slice(&auto_actor_output.stdout).expect("auto actor spawn JSON");
    assert_eq!(auto_actor["actor"]["harness"], "codex");
    assert_eq!(auto_actor["actor"]["provider"], "openai");
    assert_eq!(auto_actor["actor"]["model"], "gpt-5.3-codex");
    assert_eq!(auto_actor["actor"]["thinking_level"], "high");
    assert_eq!(auto_actor["actor"]["probe_source"], "app_protocol");

    let session_start = json_value(
        temp.path(),
        &[
            "session",
            "start",
            "--provider",
            "openai",
            "--model",
            "gpt-5",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["session", "start"], &session_start);
    assert!(session_start.get("session").is_some());
    assert!(session_start.get("verification").is_some());
    assert!(session_start["session"].get("ended_at").is_none());
    assert!(
        session_start["session"]["segments"][0]
            .get("policy_id")
            .is_none()
    );

    let session_segment = json_value(
        temp.path(),
        &[
            "session",
            "segment",
            "--provider",
            "openai",
            "--model",
            "gpt-5.1",
            "--output",
            "json",
        ],
    );
    assert_schema_declares_runtime_top_level(&["session", "segment"], &session_segment);
    assert!(session_segment.get("segment").is_some());
    assert!(session_segment["segment"].get("policy_id").is_none());

    let session_list = json_value(temp.path(), &["session", "list", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["session", "list"], &session_list);
    assert!(
        session_list["sessions"].as_array().is_some(),
        "session list should emit an envelope with a sessions array: {session_list}"
    );

    let session_show = json_value(temp.path(), &["session", "show", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["session", "show"], &session_show);
    assert!(session_show.get("session").is_some());

    let session_end = json_value(temp.path(), &["session", "end", "--output", "json"]);
    assert_schema_declares_runtime_top_level(&["session", "end"], &session_end);
    assert_eq!(session_end["session"]["active"], false);
}

#[test]
fn verify_and_status_json_tolerate_closed_downstream_pipes() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for command in ["status", "verify"] {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -o pipefail; \"$HEDDLE_BIN\" --output json {command} | head -c 0"
            ))
            .current_dir(temp.path())
            .env("HEDDLE_BIN", env!("CARGO_BIN_EXE_heddle"))
            .env(
                "HEDDLE_CONFIG",
                temp.path().join(".heddle-user/config.toml"),
            )
            .output()
            .expect("pipe probe should run");
        assert!(
            output.status.success(),
            "{command} should treat a closed downstream pipe as success; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn text_surfaces_tolerate_closed_downstream_pipes() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("work.txt"), "pending").unwrap();

    for command in [
        "--output text status",
        "--output text verify",
        "help bridge",
    ] {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "set -o pipefail; \"$HEDDLE_BIN\" {command} | head -c 0"
            ))
            .current_dir(temp.path())
            .env("HEDDLE_BIN", env!("CARGO_BIN_EXE_heddle"))
            .env(
                "HEDDLE_CONFIG",
                temp.path().join(".heddle-user/config.toml"),
            )
            .output()
            .expect("text pipe probe should run");
        assert!(
            output.status.success(),
            "{command} should treat a closed downstream pipe as success; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{command} should not print a panic for closed downstream pipes: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn tty_auto_mode_renders_text_and_explicit_json_stays_json() {
    let script_probe = std::process::Command::new("script")
        .arg("--version")
        .output();
    let Ok(probe) = script_probe else {
        eprintln!("skipping tty transcript test: util-linux script not installed");
        return;
    };
    let probe_stdout = String::from_utf8_lossy(&probe.stdout);
    if !probe.status.success() || !probe_stdout.contains("util-linux") {
        eprintln!("skipping tty transcript test: unsupported script implementation");
        return;
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    heddle(&["init"], Some(&repo)).unwrap();
    std::fs::write(repo.join("app.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(&repo)).unwrap();

    let binary = env!("CARGO_BIN_EXE_heddle");
    let config = repo.join(".heddle-user/config.toml");
    let repo_arg = repo.to_str().expect("repo path should be utf8");
    let config_arg = config.to_str().expect("config path should be utf8");

    let text_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} status"
    );
    let text = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &text_cmd, "/dev/null"])
        .output()
        .expect("run status under script tty");
    assert!(
        text.status.success(),
        "tty status should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&text.stdout),
        String::from_utf8_lossy(&text.stderr)
    );
    let text_stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        text_stdout.contains("Heddle status")
            && text_stdout.contains("Verdict:")
            && !text_stdout.trim_start().starts_with('{')
            && !text_stdout.contains('\u{1b}'),
        "auto mode on a TTY should render no-color human text: {text_stdout:?}"
    );

    let json_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} --output json status"
    );
    let json = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &json_cmd, "/dev/null"])
        .output()
        .expect("run explicit-json status under script tty");
    assert!(
        json.status.success(),
        "tty explicit JSON status should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&json.stdout),
        String::from_utf8_lossy(&json.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&json.stdout);
    let parsed: serde_json::Value = serde_json::from_str(json_stdout.trim())
        .unwrap_or_else(|_| panic!("explicit JSON under TTY should parse: {json_stdout:?}"));
    assert_eq!(parsed["thread_health"], "clean");

    let checkout = sibling_checkout_path(temp.path(), "tty-thread");
    let checkout_arg = checkout.to_str().expect("checkout path should be utf8");
    let start_cmd = format!(
        "NO_COLOR=1 COLUMNS=40 HEDDLE_CONFIG={config_arg} {binary} --repo {repo_arg} start tty-thread --workspace solid --path {checkout_arg}"
    );
    let start = std::process::Command::new("script")
        .args(["-q", "-e", "-c", &start_cmd, "/dev/null"])
        .output()
        .expect("run start under script tty");
    assert!(
        start.status.success(),
        "tty start should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    let start_stdout = String::from_utf8_lossy(&start.stdout);
    assert!(
        start_stdout.contains("Started isolated thread 'tty-thread'")
            && start_stdout.contains("Path:")
            && start_stdout.contains("Run this to switch shells:")
            && start_stdout.contains("cd ")
            && !start_stdout.contains("heddle ready --thread tty-thread")
            && !start_stdout.contains('\u{1b}'),
        "start on a TTY should render no-color human guidance: {start_stdout:?}"
    );
}

#[test]
fn global_exit_codes_and_failure_streams_are_predictable() {
    let help = heddle_output(&["help", "status"], None).expect("invoke help");
    assert_eq!(help.status.code(), Some(0));
    assert!(
        help.stderr.is_empty(),
        "help should write to stdout only: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: heddle status"));

    let typo = heddle_output(&["statuz"], None).expect("invoke typo");
    assert_eq!(
        typo.status.code(),
        Some(64),
        "unknown subcommand is a Usage error (sysexits EX_USAGE = 64); \
         see docs/exit-codes.md"
    );
    assert!(
        typo.stdout.is_empty(),
        "parse errors should not write primary output: {}",
        String::from_utf8_lossy(&typo.stdout)
    );
    let typo_stderr = String::from_utf8_lossy(&typo.stderr);
    assert!(
        typo_stderr.contains("unrecognized subcommand") && typo_stderr.contains("status"),
        "parse errors should name the problem and suggest likely commands: {typo_stderr}"
    );

    let temp = TempDir::new().unwrap();
    let missing_repo = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke missing-repo status");
    assert_eq!(
        missing_repo.status.code(),
        Some(78),
        "missing repository is a Config error (sysexits EX_CONFIG = 78) — \
         the precondition for any repo command is not met; see docs/exit-codes.md"
    );
    assert!(
        missing_repo.stdout.is_empty(),
        "JSON-mode failures must keep stdout clean: {}",
        String::from_utf8_lossy(&missing_repo.stdout)
    );
    let stderr = String::from_utf8_lossy(&missing_repo.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a JSON envelope: {stderr}"));
    assert_eq!(envelope["kind"], "repository_not_found");
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle init"),
        "environment failures should include a recovery hint: {envelope}"
    );
}

#[test]
fn fsck_on_corrupt_ref_emits_integrity_hint_in_text_and_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join(".heddle/refs/threads/main"),
        "bad-state-id",
    )
    .unwrap();

    let json = heddle_output(&["--output", "json", "fsck"], Some(temp.path()))
        .expect("invoke corrupt fsck json");
    assert!(
        !json.status.success(),
        "corrupt fsck JSON should exit non-zero"
    );
    assert!(
        json.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&json.stdout)
    );
    let json_stderr = String::from_utf8_lossy(&json.stderr);
    let envelope: serde_json::Value = serde_json::from_str(json_stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be JSON envelope: {json_stderr}"));
    assert_eq!(envelope["kind"], "repository_integrity_error");
    assert!(
        envelope["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid object"),
        "corrupt ref should preserve the original failure: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .unwrap_or("")
            .contains("heddle fsck --full"),
        "corrupt ref should point at fsck recovery: {envelope}"
    );

    let text = heddle_output(&["--output", "text", "fsck"], Some(temp.path()))
        .expect("invoke corrupt fsck text");
    assert!(
        !text.status.success(),
        "corrupt fsck text should exit non-zero"
    );
    assert!(
        text.stdout.is_empty(),
        "text failure must not write primary output: {}",
        String::from_utf8_lossy(&text.stdout)
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("Error: invalid object")
            && text_stderr.contains("Next: heddle fsck --full")
            && text_stderr.contains("heddle fsck --full"),
        "corrupt ref text recovery should include original error and fsck hint: {text_stderr}"
    );
}

#[test]
fn error_envelope_schema_is_registered_and_matches_runtime_shape() {
    // The error envelope is the stderr contract for JSON-mode failures.
    // `heddle schemas error` returns its mirror schema; the fields it
    // declares MUST match what `print_error_with_hint` actually emits.
    let schema = heddle(&["schemas", "error"], None).expect("heddle schemas error");
    let parsed: serde_json::Value = serde_json::from_str(&schema).expect("schema parses");
    let props = parsed["properties"]
        .as_object()
        .expect("schema has properties");
    for field in [
        "code",
        "error",
        "exit_code",
        "hint",
        "kind",
        "op_id",
        "idempotency_status",
        "replayed",
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "primary_command_template",
        "recovery_commands",
        "recovery_action_templates",
    ] {
        assert!(
            props.contains_key(field),
            "ErrorEnvelopeSchema must declare `{field}`: {schema}"
        );
    }
    let required: Vec<&str> = parsed["required"]
        .as_array()
        .expect("schema lists required fields")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for field in [
        "code",
        "error",
        "exit_code",
        "hint",
        "kind",
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "primary_command_template",
        "recovery_commands",
        "recovery_action_templates",
    ] {
        assert!(
            required.contains(&field),
            "`{field}` must be required: {schema}"
        );
    }

    // And the runtime really emits this shape: trigger a known failure
    // class and parse the stderr envelope.
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke heddle status");
    assert!(!output.status.success());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr is a JSON object");
    for field in [
        "code",
        "error",
        "exit_code",
        "hint",
        "kind",
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "primary_command_template",
        "recovery_commands",
        "recovery_action_templates",
    ] {
        assert!(
            envelope.get(field).is_some(),
            "envelope must carry `{field}` field per the schema: {stderr}"
        );
    }
    assert_eq!(envelope["kind"], "repository_not_found");
    assert_eq!(envelope["code"], "repository_not_found");
    // EX_CONFIG (sysexits) — repository config missing. Matches the
    // taxonomy in `crates/cli/src/exit.rs`.
    assert_eq!(envelope["exit_code"], 78);
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["init", temp.path().to_str().expect("temp path utf8")])
    );

    let op_id = "550e8400-e29b-41d4-a716-446655440099";
    let output = heddle_output(&["--output", "json", "--op-id", op_id, "status"], None)
        .expect("invoke op-id decorated failure");
    assert!(!output.status.success());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr is a JSON object");
    assert_eq!(envelope["op_id"], op_id);
    assert!(envelope["idempotency_status"].as_str().is_some());
    assert_eq!(envelope["replayed"], false);
}

#[test]
fn generic_json_runtime_errors_keep_nonempty_machine_envelope() {
    let output = heddle_output(&["--output", "json", "schemas", "not-a-schema"], None)
        .expect("invoke missing schema");
    assert!(!output.status.success(), "missing schema should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "schema_not_registered");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("No JSON schema is registered")),
        "runtime error envelope should preserve the original error: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| !hint.trim().is_empty()),
        "runtime error envelope must carry a non-empty hint: {envelope}"
    );
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["schemas"]),
        "schema lookup failures should recover through schema discovery, not status: {envelope}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(|templates| templates.iter().any(|template| {
                template["argv_template"] == heddle_argv_json(["commands", "--output", "json"])
            })),
        "schema lookup failures should point agents at the command catalog: {envelope}"
    );
}

#[test]
fn schema_near_miss_recommends_real_match_or_catalog_not_unrelated_schema() {
    let output =
        heddle_output(&["--output", "json", "schemas", "mer"], None).expect("invoke near miss");
    assert!(
        !output.status.success(),
        "near-miss schema lookup should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "schema_not_registered");
    assert_eq!(
        envelope["primary_command"],
        "heddle schemas merge --preview"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("merge --preview") && !hint.contains("abort")),
        "near-miss advice should point at the real nearby schema, not an unrelated verb: {envelope}"
    );
    assert!(
        envelope["recovery_commands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command == &serde_json::json!("heddle schemas"))
                && commands
                    .iter()
                    .all(|command| command != &serde_json::json!("heddle schemas abort"))),
        "schema recovery should include neutral catalog discovery and exclude unrelated fallbacks: {envelope}"
    );
}

#[test]
fn schemas_resolves_unambiguous_base_verbs_to_concrete_runtime_schema() {
    let output = heddle(&["schemas", "merge"], None).expect("heddle schemas merge");
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|err| panic!("schemas merge should emit JSON: {err}: {output}"));
    assert_eq!(parsed["title"], "MergePreviewSchema");
    let properties = parsed["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema should expose properties: {parsed}"));
    assert!(
        properties.contains_key("preview_summary") && properties.contains_key("would_merge"),
        "`heddle schemas merge` should guide agents to the merge preview schema: {parsed}"
    );
}

#[test]
fn doctor_schemas_reports_runtime_and_documented_coverage() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let repo_arg = repo_root.to_str().expect("workspace root should be utf8");
    let output = heddle(
        &["--repo", repo_arg, "doctor", "schemas", "--output", "json"],
        Some(repo_root),
    )
    .expect("heddle doctor schemas --output json");
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|_| panic!("doctor schemas should emit JSON: {output}"));

    assert_eq!(parsed["output_kind"], "doctor_schemas");
    assert_eq!(
        parsed["issues"].as_array().map(Vec::len),
        Some(0),
        "schema docs must not have drift findings: {output}"
    );
    assert_eq!(parsed["status"], "available");
    assert_eq!(parsed["verified"], true);
    assert!(
        parsed["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("advanced/internal/admin")),
        "doctor schemas should summarize the machine-contract result at the top level: {output}"
    );
    assert_eq!(parsed["recommended_action"], serde_json::Value::Null);
    assert_eq!(
        parsed["recommended_action_template"],
        serde_json::Value::Null
    );
    assert_eq!(parsed["recovery_commands"], serde_json::json!([]));
    assert_eq!(
        parsed["unmatched_verbs"].as_array().map(Vec::len),
        Some(0),
        "every documented schema verb must have a parseable documented sample: {output}"
    );
    let registered: std::collections::BTreeSet<_> = parsed["registered_verbs"]
        .as_array()
        .expect("registered verbs should be an array")
        .iter()
        .filter_map(|verb| verb.as_str())
        .collect();
    let documented: std::collections::BTreeSet<_> = parsed["documented_verbs"]
        .as_array()
        .expect("documented verbs should be an array")
        .iter()
        .filter_map(|verb| verb.as_str())
        .collect();
    let undocumented: std::collections::BTreeSet<_> = parsed["undocumented_verbs"]
        .as_array()
        .expect("undocumented verbs should be an array")
        .iter()
        .filter_map(|verb| verb.as_str())
        .collect();
    let catalog_coverage = &parsed["command_contract_schema_coverage"];
    assert!(
        catalog_coverage.is_object(),
        "doctor schemas should expose catalog-wide schema coverage separately from drift: {output}"
    );
    assert_eq!(
        catalog_coverage["json_commands_without_schema"],
        serde_json::json!(
            catalog_coverage["json_commands_total"].as_u64().unwrap()
                - catalog_coverage["json_commands_with_schema"]
                    .as_u64()
                    .unwrap()
                - catalog_coverage["json_commands_with_accepted_opaque_schema"]
                    .as_u64()
                    .unwrap()
        ),
        "catalog schema gap count should be derived from all JSON-capable commands: {output}"
    );
    assert_eq!(catalog_coverage["status"], "available");
    assert_eq!(catalog_coverage["verified_scope"], "everyday_and_agent");
    assert_eq!(
        catalog_coverage["advanced_scope"],
        "advanced_internal_admin"
    );
    assert_eq!(catalog_coverage["json_commands_without_schema"], 0);
    assert_eq!(catalog_coverage["mutating_commands_without_schema"], 0);
    assert_eq!(
        catalog_coverage["verified_scope_json_commands_with_accepted_opaque_schema"], 0,
        "verified advertised scope must not rely on opaque schemas: {output}"
    );
    assert!(
        catalog_coverage["advanced_scope_json_commands_with_accepted_opaque_schema"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "advanced scope should segment opaque schemas outside verified coverage: {output}"
    );
    assert_eq!(
        catalog_coverage["verified_scope_json_commands_without_schema"], 0,
        "verified advertised scope must have schemas for every JSON command: {output}"
    );
    assert_eq!(
        catalog_coverage["undocumented_schema_verbs_total"], 0,
        "all runtime schema verbs should have documented samples: {output}"
    );
    assert!(
        catalog_coverage["accepted_opaque_schema_verbs_total"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "advanced generic schema verbs should be explicit accepted opaque coverage: {output}"
    );
    assert_eq!(
        catalog_coverage["unaccepted_opaque_schema_verbs_total"], 0,
        "clean doctor schemas must not hide unaccepted opaque generic schemas: {output}"
    );
    assert!(
        undocumented.is_empty(),
        "doctor schemas should have no runtime-only schema verbs left: {output}"
    );
    assert_eq!(
        registered.len(),
        documented.len(),
        "doctor schemas should account for every runtime schema verb exactly once: {output}"
    );
    for verb in documented.iter().chain(undocumented.iter()) {
        assert!(
            registered.contains(verb),
            "reported verb `{verb}` should be in the runtime registry: {output}"
        );
    }
    for verb in [
        "branch",
        "switch",
        "checkout",
        "bridge git reconcile",
        "capture",
        "commit",
        "actor spawn",
        "actor list",
        "actor show",
        "actor explain",
        "actor done",
        "agent serve",
        "agent status",
        "agent stop",
        "agent reserve",
        "agent heartbeat",
        "agent capture",
        "agent ready",
        "agent release",
        "agent list",
        "revert",
        "remote add",
        "remote remove",
        "remote set-default",
        "stash push",
        "stash list",
        "stash pop",
        "stash apply",
        "stash drop",
        "stash clear",
        "stash show",
        "session start",
        "session segment",
        "session end",
        "session show",
        "session list",
        "start",
        "thread create",
        "thread current",
        "thread switch",
        "thread captures",
        "thread rename",
        "thread refresh",
        "thread drop",
        "thread show",
        "undo",
    ] {
        assert!(
            documented.contains(verb),
            "high-value runtime schema `{verb}` should be documented and sample-checked: {output}"
        );
        assert!(
            !undocumented.contains(verb),
            "documented runtime schema `{verb}` should not be reported as a docs coverage gap: {output}"
        );
    }
    for verb in [
        "schemas",
        "doctor",
        "doctor docs",
        "doctor schemas",
        "git-overlay",
        "version",
        "watch",
        "try",
        "blame",
        "fsck",
        "resolve",
    ] {
        assert!(
            documented.contains(verb),
            "agent-critical runtime schema `{verb}` should be documented and sample-checked: {output}"
        );
        assert!(
            !undocumented.contains(verb),
            "agent-critical runtime schema `{verb}` should not be a docs coverage gap: {output}"
        );
    }
    assert!(
        !undocumented.contains("status"),
        "documented runtime schemas should not also appear in coverage gaps: {output}"
    );
}

#[test]
fn push_without_default_remote_uses_typed_json_recovery() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["--output", "json", "push"], Some(temp.path()))
        .expect("invoke push without default remote");
    assert!(
        !output.status.success(),
        "push without a remote should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("push failure should emit JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "remote_not_configured");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle remote add <name> <url>")
                && hint.contains("heddle remote list")
                && hint.contains("heddle remote set-default <name>")),
        "push remote setup hint should be specific and actionable: {envelope}"
    );
    assert_eq!(
        envelope["primary_command"],
        "heddle remote add <name> <url>"
    );
    assert!(envelope["primary_command_argv"].is_null(), "{envelope}");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["remote", "add", "<name>", "<url>"]),
        "{envelope}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(
                |templates| templates.iter().any(|template| template["argv_template"]
                    == heddle_argv_json(["remote", "set-default", "<name>"]))
            ),
        "push remote setup should include structured set-default recovery: {envelope}"
    );
    assert!(
        envelope["recovery_commands"]
            .as_array()
            .is_some_and(|commands| commands.contains(&serde_json::json!("heddle remote list"))),
        "push remote setup should include remote inspection recovery: {envelope}"
    );
}

#[test]
fn push_with_unknown_remote_uses_typed_json_recovery() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "push", "missing-remote"],
        Some(temp.path()),
    )
    .expect("invoke push with unknown remote");
    assert!(!output.status.success(), "unknown remote push should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON failure must not pollute stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("push failure should emit JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "remote_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("missing-remote")),
        "unknown remote error should name the requested remote: {envelope}"
    );
    assert_eq!(envelope["primary_command"], "heddle remote list");
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["remote", "list"]),
        "{envelope}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(
                |templates| templates.iter().any(|template| template["argv_template"]
                    == heddle_argv_json(["remote", "add", "<name>", "<url>"]))
            ),
        "unknown remote recovery should include structured remote add template: {envelope}"
    );
}

#[test]
fn doctor_schemas_json_failure_uses_recovery_envelope() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("docs")).unwrap();
    std::fs::write(
        temp.path().join("docs/json-schemas.md"),
        "\
## `heddle status --output json`

```json
{\"verified\": true}
```
",
    )
    .unwrap();

    let repo_arg = temp.path().to_str().expect("temp path should be utf8");
    let output = heddle_output(
        &["--repo", repo_arg, "doctor", "schemas", "--output", "json"],
        Some(temp.path()),
    )
    .expect("invoke heddle doctor schemas");

    assert!(
        !output.status.success(),
        "schema failure should exit non-zero"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON failure should not also emit the success report on stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("schema failure should emit one JSON envelope: {err}: {stderr}")
    });
    assert_eq!(envelope["kind"], "machine_contract_drift");
    assert_eq!(envelope["code"], "machine_contract_drift");
    assert_eq!(
        envelope["primary_command"],
        "heddle doctor schemas --output json"
    );
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["doctor", "schemas", "--output", "json"])
    );
    assert_json_recovery_advice_fields(&envelope, stderr);
}

#[test]
fn doctor_schemas_outside_source_tree_points_agents_to_catalog_surfaces() {
    let temp = TempDir::new().unwrap();
    let output = heddle_output(
        &["doctor", "schemas", "--output", "json"],
        Some(temp.path()),
    )
    .expect("invoke heddle doctor schemas outside source tree");

    assert!(
        !output.status.success(),
        "source-docs drift check should fail outside the source checkout"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON failure should not emit a partial success report on stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("doctor schemas source-docs failure should emit JSON: {err}: {stderr}")
    });
    assert_eq!(envelope["kind"], "doctor_schemas_source_docs_missing");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("source checkout")
                && hint.contains("heddle commands --output json")
                && hint.contains("heddle schemas status")),
        "installed-agent hint should point to catalog/schema surfaces, not repo init: {envelope}"
    );
    assert_eq!(envelope["primary_command"], "heddle commands --output json");
    assert!(
        envelope["recovery_commands"]
            .as_array()
            .is_some_and(|commands| commands.iter().all(|command| command
                .as_str()
                .is_none_or(|text| !text.contains("heddle init")))),
        "doctor schemas outside source tree should not imply repo initialization fixes docs drift: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, stderr);
}

#[test]
fn status_text_hides_capture_durability_local_only_by_default() {
    // The fallback "Capture durability: local only" line repeated on
    // every `heddle status` against a non-checkpointed state — pure
    // noise since the absence of a `Git checkpoint:` line already
    // encodes the same information. Hidden by default; `-v` brings it
    // back. JSON output is unchanged (the field is on the wire shape).
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("a"), "1").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let default =
        heddle(&["--output", "text", "status"], Some(temp.path())).expect("status default");
    assert!(
        !default.contains("Capture durability:"),
        "default status must not show the local-only fallback: {default}"
    );

    let verbose =
        heddle(&["--output", "text", "-v", "status"], Some(temp.path())).expect("status -v");
    assert!(
        verbose.contains("Capture durability: local only"),
        "-v status must surface the durability line: {verbose}"
    );
}

#[test]
fn default_status_and_log_hide_internal_hashes_until_verbose() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    let capture = heddle_output_with_env(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
        &[
            ("HEDDLE_AGENT_PROVIDER", "codex"),
            ("HEDDLE_AGENT_MODEL", "gpt-5"),
        ],
    )
    .expect("capture with agent attribution should run");
    assert!(capture.status.success(), "capture should succeed");

    let status_json = json_value(temp.path(), &["status", "--output", "json"]);
    let content_hash = status_json["state"]["content_hash"]
        .as_str()
        .expect("status JSON should retain the content hash");

    let status_text =
        heddle(&["--output", "text", "status"], Some(temp.path())).expect("status text");
    for hidden in [
        content_hash,
        "Base:",
        "Git checkpoint:",
        "Agent:",
        "Usage:",
        "State:",
        "Intent:",
    ] {
        assert!(
            !status_text.contains(hidden),
            "default status should hide internal detail `{hidden}`: {status_text}"
        );
    }

    let status_verbose =
        heddle(&["--output", "text", "-v", "status"], Some(temp.path())).expect("status -v");
    assert!(
        status_verbose.contains(content_hash)
            && status_verbose.contains("Base:")
            && status_verbose.contains("State:")
            && status_verbose.contains("Intent:"),
        "verbose status should keep diagnostic state internals available: {status_verbose}"
    );

    let log_text = heddle(&["--output", "text", "log"], Some(temp.path())).expect("log text");
    for hidden in [content_hash, "Agent:", "Git checkpoint:", "Principal:"] {
        assert!(
            !log_text.contains(hidden),
            "default log should hide internal detail `{hidden}`: {log_text}"
        );
    }

    let log_oneline = heddle(&["--output", "text", "log", "--oneline"], Some(temp.path()))
        .expect("log --oneline");
    assert!(
        !log_oneline.contains(content_hash),
        "default oneline log should not spend a column on the content hash: {log_oneline}"
    );

    let log_verbose =
        heddle(&["--output", "text", "-v", "log"], Some(temp.path())).expect("log -v");
    assert!(
        log_verbose.contains(content_hash)
            && log_verbose.contains("Agent:")
            && log_verbose.contains("Principal:"),
        "verbose log should keep content hashes and attribution available: {log_verbose}"
    );

    let log_json = json_value(temp.path(), &["log", "--output", "json"]);
    assert_eq!(
        log_json["states"][0]["content_hash"], content_hash,
        "log JSON should keep exact machine fields: {log_json}"
    );
}

#[test]
fn default_undo_text_hides_batches_and_checkpoint_ids_until_verbose() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "seed\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["adopt", "--ref", "main"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("tracked.txt"), "seed\nchanged\n").unwrap();
    heddle(&["commit", "-m", "write saved change"], Some(temp.path())).unwrap();

    let list = heddle(&["--output", "text", "undo", "--list"], Some(temp.path()))
        .expect("undo --list text");
    for hidden in ["Batch", "batch", "git checkpoint"] {
        assert!(
            !list.contains(hidden),
            "default undo history should hide implementation detail `{hidden}`: {list}"
        );
    }
    assert!(
        list.contains("Recent undo history") && list.contains("Git commit written"),
        "default undo history should describe user-visible state: {list}"
    );

    let preview = heddle(
        &["--output", "text", "undo", "--preview"],
        Some(temp.path()),
    )
    .expect("undo preview text");
    for hidden in ["Batch", "batch", "git checkpoint"] {
        assert!(
            !preview.contains(hidden),
            "default undo preview should hide implementation detail `{hidden}`: {preview}"
        );
    }
    assert!(
        preview.contains("Would undo 1 saved change"),
        "default undo preview should read as a user action: {preview}"
    );

    let verbose = heddle(
        &["--output", "text", "-v", "undo", "--list"],
        Some(temp.path()),
    )
    .expect("verbose undo list");
    assert!(
        verbose.contains("Batch") && verbose.contains("git checkpoint"),
        "verbose undo history should keep exact operation detail available: {verbose}"
    );

    let list_json = json_value(temp.path(), &["undo", "--list", "--output", "json"]);
    assert!(
        list_json["batches"]
            .as_array()
            .is_some_and(|batches| !batches.is_empty()),
        "undo JSON should keep batch-level contract fields: {list_json}"
    );
}

#[test]
fn blame_drops_email_when_attribution_overflows_column() {
    // `Ada Lovelace <ada@really.long.example.com>` blew the 20-char column,
    // truncating to `Ada Lovelace <ada...` — keeping the noise and
    // dropping the signal. The fit_author helper drops the email
    // entirely when the name alone fits the column.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let user_cfg = temp.path().with_extension("ada-blame-config.toml");
    std::fs::write(
        &user_cfg,
        "[principal]\nname = \"Ada Lovelace\"\nemail = \"ada@really.long.example.com\"\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("note.txt"), "first line\nsecond line\n").unwrap();
    heddle_output_with_env(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", user_cfg.to_str().unwrap())],
    )
    .unwrap();

    let blame = heddle_output_with_env(
        &["--output", "text", "blame", "note.txt"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", user_cfg.to_str().unwrap())],
    )
    .expect("blame note.txt");
    assert!(
        blame.status.success(),
        "blame should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&blame.stdout),
        String::from_utf8_lossy(&blame.stderr)
    );
    let output = String::from_utf8_lossy(&blame.stdout);
    assert!(
        output.contains("Ada Lovelace"),
        "blame must show the principal name: {output}"
    );
    assert!(
        !output.contains("Ada Loveli...") && !output.contains("Ada Lovela..."),
        "blame must not mid-name-truncate when the name itself fits: {output}"
    );
    assert!(
        !output.contains("really.long"),
        "blame must drop the email when the name fits the column: {output}"
    );
}

#[test]
fn blame_missing_file_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    heddle(&["capture", "-m", "tracked"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "blame", "missing.txt"],
        Some(temp.path()),
    )
    .expect("invoke missing blame");
    assert!(!output.status.success(), "missing blame should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing blame refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing blame should emit JSON envelope");
    assert_eq!(envelope["kind"], "blame_file_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("File 'missing.txt' not found in state")),
        "missing blame should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle show")),
        "missing blame hint should name state inspection: {stderr}"
    );
}

#[test]
fn freshly_initialized_repo_reports_clean_health() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Verdict: clean"),
        "a fresh init should be healthy, not 'needs_attention': {text}"
    );
    assert!(
        !text.contains("Next step:"),
        "a fresh init has nothing to recommend; the renderer should stay silent: {text}"
    );

    let json = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    assert!(
        json.contains(r#""thread_health":"clean""#),
        "fresh-init JSON should carry the same 'clean' health: {json}"
    );
    assert!(
        json.contains(r#""output_kind":"status""#) && json.contains(r#""recommended_action":null"#),
        "fresh-init JSON should expose status output kind and null recommended_action: {json}"
    );
}

/// Build a local bare git repo with `master` carrying `commits`
/// commits, suitable for `heddle clone` from a local path.
fn make_local_master_git_repo(parent: &std::path::Path, commits: usize) -> std::path::PathBuf {
    let bare = parent.join("origin.git");
    let repo = gix::init_bare(&bare).expect("init bare origin");
    let mut parent_oid: Option<gix::hash::ObjectId> = None;
    for i in 0..commits {
        let blob = repo
            .write_blob(format!("content {i}\n").as_bytes())
            .expect("write blob")
            .detach();
        let empty = repo.empty_tree().id;
        let mut editor = repo.edit_tree(empty).expect("edit tree");
        editor
            .upsert(
                format!("f{i}.txt"),
                gix::object::tree::EntryKind::Blob,
                blob,
            )
            .expect("add file");
        let tree = editor.write().expect("write tree").detach();
        let parents = parent_oid.map(|p| vec![p]).unwrap_or_default();
        let commit = git_commit_with_tree(
            &repo,
            Some("refs/heads/master"),
            tree,
            &format!("c{i}"),
            &parents,
        );
        parent_oid = Some(commit);
    }
    // Honour the remote default branch so `heddle clone` picks `master`.
    git_set_reference(&repo, "HEAD", parent_oid.expect("at least one commit"));
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/master\n")
        .expect("pin remote HEAD to master");
    bare
}

#[test]
fn bridge_git_import_after_clone_reports_commits_not_zero() {
    // heddle#147: rerunning `bridge git import --ref master --path .`
    // after `heddle clone` used to land at `commits_imported: 0` even
    // though every commit on master had been imported during clone —
    // visually indistinguishable from "your import did nothing".
    // After the fix, `commits_imported` reports commits walked (matching
    // `bridge git ingest`), `states_created` carries the dedup story,
    // and an `already_in_sync` flag tags the no-op case so callers can
    // render the right thing.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 3);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone should succeed");

    let json = heddle(
        &[
            "--output", "json", "bridge", "git", "import", "--ref", "master", "--path", ".",
        ],
        Some(&work),
    )
    .expect("rerun bridge git import");
    let parsed: Value = serde_json::from_str(&json).expect("import JSON parses");
    assert_eq!(
        parsed["commits_imported"], 3,
        "commits_imported should report walked commits, not just new states: {json}"
    );
    assert_eq!(
        parsed["states_created"], 0,
        "no new heddle states should be created on a re-import: {json}"
    );
    assert_eq!(
        parsed["already_in_sync"], true,
        "already_in_sync should flag the no-op case: {json}"
    );
    assert_eq!(parsed["branches_synced"], 1);

    let text = heddle(
        &[
            "--output", "text", "bridge", "git", "import", "--ref", "master", "--path", ".",
        ],
        Some(&work),
    )
    .expect("rerun import text");
    assert!(
        text.contains("already in sync"),
        "text output should call out that the import was a no-op: {text}"
    );
}

#[test]
fn bridge_git_status_recommendation_runs_cleanly_after_clone() {
    // heddle#148: the recommended-action chain from `bridge git status`
    // used to dead-end at `heddle sync`. After clone, the bridge is in
    // sync (no missing branches) — the import_hint must be absent.
    // This is the structural side of the chain: status doesn't try to
    // drive the operator into a verb that errors.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 2);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone");

    let json = heddle(
        &["--output", "json", "bridge", "git", "status"],
        Some(&work),
    )
    .expect("bridge git status JSON");
    let parsed: Value = serde_json::from_str(&json).expect("status JSON parses");
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "bridge git status should report no missing branches after clone: {json}"
    );
}

#[test]
fn verify_after_git_overlay_clone_reports_clone_verified() {
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 2);
    let work = temp.path().join("work");

    let clone_json = heddle(
        &[
            "--output",
            "json",
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone");
    let clone_output: Value = inject_post_verification_at(
        &work,
        &["clone"],
        serde_json::from_str(&clone_json).expect("clone JSON parses"),
    );
    assert_eq!(clone_output["output_kind"], "clone");
    assert_eq!(clone_output["action"], "clone");
    assert_eq!(clone_output["status"], "cloned");
    assert_eq!(clone_output["success"], true);
    assert_eq!(clone_output["cloned"], true);
    assert_eq!(clone_output["transport"], "git");
    assert_eq!(clone_output["branch"], "master");
    assert_eq!(clone_output["repository_capability"], "git-overlay");
    assert_eq!(clone_output["commits_imported"], 2);
    assert_eq!(clone_output["states_created"], 2);
    assert_eq!(
        clone_output["verification"]["clone_verification"], "verified",
        "clone JSON should prove verify without requiring a follow-up verify probe: {clone_json}"
    );
    assert_eq!(clone_output["verification"]["verified"], true);
    assert_eq!(
        clone_output["verification"]["recommended_action"],
        Value::Null,
        "clean clone verify should not recommend extra recovery: {clone_json}"
    );
    assert_eq!(
        clone_output["verification"]["recommended_action_template"]["argv_template"],
        Value::Null
    );
    assert_eq!(
        clone_output["verification"]["recovery_commands"],
        serde_json::json!([])
    );
    let exclude = std::fs::read_to_string(work.join(".git/info/exclude")).unwrap();
    {
        let pattern = ".heddle/";
        assert!(
            exclude.lines().any(|line| line.trim() == pattern),
            "clone should install the same local Git exclude policy as init; missing {pattern:?}: {exclude}"
        );
    }
    for pattern in [".heddleignore", "__pycache__", "*.pyc"] {
        assert!(
            !exclude.lines().any(|line| line.trim() == pattern),
            "clone should not auto-ignore project artifacts; found {pattern:?}: {exclude}"
        );
    }

    let json = heddle(&["--output", "json", "verify"], Some(&work)).expect("verify JSON");
    let parsed: Value = serde_json::from_str(&json).expect("verify JSON parses");
    assert_eq!(
        parsed["clone_verification"], "verified",
        "git-overlay verify should treat a clean mapped checkout as clone-verified: {json}"
    );
    assert_eq!(parsed["recommended_action"], Value::Null);
    assert_eq!(parsed["recommended_action_argv"], Value::Null);
    assert_eq!(parsed["recovery_commands"], serde_json::json!([]));
    assert!(
        parsed.get("verification").is_none(),
        "verify JSON should not duplicate itself under a nested verify object: {json}"
    );
    let checks = parsed["checks"].as_array().expect("checks array");
    let clone = checks
        .iter()
        .find(|check| check["name"] == "Clone")
        .unwrap_or_else(|| panic!("verify checks should include Clone row: {json}"));
    assert_eq!(clone["status"], "verified");
    assert_eq!(clone["clean"], true);

    let text =
        heddle(&["--output", "text", "--verbose", "verify"], Some(&work)).expect("verify text");
    assert!(
        text.contains("Checkout") && text.contains("Git checkout and Heddle mapping agree"),
        "verify text should make checkout verification confidence visible: {text}"
    );
    assert!(
        !text.contains("clone verification is not applicable"),
        "git-overlay clone verify should not undercut itself as not applicable: {text}"
    );
    assert!(
        !text.contains("Next:"),
        "clean human verify should not print an empty next action: {text}"
    );
}

#[test]
fn plain_git_verify_renders_clone_check_as_not_applicable_not_ok() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");

    let output = heddle_output(
        &["--output", "text", "--verbose", "verify"],
        Some(temp.path()),
    )
    .expect("invoke strict verify text");
    assert!(
        !output.status.success(),
        "blocked plain Git verbose verify should exit nonzero"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Checkout") && text.contains("n/a"),
        "plain Git verify should render checkout verification as not applicable, not successful: {text}"
    );
    assert!(
        !text.contains("Checkout          ok clone verification is not applicable"),
        "plain Git verify should not mix ok status with non-applicable checkout verification: {text}"
    );
    assert!(
        !temp.path().join(".heddle").exists(),
        "plain Git verify must remain observe-only"
    );
}

#[test]
fn bridge_git_divergence_error_uses_structured_recovery_envelope() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    let base_oid = {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(temp.path())
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success(), "git rev-parse should succeed");
        String::from_utf8(output.stdout)
            .expect("git oid should be UTF-8")
            .trim()
            .to_string()
    };

    json_value(temp.path(), &["adopt", "--output", "json"]);
    std::fs::write(temp.path().join("tracked.txt"), "heddle side\n").unwrap();
    json_value(
        temp.path(),
        &["commit", "-m", "heddle side", "--output", "json"],
    );

    let reset = std::process::Command::new("git")
        .args(["reset", "--hard", &base_oid])
        .current_dir(temp.path())
        .output()
        .expect("git reset should run");
    assert!(
        reset.status.success(),
        "git reset should succeed: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    std::fs::write(temp.path().join("tracked.txt"), "git side\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "git side");

    let output = heddle_output(
        &[
            "--output", "json", "bridge", "git", "import", "--ref", "main",
        ],
        Some(temp.path()),
    )
    .expect("invoke bridge git import");
    assert!(
        !output.status.success(),
        "diverged import should fail closed"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON error envelope should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8");
    let envelope = parse_exactly_one_json_value(stderr)
        .unwrap_or_else(|err| panic!("stderr should be one JSON envelope: {err}: {stderr}"));
    assert_json_recovery_advice_fields(&envelope, stderr);
    assert_eq!(envelope["kind"], "git_heddle_thread_diverged");
    assert_eq!(envelope["code"], "git_heddle_thread_diverged");
    assert_eq!(
        envelope["primary_command"],
        "heddle bridge git reconcile --ref main --preview"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!(["heddle bridge git reconcile --ref main --preview"])
    );
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("imported commit states")
                && preserved.contains("Git/Heddle mapping records")),
        "diverged import should describe preserved partial state: {envelope}"
    );
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json(["bridge", "git", "reconcile", "--ref", "main", "--preview",])
    );
}

#[test]
fn bridge_git_import_schema_declares_already_in_sync() {
    // heddle#147 added `already_in_sync: bool` to the JSON output of
    // `bridge git import`. The schema contract surfaced via
    // `heddle schemas "bridge git import"` must list the field, or
    // automation that validates against the schema will reject the
    // new payload shape.
    let schema = heddle(&["schemas", "bridge git import"], None)
        .expect("heddle schemas \"bridge git import\"");
    let parsed: Value = serde_json::from_str(&schema).expect("schema parses");
    let props = parsed["properties"]
        .as_object()
        .expect("schema has properties");
    assert!(
        props.contains_key("already_in_sync"),
        "BridgeImportSchema must declare `already_in_sync`: {schema}"
    );
    assert_eq!(
        props["already_in_sync"]["type"], "boolean",
        "`already_in_sync` must be a boolean: {schema}"
    );
}

#[test]
fn bridge_git_sync_after_clone_reports_zero_imported() {
    // heddle#147 made the import walker count every walked commit in
    // `commits_imported`. `bridge git sync` re-uses the importer, so
    // a no-op sync of an already-synced overlay used to report the
    // full walked history as `commits_imported` — exactly the signal
    // operators rely on sync to suppress. Sync must keep its
    // `commits_imported` scoped to commits that produced a new
    // heddle state on this run.
    let temp = TempDir::new().unwrap();
    let bare = make_local_master_git_repo(temp.path(), 3);
    let work = temp.path().join("work");

    heddle(
        &[
            "clone",
            bare.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("heddle clone");

    let json = heddle(&["--output", "json", "bridge", "git", "sync"], Some(&work))
        .expect("bridge git sync JSON");
    let parsed: Value = inject_post_verification_at(
        &work,
        &["bridge", "git", "sync"],
        serde_json::from_str(&json).expect("sync JSON parses"),
    );
    assert_eq!(parsed["output_kind"], "bridge_git_sync");
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["action"], "bridge git sync");
    assert_eq!(
        parsed["commits_imported"], 0,
        "no-op sync should report zero newly-imported commits, not the \
         walked history: {json}"
    );
    assert_eq!(
        parsed["verification"]["verified"], true,
        "bridge git sync JSON should include the post-sync verification contract: {json}"
    );

    let text = heddle(&["--output", "text", "bridge", "git", "sync"], Some(&work))
        .expect("bridge git sync text");
    assert!(
        text.contains("imported: 0 commits") || text.contains("imported: 0"),
        "text output should also report zero imported on a no-op sync: {text}"
    );
}

/// Every non-`Ok` (0) exit code declared on any `CommandContract.exit_codes`
/// entry must be documented in `docs/exit-codes.md`. Catches the
/// "added a new code, forgot to update the table" regression.
#[test]
fn exit_codes_declared_have_doc_entry() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/exit-codes.md"
    ))
    .expect("docs/exit-codes.md should exist");

    let catalog = cli::cli::commands::build_command_catalog();
    let mut declared = std::collections::BTreeSet::new();
    for entry in &catalog.commands {
        for code in &entry.exit_codes {
            if code.code != 0 {
                declared.insert(code.code);
            }
        }
    }

    assert!(
        !declared.is_empty(),
        "no command declares non-zero exit codes; the representative sweep is missing"
    );

    for code in declared {
        let needle = format!("|  {code}  |");
        let alt_needle = format!("| {code} |");
        assert!(
            doc.contains(&needle) || doc.contains(&alt_needle),
            "docs/exit-codes.md is missing a table row for code {code}; \
             add it or remove the declaration from CommandContract.exit_codes"
        );
    }
}

/// Schema-stability contract: `exit_codes` must surface in the JSON catalog
/// (`heddle commands --output json`). Agents discover the contract via that
/// JSON; if a future refactor drops the field, every agent retry policy
/// degrades silently.
#[test]
fn exit_codes_surface_in_json_catalog() {
    let catalog = cli::cli::commands::build_command_catalog();
    let push = catalog
        .commands
        .iter()
        .find(|c| c.display == "push")
        .expect("push command in catalog");
    assert!(
        push.exit_codes.iter().any(|c| c.code == 75),
        "push must surface TempFail (75) in its catalogued exit_codes; \
         agents key retry behavior off this code"
    );
    let bridge_import = catalog
        .commands
        .iter()
        .find(|c| c.display == "bridge git import")
        .expect("bridge git import command in catalog");
    assert!(
        bridge_import.exit_codes.iter().any(|c| c.code == 65),
        "bridge git import must surface DataErr (65) for malformed repos"
    );
}

/// `heddle log`, `show`, and `bridge git status` default text views must
/// lead with command data — not the `Repository:` mode preamble, which is
/// noise on every read (heddle#275). `-v` keeps the preamble for
/// diagnostics, and `--output json` is untouched.
#[test]
fn read_commands_gate_repository_preamble_on_verbose() {
    let temp = TempDir::new().unwrap();
    init_git_repo_for_json_contract(temp.path(), "main");
    std::fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all_for_json_contract(temp.path(), "seed");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["bridge", "git", "import", "--ref", "main"],
        Some(temp.path()),
    )
    .unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked changed\n").unwrap();
    heddle(&["commit", "-m", "checkpoint"], Some(temp.path())).unwrap();

    for (label, default_args, verbose_args) in [
        (
            "log",
            vec!["log", "--output", "text"],
            vec!["-v", "log", "--output", "text"],
        ),
        (
            "show",
            vec!["show", "HEAD", "--output", "text"],
            vec!["-v", "show", "HEAD", "--output", "text"],
        ),
        (
            "bridge git status",
            vec!["bridge", "git", "status", "--output", "text"],
            vec!["-v", "bridge", "git", "status", "--output", "text"],
        ),
    ] {
        let default_text = heddle(&default_args, Some(temp.path()))
            .unwrap_or_else(|e| panic!("{label} default text should render: {e}"));
        assert!(
            !default_text.contains("Repository:"),
            "{label} default text leaked the mode preamble: {default_text}"
        );
        // Suppressing the preamble must not leave the spacer that used to
        // follow it dangling as a leading blank line (heddle#275 r2).
        assert!(
            !default_text.starts_with('\n'),
            "{label} default text starts with an orphaned blank line: {default_text:?}"
        );

        let verbose_text = heddle(&verbose_args, Some(temp.path()))
            .unwrap_or_else(|e| panic!("{label} verbose text should render: {e}"));
        assert!(
            verbose_text.contains("Repository:"),
            "{label} -v text should retain the mode preamble: {verbose_text}"
        );

        let json = heddle(
            &default_args
                .iter()
                .map(|a| if *a == "text" { "json" } else { a })
                .collect::<Vec<_>>(),
            Some(temp.path()),
        )
        .unwrap_or_else(|e| panic!("{label} json should render: {e}"));
        let parsed: Value = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{label} json should parse: {e}"));
        assert!(
            parsed["repository_capability"].is_string(),
            "{label} json must keep repository_capability: {json}"
        );
    }
}
