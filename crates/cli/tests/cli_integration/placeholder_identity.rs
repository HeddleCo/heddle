use std::fs;

use tempfile::TempDir;

use super::heddle_output_with_env;

fn write_user_config(dir: &TempDir, name: &str, email: &str) -> std::path::PathBuf {
    let config = dir.path().join("user-config.toml");
    fs::write(
        &config,
        format!("[principal]\nname = \"{name}\"\nemail = \"{email}\"\n"),
    )
    .expect("write user config");
    config
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn init_warns_when_user_config_principal_is_placeholder() {
    let temp = TempDir::new().unwrap();
    let config = write_user_config(&temp, "T", "t@e.c");

    let output = heddle_output_with_env(
        &["init"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", config.to_str().unwrap())],
    )
    .expect("run init");

    assert!(output.status.success(), "init should warn, not fail");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("WARNING: principal attribution looks like a placeholder")
            && stderr.contains("T <t@e.c>")
            && stderr.contains("heddle init --principal-name <name> --principal-email <email>"),
        "init should explain the placeholder identity and fix: {stderr}"
    );
}

#[test]
fn placeholder_warning_is_not_repeated_after_init() {
    let temp = TempDir::new().unwrap();
    let config = write_user_config(&temp, "T", "t@e.c");
    let env = [("HEDDLE_CONFIG", config.to_str().unwrap())];

    let init = heddle_output_with_env(&["init"], Some(temp.path()), &env).expect("run init");
    assert!(init.status.success(), "init should succeed");

    fs::write(temp.path().join("one.txt"), "one\n").expect("write first file");
    let first = heddle_output_with_env(&["capture", "-m", "first"], Some(temp.path()), &env)
        .expect("run first capture");
    assert!(first.status.success(), "first capture should succeed");
    let first_stderr = stderr(&first);
    assert!(
        !first_stderr.contains("principal attribution looks like a placeholder"),
        "init already surfaced the warning; capture should not repeat it: {first_stderr}"
    );

    fs::write(temp.path().join("two.txt"), "two\n").expect("write second file");
    let second = heddle_output_with_env(&["capture", "-m", "second"], Some(temp.path()), &env)
        .expect("run second capture");
    assert!(
        second.status.success(),
        "second capture should still succeed"
    );
    let second_stderr = stderr(&second);
    assert!(
        !second_stderr.contains("principal attribution looks like a placeholder"),
        "later captures should not spam the placeholder warning: {second_stderr}"
    );
}

/// A *corrupt* (un-parseable) user config must FAIL CLOSED on the
/// identity-bearing capture path, not silently fall back to the
/// `Unknown <unknown@example.com>` default and mis-attribute the
/// snapshot. `UserConfig::load_default()` already maps a *missing* file
/// to the default; only a malformed file produces an Err, and the
/// capture path now propagates it with `?` instead of
/// `.unwrap_or_default()`.
#[test]
fn capture_fails_closed_on_corrupt_user_config() {
    let temp = TempDir::new().unwrap();

    // Bootstrap the repo with a VALID config so `init` succeeds — we
    // want to isolate the capture/identity path, not fail at init.
    let good_config = write_user_config(&temp, "Ada Lovelace", "ada@users.test");
    let init = heddle_output_with_env(
        &["init"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", good_config.to_str().unwrap())],
    )
    .expect("run init");
    assert!(
        init.status.success(),
        "init should succeed: {}",
        stderr(&init)
    );

    // Now corrupt the user config and capture: the identity load must
    // error instead of attributing the snapshot to the Unknown default.
    let bad_config = temp.path().join("corrupt-config.toml");
    fs::write(&bad_config, "principal = [broken\n").expect("write corrupt config");

    fs::write(temp.path().join("file.txt"), "content\n").expect("write file");
    let output = heddle_output_with_env(
        &["capture", "-m", "should-not-attribute-to-unknown"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", bad_config.to_str().unwrap())],
    )
    .expect("run capture");

    assert!(
        !output.status.success(),
        "capture must fail on a corrupt user config, not fall back to Unknown"
    );
    let err = stderr(&output);
    let combined = format!("{}{}", String::from_utf8_lossy(&output.stdout), err);
    assert!(
        !combined.contains("unknown@example.com"),
        "corrupt config must not produce an Unknown-attributed snapshot: {combined}"
    );
    assert!(
        err.to_lowercase().contains("config") || err.contains("corrupt-config.toml"),
        "error should explain the config parse failure: {err}"
    );
}

#[test]
fn real_user_config_principal_does_not_warn_on_init_or_first_capture() {
    let temp = TempDir::new().unwrap();
    let config = write_user_config(&temp, "Ada Lovelace", "ada@users.test");
    let env = [("HEDDLE_CONFIG", config.to_str().unwrap())];

    let init = heddle_output_with_env(&["init"], Some(temp.path()), &env).expect("run init");
    assert!(init.status.success(), "init should succeed");
    let init_stderr = stderr(&init);
    assert!(
        !init_stderr.contains("principal attribution looks like a placeholder"),
        "real init identity should not warn: {init_stderr}"
    );

    fs::write(temp.path().join("file.txt"), "content\n").expect("write file");
    let capture = heddle_output_with_env(&["capture", "-m", "first"], Some(temp.path()), &env)
        .expect("run first capture");
    assert!(capture.status.success(), "capture should succeed");
    let capture_stderr = stderr(&capture);
    assert!(
        !capture_stderr.contains("principal attribution looks like a placeholder"),
        "real capture identity should not warn: {capture_stderr}"
    );
}
