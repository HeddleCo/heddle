// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn git_overlay_guide_is_concise_and_actionable() {
    let help = heddle(&["help", "git-overlay"], None).unwrap();
    assert!(
        help.contains("Show the low-friction Git-overlay workflow"),
        "help should discover the guide command: {help}"
    );

    let output = heddle(&["--output", "text", "git-overlay"], None).unwrap();

    assert!(
        output.contains("Git-overlay quick start"),
        "guide should have a clear title: {output}"
    );
    assert!(
        output.contains("heddle bridge git import --ref <branch>"),
        "guide should teach scoped import using the real verb path: {output}"
    );
    assert!(
        output.contains("heddle start <topic> --path ../<topic>"),
        "guide should teach isolated threads: {output}"
    );
    assert!(
        output.contains("heddle doctor"),
        "guide should point to doctor for recovery: {output}"
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
        text.contains("Health: dirty_worktree"),
        "doctor should show health: {text}"
    );
    assert!(
        text.contains("Next step: heddle capture"),
        "doctor should provide one primary recovery command: {text}"
    );
    assert!(
        !text.contains("Next:"),
        "doctor should use the newer next-step label: {text}"
    );

    let json = heddle(&["doctor", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("doctor JSON should parse");
    assert_eq!(parsed["health"]["recommended_action"], "heddle capture");
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
        text.contains("Git:"),
        "verbose version should show Git availability: {text}"
    );
    assert!(
        text.contains("Repository:"),
        "verbose version should show repository capability: {text}"
    );

    let json = heddle(&["version", "--verbose", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("version JSON should parse");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert!(parsed["features"].as_array().is_some());
}

#[test]
fn heavy_thread_start_explains_non_empty_workspace_recovery() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let target = temp.path().join("already-used");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("draft.txt"), "uncaptured").unwrap();
    let error = heddle(
        &[
            "start",
            "ux-thread",
            "--path",
            target.to_str().expect("path should be utf8"),
        ],
        Some(temp.path()),
    )
    .expect_err("non-empty heavy worktree should fail with guidance");

    assert!(
        error.contains("is not empty")
            && error.contains("heddle capture")
            && error.contains("heddle start --workspace heavy"),
        "thread start should give premium recovery guidance: {error}"
    );
}

#[test]
fn thread_list_groups_threads_by_user_workflow() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("feature-work");
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
}

#[test]
fn json_flag_emits_deprecation_warning_and_still_renders_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["status", "--json"], Some(temp.path())).unwrap();
    assert!(output.status.success(), "status --json should succeed");

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout should still be JSON when --json is passed: {stdout}"
    );
    assert!(
        stderr.contains("--json is deprecated"),
        "stderr should carry the deprecation hint: {stderr}"
    );
    assert!(
        stderr.contains("use --output json"),
        "stderr should suggest the replacement flag: {stderr}"
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
        stderr.contains("Hint:") && stderr.contains("heddle init"),
        "stderr should suggest `heddle init`: {stderr}"
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
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim()).expect(&format!(
        "stderr should be a single-line JSON envelope: {stderr}"
    ));
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
            .contains("heddle init"),
        "envelope.hint should suggest heddle init: {envelope}"
    );
}

