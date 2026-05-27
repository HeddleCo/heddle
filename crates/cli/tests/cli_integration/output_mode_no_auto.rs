// SPDX-License-Identifier: Apache-2.0
//! `--output auto` and `output.format = "auto"` are gone — regression cover.
//!
//! Background: PR #251 fixed the *default* surface (no flag → text regardless
//! of pipe state) but left the `--output auto` value and its
//! TTY-detect-then-pick behaviour in place. Round 5 of the persona eval
//! confirmed that under a config with `output.format = "auto"`,
//! `heddle status > file` still emitted JSON — the exact ergonomic surprise
//! we tried to delete. Heddle #271 deletes `auto` entirely from both the
//! CLI surface and the repo/user config; this file pins the contract.
//!
//! No backcompat alias. Pre-1.0; legacy configs error loudly with a typed
//! `Next:` envelope rather than silently mapping to text.

use std::{process::Command, str};

use serde_json::Value;
use tempfile::TempDir;

use super::{assert_json_recovery_advice_fields, heddle, heddle_output};

#[test]
fn piped_status_with_no_output_flag_renders_text() {
    // Post-PR251 default — re-asserted here because the failure mode this
    // regression-protects (silent JSON on a pipe) was the entire reason
    // #271 exists. `Command::output()` always pipes stdout/stderr, so
    // `heddle_output` is already the "piped" shape.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init should succeed");

    let output =
        heddle_output(&["status"], Some(temp.path())).expect("status with no --output flag");
    assert!(
        output.status.success(),
        "default status under a pipe should succeed: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = str::from_utf8(&output.stdout).expect("stdout utf8");
    let trimmed = stdout.trim_start();
    assert!(
        !trimmed.starts_with('{') && !trimmed.starts_with('['),
        "piped default status must stay text, not JSON: {stdout}"
    );
    assert!(
        serde_json::from_str::<Value>(stdout).is_err(),
        "piped default status must not parse as JSON: {stdout}"
    );
}

#[test]
fn piped_status_with_explicit_json_flag_emits_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init should succeed");

    let stdout = heddle(&["status", "--output", "json"], Some(temp.path()))
        .expect("status --output json should succeed under a pipe");
    let parsed: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("JSON parse failed: {err}: {stdout}"));
    assert!(
        parsed.is_object(),
        "status --output json should emit a JSON object: {stdout}"
    );
}

