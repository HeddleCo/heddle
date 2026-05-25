// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_bridge_git_init() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    assert!(heddle(&["bridge", "init"], Some(temp.path())).is_ok());
    assert!(
        temp.path().join(".heddle/git").exists(),
        "Git mirror should exist"
    );
}

#[test]
fn test_cli_bridge_git_export_and_pull_roundtrip() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let dest_holder = TempDir::new().unwrap();
    let dest = dest_holder.path().join("export");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "bridge export").unwrap();
    heddle(&["capture", "-m", "Bridge source"], Some(source.path())).unwrap();

    // Phase A: `bridge export` requires `--destination`. Pre-Phase-A
    // it silently no-op'd if no flag was given (writing only the sidecar
    // mapping, not actually exporting any git objects). Now it errors.
    let export = heddle(
        &["bridge", "export", "--destination", dest.to_str().unwrap()],
        Some(source.path()),
    );
    assert!(export.is_ok(), "bridge export failed: {:?}", export.err());

    let dest_repo = gix::open(&dest).unwrap();
    assert!(dest_repo.find_reference("refs/heads/main").is_ok());

    heddle(&["init"], Some(target.path())).unwrap();
    let pull = heddle(
        &["bridge", "pull", dest.to_str().unwrap()],
        Some(target.path()),
    );
    assert!(pull.is_ok(), "Bridge pull failed: {:?}", pull.err());

    let target_repo = Repository::open(target.path()).unwrap();
    assert!(target_repo.refs().get_thread("main").unwrap().is_some());
}

#[test]
fn test_cli_bridge_git_import_from_external_repo() {
    let heddle_repo_dir = TempDir::new().unwrap();
    let git_repo_dir = TempDir::new().unwrap();
    let git_repo = gix::init(git_repo_dir.path()).unwrap();
    let tree_oid = git_empty_tree_oid(&git_repo);
    git_commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        tree_oid,
        "Imported commit",
        &[],
    );

    heddle(&["init"], Some(heddle_repo_dir.path())).unwrap();
    let result = heddle(
        &[
            "bridge",
            "import",
            "--path",
            git_repo_dir.path().to_str().unwrap(),
        ],
        Some(heddle_repo_dir.path()),
    );
    assert!(result.is_ok(), "Bridge import failed: {:?}", result.err());

    let repo = Repository::open(heddle_repo_dir.path()).unwrap();
    assert!(repo.refs().get_thread("main").unwrap().is_some());
}

#[test]
fn test_cli_bridge_git_push_to_local_bare_remote() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let remote_repo = gix::init_bare(remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("push.txt"), "bridge push").unwrap();
    heddle(&["capture", "-m", "Bridge push"], Some(source.path())).unwrap();

    let result = heddle(
        &["bridge", "push", remote.path().to_str().unwrap()],
        Some(source.path()),
    );
    assert!(result.is_ok(), "Bridge push failed: {:?}", result.err());

    assert!(remote_repo.find_reference("refs/heads/main").is_ok());
}

/// `heddle push --mirror=<git-remote>` performs the primary push to the
/// heddle remote AND a git-bridge push to the configured mirror, in one
/// invocation.
#[test]
fn test_cli_push_mirror_dual_push_to_weft_and_git_remote() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    let mirror_repo = gix::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "dual push").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    // `--output text` forces the text branch of `render_mirror_outcome`.
    // Default `auto` resolves to JSON when stdout is piped (which it is
    // under `cargo test`), so without this flag the text-success path
    // would never execute and codecov/patch would miss it.
    let stdout = heddle(
        &[
            "--output", "text", "push", &weft_path, "--thread", "main", &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("dual push (--mirror=<remote>) should succeed");

    // Text branch on success emits a "mirrored to <remote>" line.
    assert!(
        stdout.contains("mirrored to") && stdout.contains(&git_path),
        "text-mode success line missing: {}",
        stdout
    );

    // Primary push landed at the heddle target.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "weft target should have main thread after primary push: {}",
        threads
    );

    // Mirror push landed at the bare git remote.
    assert!(
        mirror_repo.find_reference("refs/heads/main").is_ok(),
        "git mirror remote should have refs/heads/main after mirror push"
    );
}