#[test]
fn global_flags_only_renders_curated_help_not_clap_error() {
    // The user typed `heddle --output text` with no subcommand. Without the
    // intercept, clap would dump a 60+ verb wall of text. With it, the
    // curated everyday-verb help renders cleanly.
    let output = heddle_output(&["--output", "text"], None).expect("invoke heddle");
    assert!(
        output.status.success(),
        "global-flags-only invocation should print help and exit 0"
    );

    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stdout.contains("Heddle") && stdout.contains("Everyday commands:"),
        "curated help should render: stdout={stdout}"
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
        output.contains("Run this to switch shells:"),
        "text-mode start should suggest the cd command: {output}"
    );
    assert!(
        output.contains("    cd "),
        "the cd hint should include the literal `cd` invocation: {output}"
    );
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
fn unknown_state_id_hints_at_heddle_log() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "text", "goto", "hd-nonexistent"],
        Some(temp.path()),
    )
    .expect("invoke heddle goto");
    assert!(
        !output.status.success(),
        "goto on a missing state should exit non-zero"
    );

    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("State not found"),
        "stderr should carry the original error: {stderr}"
    );
    assert!(
        stderr.contains("Hint:") && stderr.contains("heddle log"),
        "stderr should suggest `heddle log`: {stderr}"
    );
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
        stderr.contains("Thread not found"),
        "stderr should carry the original error: {stderr}"
    );
    assert!(
        stderr.contains("Hint:") && stderr.contains("heddle thread list"),
        "stderr should suggest `heddle thread list`: {stderr}"
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
fn context_get_honors_user_config_principal_not_unknown() {
    // Regression: `heddle context set` / `context get` used to route through
    // `repo.get_attribution()`, which only consults env + repo config.
    // A user with `[principal]` only in `~/.config/heddle/config.toml` saw
    // every annotation surface as `Unknown <unknown@example.com>`. After
    // the migration to `resolve_attribution`, the user-config principal
    // wins as it does for `heddle capture`.
    let temp = TempDir::new().unwrap();
    let user_cfg_dir = temp.path().join(".heddle-user");
    std::fs::create_dir_all(&user_cfg_dir).unwrap();
    std::fs::write(
        user_cfg_dir.join("config.toml"),
        "[principal]\nname = \"Ada\"\nemail = \"ada@example.com\"\n",
    )
    .unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
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
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "context", "get", "--path", "main.rs"],
        Some(temp.path()),
    )
    .expect("context get");
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
fn error_envelope_schema_is_registered_and_matches_runtime_shape() {
    // The error envelope is the stderr contract for JSON-mode failures.
    // `heddle schemas error` returns its mirror schema; the fields it
    // declares MUST match what `print_error_with_hint` actually emits.
    let schema = heddle(&["schemas", "error"], None).expect("heddle schemas error");
    let parsed: serde_json::Value = serde_json::from_str(&schema).expect("schema parses");
    let props = parsed["properties"]
        .as_object()
        .expect("schema has properties");
    for field in ["error", "hint", "kind"] {
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
    assert!(
        required.contains(&"error"),
        "`error` must be required: {schema}"
    );
    assert!(
        required.contains(&"hint"),
        "`hint` must be required: {schema}"
    );
    assert!(
        required.contains(&"kind"),
        "`kind` must be required: {schema}"
    );

    // And the runtime really emits this shape: trigger a known failure
    // class and parse the stderr envelope.
    let temp = TempDir::new().unwrap();
    let output = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("invoke heddle status");
    assert!(!output.status.success());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr is a JSON object");
    for field in ["error", "hint", "kind"] {
        assert!(
            envelope.get(field).is_some(),
            "envelope must carry `{field}` field per the schema: {stderr}"
        );
    }
    assert_eq!(envelope["kind"], "repository_not_found");
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
fn blame_drops_email_when_attribution_overflows_column() {
    // `Luke Thorne <the.thorne48@gmail.com>` blew the 20-char column,
    // truncating to `Luke Thorne <the...` — keeping the noise and
    // dropping the signal. The fit_author helper drops the email
    // entirely when the name alone fits the column.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join(".heddle-user/config.toml"),
        "[principal]\nname = \"Ada Lovelace\"\nemail = \"ada@really.long.example.com\"\n",
    )
    .unwrap_or(()); // best-effort; harness already wrote a config we'll override
    let cfg_dir = temp.path().join(".heddle-user");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[principal]\nname = \"Ada Lovelace\"\nemail = \"ada@really.long.example.com\"\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("note.txt"), "first line\nsecond line\n").unwrap();
    heddle(
        &["capture", "-m", "seed", "--confidence", "0.9"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "blame", "note.txt"],
        Some(temp.path()),
    )
    .expect("blame note.txt");
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
fn freshly_initialized_repo_reports_clean_health() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Health: clean"),
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
        json.contains(r#""recommended_action":"""#),
        "fresh-init JSON should expose an empty recommended_action: {json}"
    );
}