#[test]
fn output_auto_flag_errors_at_parse_with_helpful_message() {
    // `--output auto` is gone. clap should reject it at parse time with a
    // value-list hint that names the remaining valid values.
    let output = heddle_output(&["--output", "auto", "status"], None)
        .expect("heddle should run even when args reject");
    assert!(
        !output.status.success(),
        "--output auto should fail at parse: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lower = stderr.to_ascii_lowercase();
    assert!(
        stderr_lower.contains("auto"),
        "parse error should name the rejected value 'auto': {stderr}"
    );
    assert!(
        stderr_lower.contains("text") && stderr_lower.contains("json"),
        "parse error should list the remaining valid values 'text' and 'json': {stderr}"
    );
}

#[test]
fn repo_config_with_output_format_auto_errors_with_typed_envelope() {
    // Loud error, typed envelope. The repo config file is the place a
    // legacy `auto` value can still appear in the wild (anyone who copied
    // the example config from before the rip will have it). We refuse to
    // silently map it to text — that's the bug class #271 closes.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init should succeed");

    let config_path = temp.path().join(".heddle").join("config.toml");
    let existing = std::fs::read_to_string(&config_path).expect("repo config exists after init");
    // `init` writes an empty `[output]` section. Inject `format = "auto"`
    // into it so the toml parser hits the rejection path.
    let mutated = existing.replace("[output]\n", "[output]\nformat = \"auto\"\n");
    assert_ne!(
        mutated, existing,
        "test fixture should mutate the [output] section: {existing}"
    );
    std::fs::write(&config_path, &mutated).expect("write mutated config");

    // Any command that loads the repo config exercises the parse path.
    // Use `status` because it's the one R5/A1 personas hit on the JSON-pipe
    // bug.
    let text_out =
        heddle_output(&["status"], Some(temp.path())).expect("status should run with bad config");
    assert!(
        !text_out.status.success(),
        "status with output.format='auto' must fail loudly: stdout={}; stderr={}",
        String::from_utf8_lossy(&text_out.stdout),
        String::from_utf8_lossy(&text_out.stderr)
    );
    let stderr = String::from_utf8_lossy(&text_out.stderr);
    assert!(
        stderr.contains("output.format") && stderr.contains("'auto'"),
        "text envelope should name the field and the rejected value: {stderr}"
    );
    assert!(
        stderr.contains("'text'") && stderr.contains("'json'"),
        "text envelope should list the valid values: {stderr}"
    );
    assert!(
        stderr.contains("Next:"),
        "text envelope should carry a typed Next: line: {stderr}"
    );

    // JSON envelope path. Same failure must surface a structured body
    // with the recovery-advice fields the rest of the CLI envelope
    // contract requires.
    let json_out = heddle_output(&["--output", "json", "status"], Some(temp.path()))
        .expect("status --output json should run with bad config");
    assert!(
        !json_out.status.success(),
        "status with output.format='auto' must fail under --output json too"
    );
    let stderr_json_text = String::from_utf8_lossy(&json_out.stderr);
    let last_line = stderr_json_text
        .lines()
        .rfind(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected a JSON envelope on stderr; got: {stderr_json_text}"));
    let envelope: Value = serde_json::from_str(last_line.trim()).unwrap_or_else(|err| {
        panic!("stderr JSON envelope should parse: {err}: {last_line}")
    });
    assert_eq!(
        envelope["kind"], "invalid_repo_config_output_format",
        "JSON envelope kind should classify the field-specific failure: {envelope}"
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|err| err.contains("output.format") && err.contains("'auto'")),
        "JSON envelope error message should name field and value: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, "repo config output.format=auto");
}

#[test]
fn user_config_with_output_format_auto_via_heddle_config_env_errors_with_typed_envelope() {
    // Codex R2: the deserializer rejection only produced a typed envelope
    // when `output.format = "auto"` lived in `.heddle/config.toml`. The
    // same value in the GLOBAL user config (`HEDDLE_CONFIG` / `~/.config`)
    // hit a parse failure during `UserConfig::load_default` in `main`
    // before the error printer was wired, so every command exited with
    // a raw TOML parse error instead of the promised `Next:` envelope.
    // This test pins the contract for the `HEDDLE_CONFIG` route.
    let temp = TempDir::new().unwrap();
    let bad_user_config = temp.path().join("user-config.toml");
    std::fs::write(&bad_user_config, "[output]\nformat = \"auto\"\n")
        .expect("write bad user config");

    let text_out = run_with_bad_user_config(&bad_user_config, None, &["status"]);
    assert!(
        !text_out.status.success(),
        "status with user output.format='auto' must fail loudly: stdout={}; stderr={}",
        String::from_utf8_lossy(&text_out.stdout),
        String::from_utf8_lossy(&text_out.stderr)
    );
    let stderr = String::from_utf8_lossy(&text_out.stderr);
    assert_typed_output_format_envelope(&stderr, "HEDDLE_CONFIG user config");

    let json_out =
        run_with_bad_user_config(&bad_user_config, None, &["--output", "json", "status"]);
    assert!(
        !json_out.status.success(),
        "status --output json with user output.format='auto' must fail too: stderr={}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let envelope = parse_envelope(&json_out.stderr);
    assert_eq!(
        envelope["kind"], "invalid_repo_config_output_format",
        "JSON envelope kind should classify the field-specific failure: {envelope}"
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|err| err.contains("output.format") && err.contains("'auto'")),
        "JSON envelope error message should name field and value: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, "user config HEDDLE_CONFIG output.format=auto");
}

#[test]
fn user_config_with_output_format_auto_via_home_path_errors_with_typed_envelope() {
    // Mirror case: the same failure must surface a typed envelope when
    // the user config is discovered via `$HOME/.config/heddle/config.toml`
    // (the no-env-var fallback). Without `HEDDLE_CONFIG` and
    // `XDG_CONFIG_HOME` overrides, `UserConfig::default_path` walks down
    // to the HOME-based path; we set HOME explicitly so the test does
    // not depend on the developer's real `~/.config/heddle/`.
    let temp = TempDir::new().unwrap();
    let fake_home = temp.path();
    let config_path = fake_home.join(".config").join("heddle").join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir config parent");
    std::fs::write(&config_path, "[output]\nformat = \"auto\"\n")
        .expect("write bad home config");

    let text_out = run_with_home_user_config(fake_home, None, &["status"]);
    assert!(
        !text_out.status.success(),
        "status with HOME user output.format='auto' must fail loudly: stdout={}; stderr={}",
        String::from_utf8_lossy(&text_out.stdout),
        String::from_utf8_lossy(&text_out.stderr)
    );
    let stderr = String::from_utf8_lossy(&text_out.stderr);
    assert_typed_output_format_envelope(&stderr, "HOME-based user config");
}

fn run_with_bad_user_config(
    config_path: &std::path::Path,
    cwd: Option<&std::path::Path>,
    args: &[&str],
) -> std::process::Output {
    let temp;
    let dir = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => {
            temp = TempDir::new().expect("tempdir for cwd");
            temp.path().to_path_buf()
        }
    };
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(&dir)
        .env("HEDDLE_CONFIG", config_path)
        .output()
        .expect("spawn heddle")
}

fn run_with_home_user_config(
    home: &std::path::Path,
    cwd: Option<&std::path::Path>,
    args: &[&str],
) -> std::process::Output {
    let temp;
    let dir = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => {
            temp = TempDir::new().expect("tempdir for cwd");
            temp.path().to_path_buf()
        }
    };
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(&dir)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", home)
        .output()
        .expect("spawn heddle")
}

fn assert_typed_output_format_envelope(stderr: &str, context: &str) {
    assert!(
        stderr.contains("output.format") && stderr.contains("'auto'"),
        "{context}: text envelope should name the field and the rejected value: {stderr}"
    );
    assert!(
        stderr.contains("'text'") && stderr.contains("'json'"),
        "{context}: text envelope should list the valid values: {stderr}"
    );
    assert!(
        stderr.contains("Next:"),
        "{context}: text envelope should carry a typed Next: line: {stderr}"
    );
    assert!(
        !stderr.contains("TOML parse error"),
        "{context}: raw TOML parse error must not leak past the typed envelope: {stderr}"
    );
}

fn parse_envelope(stderr_bytes: &[u8]) -> Value {
    let stderr = String::from_utf8_lossy(stderr_bytes);
    let line = stderr
        .lines()
        .rfind(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected JSON envelope on stderr; got: {stderr}"));
    serde_json::from_str(line.trim())
        .unwrap_or_else(|err| panic!("stderr JSON envelope should parse: {err}: {line}"))
}

#[test]
fn output_mode_auto_variant_is_absent_from_source() {
    // Belt-and-braces grep: if anyone re-adds `OutputMode::Auto` or
    // `OutputFormat::Auto`, this test fails before runtime behaviour does.
    // We scan the cli + repo source trees relative to CARGO_MANIFEST_DIR.
    let cli_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let repo_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .join("repo")
        .join("src");
    for root in [cli_src, repo_src] {
        for entry in walkdir(&root) {
            if entry.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let contents = std::fs::read_to_string(&entry).expect("read rs file");
            assert!(
                !contents.contains("OutputMode::Auto"),
                "{} still references OutputMode::Auto",
                entry.display()
            );
            assert!(
                !contents.contains("OutputFormat::Auto"),
                "{} still references OutputFormat::Auto",
                entry.display()
            );
        }
    }
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