/// Mirror push failure is reported as a warning but does NOT cause the
/// primary push to fail. The user still sees the primary push succeed.
#[test]
fn test_cli_push_mirror_failure_does_not_abort_primary_push() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "warn on mirror fail").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    // Pointing the mirror at a nonexistent path is a failure.
    let bogus_mirror = source
        .path()
        .join("does-not-exist-mirror")
        .to_string_lossy()
        .to_string();
    let mirror_arg = format!("--mirror={}", bogus_mirror);
    // `--output text` forces the text branch of `render_mirror_outcome`
    // on the failure path. The warning lands on stderr, so this test
    // proves the primary push still succeeds and leaves separate
    // stderr-checking to the JSON variant (which captures the structured
    // failure on stdout).
    let result = heddle(
        &[
            "--output", "text", "push", &weft_path, "--thread", "main", &mirror_arg,
        ],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "primary push must still succeed even when mirror push fails: {:?}",
        result.err()
    );

    // Primary push still landed.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "primary push should land even if mirror push fails: {}",
        threads
    );
}

/// `--mirror` MUST require `=` to take an explicit value. Without
/// `require_equals = true`, clap would consume the next token (the
/// positional primary remote) as the mirror value, silently pushing
/// the primary to the configured default and the mirror to the
/// intended primary target.
///
/// Pins the behavior: `heddle push --mirror <PRIMARY>` parses
/// `<PRIMARY>` as the positional remote, and `--mirror` takes its
/// `default_missing_value` ("origin"). Since no `origin` git remote
/// is configured here, the mirror push warns but does not abort.
#[test]
fn test_cli_push_mirror_requires_equals_does_not_swallow_positional() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "require equals").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    // Space form with the positional remote immediately after
    // `--mirror`. Without `require_equals=true`, clap consumes
    // `weft_path` as the mirror's value, leaving the primary remote
    // unspecified — silently inverting the user's intent. With
    // `require_equals=true`, `--mirror` takes its
    // `default_missing_value` and `weft_path` parses as the
    // positional primary remote.
    let result = heddle(
        &["push", "--mirror", &weft_path, "--thread", "main"],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "push must succeed; primary should land at <PRIMARY> and mirror default (origin) is best-effort: {:?}",
        result.err()
    );

    // Primary push landed at the heddle target — proving the
    // positional was NOT swallowed by --mirror.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "primary push should land at the positional remote, not be swallowed by --mirror: {}",
        threads
    );
}

/// `--mirror=<name>` parses the explicit value and `--mirror` alone
/// takes the `default_missing_value`. Pins both forms in one test
/// so the parse table is asserted from end to end.
#[test]
fn test_cli_push_mirror_explicit_equals_form_parses_value() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    let mirror_repo = gix::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "explicit eq").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    // Flag-before-positional ordering — the form Codex's finding said
    // the original parse table mishandled.
    let result = heddle(
        &["push", &mirror_arg, "--thread", "main", &weft_path],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "--mirror=<remote> followed by positional must parse cleanly: {:?}",
        result.err()
    );

    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(threads.contains("main"));
    assert!(
        mirror_repo.find_reference("refs/heads/main").is_ok(),
        "mirror push should land at the explicit <git_path>"
    );
}

/// JSON output path on mirror success: covers the `mirrored:true`
/// branch of `render_mirror_outcome`.
#[test]
fn test_cli_push_mirror_json_success_emits_mirrored_true() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    gix::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "json ok").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    let stdout = heddle(
        &[
            "--output",
            "json",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("push --output json --mirror=<remote> must succeed");

    assert!(
        stdout.contains("\"mirrored\":true"),
        "JSON success line missing: {}",
        stdout
    );
    assert!(
        stdout.contains(&git_path),
        "JSON output must echo the mirror remote: {}",
        stdout
    );
}

/// JSON output path on mirror failure: covers the `mirrored:false`
/// + `error` branch of `render_mirror_outcome`.
#[test]
fn test_cli_push_mirror_json_failure_emits_mirrored_false_with_error() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "json err").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let bogus = source
        .path()
        .join("nope-mirror")
        .to_string_lossy()
        .to_string();
    let mirror_arg = format!("--mirror={}", bogus);
    let stdout = heddle(
        &[
            "--output",
            "json",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("primary push must succeed even when mirror push fails");

    assert!(
        stdout.contains("\"mirrored\":false"),
        "JSON failure line missing: {}",
        stdout
    );
    assert!(
        stdout.contains("\"error\""),
        "JSON failure must include error field: {}",
        stdout
    );
}