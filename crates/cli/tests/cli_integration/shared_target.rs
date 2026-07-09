// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage of shared cargo `target/` redirect for
//! solid/materialized threads (default-on for Rust workspaces; P1-A).
//!
//! These tests run the built `heddle` binary inside temp dirs and
//! inspect what `start` writes to disk. They never actually invoke
//! cargo — that would be too slow for a unit test — but they verify
//! that cargo would pick up the redirect by parsing the
//! `.cargo/config.toml` we leave behind.

use super::*;

/// Initialize a tiny Rust workspace inside `dir`. Just enough that
/// shared-target finds a `Cargo.toml` to fingerprint and the default-on
/// heuristic recognizes the workspace as Rust. We intentionally don't
/// write a `Cargo.lock` for the first test; one of the subsequent
/// tests covers the lock-bias fingerprint path.
fn init_rust_workspace(dir: &std::path::Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        b"[workspace]\nresolver = \"2\"\nmembers = []\n",
    )
    .unwrap();
}

#[test]
fn shared_target_writes_cargo_config_pointing_to_shared_dir() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("probe-a");
    heddle(
        &[
            "start",
            "probe-a",
            "--path",
            thread_path.to_str().unwrap(),
            "--shared-target",
        ],
        Some(temp.path()),
    )
    .expect("start --shared-target should succeed in a Rust workspace");

    let cargo_config = thread_path.join(".cargo").join("config.toml");
    assert!(
        cargo_config.is_file(),
        "expected `.cargo/config.toml` at {}",
        cargo_config.display()
    );

    let body = std::fs::read_to_string(&cargo_config).unwrap();
    let parsed: toml::Value = toml::from_str(&body).expect("config.toml is valid TOML");
    let target_dir = parsed
        .get("build")
        .and_then(|t| t.get("target-dir"))
        .and_then(|v| v.as_str())
        .expect("config has [build].target-dir");

    let target_path = std::path::PathBuf::from(target_dir);
    assert!(
        target_path.is_absolute(),
        "shared target dir should be absolute, got {}",
        target_dir
    );
    // Path layout check, not literal string equality: macOS
    // canonicalizes `/var` → `/private/var` for `TempDir` paths, so
    // we assert on the trailing `.heddle/targets/<fingerprint>`
    // structure rather than hard-coding the temp path prefix.
    let parts: Vec<_> = target_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let len = parts.len();
    assert!(
        len >= 3
            && parts[len - 3] == ".heddle"
            && parts[len - 2] == "targets"
            && !parts[len - 1].is_empty(),
        "shared target dir should end in `.heddle/targets/<fingerprint>`, got {}",
        target_dir
    );
    assert!(
        target_path.is_dir(),
        "shared target dir should exist on disk, got {}",
        target_dir
    );
}

#[test]
fn default_shared_target_for_rust_solid_thread_without_flags() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("default-a");
    heddle(
        &["start", "default-a", "--path", thread_path.to_str().unwrap()],
        Some(temp.path()),
    )
    .expect("start without flags should succeed in a Rust workspace");

    let cargo_config = thread_path.join(".cargo").join("config.toml");
    assert!(
        cargo_config.is_file(),
        "Rust solid/materialized threads should default shared-target on; \
         expected `.cargo/config.toml` at {}",
        cargo_config.display()
    );
    let body = std::fs::read_to_string(&cargo_config).unwrap();
    assert!(
        body.contains("[build]") && body.contains("target-dir"),
        "default shared-target config should set [build].target-dir: {body}"
    );
}

#[test]
fn shared_target_uses_same_dir_for_two_threads_in_one_workspace() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let path_a = temp.path().join("probe-a");
    let path_b = temp.path().join("probe-b");

    // Default-on: no flags required for second+ threads to share.
    heddle(
        &["start", "probe-a", "--path", path_a.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["start", "probe-b", "--path", path_b.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();

    let cfg_a = std::fs::read_to_string(path_a.join(".cargo").join("config.toml")).unwrap();
    let cfg_b = std::fs::read_to_string(path_b.join(".cargo").join("config.toml")).unwrap();
    let parse = |body: &str| -> String {
        let parsed: toml::Value = toml::from_str(body).unwrap();
        parsed["build"]["target-dir"].as_str().unwrap().to_string()
    };
    assert_eq!(
        parse(&cfg_a),
        parse(&cfg_b),
        "both threads in one workspace should share the same target dir",
    );
}

#[test]
fn no_shared_target_opts_out_of_cargo_config() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("plain");
    heddle(
        &[
            "start",
            "plain",
            "--path",
            thread_path.to_str().unwrap(),
            "--no-shared-target",
        ],
        Some(temp.path()),
    )
    .unwrap();

    assert!(
        !thread_path.join(".cargo").join("config.toml").exists(),
        "with --no-shared-target, no .cargo/config.toml should be written",
    );
}

#[test]
fn advisory_fires_on_second_thread_with_no_shared_target() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    // First thread: default shared-target on; no advisory.
    let path_a = temp.path().join("first");
    let out_a = heddle_output(
        &["start", "first", "--path", path_a.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();
    assert!(out_a.status.success());
    let stderr_a = std::str::from_utf8(&out_a.stderr).unwrap_or("");
    assert!(
        !stderr_a.contains("without a shared cargo target"),
        "first thread with default shared-target should not advise; got stderr: {stderr_a}"
    );

    // Second thread with opt-out: advisory should fire.
    let path_b = temp.path().join("second");
    let out_b = heddle_output(
        &[
            "start",
            "second",
            "--path",
            path_b.to_str().unwrap(),
            "--no-shared-target",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(out_b.status.success());
    let stderr_b = std::str::from_utf8(&out_b.stderr).unwrap_or("");
    assert!(
        stderr_b.contains("shared cargo target") && stderr_b.contains("second"),
        "second thread with --no-shared-target should advise sharing; got stderr: {stderr_b}"
    );
}

#[test]
fn no_advisory_when_default_shared_target_applies() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    // Prime a first materialized thread so the heuristic has something
    // to count.
    let path_a = temp.path().join("warm-a");
    heddle(
        &["start", "warm-a", "--path", path_a.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();

    // Second thread with default shared-target: no nudge.
    let path_b = temp.path().join("warm-b");
    let out = heddle_output(
        &["start", "warm-b", "--path", path_b.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();
    assert!(out.status.success());
    let stderr = std::str::from_utf8(&out.stderr).unwrap_or("");
    assert!(
        !stderr.contains("without a shared cargo target")
            && !stderr.contains("consider `heddle start --shared-target"),
        "default shared-target should suppress the nudge; got stderr: {stderr}"
    );
}

#[test]
fn no_advisory_in_non_rust_workspace() {
    // No Cargo.toml at the root: the heuristic should leave the
    // user alone regardless of how many threads they have.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let path_a = temp.path().join("non-rust-a");
    heddle(
        &["start", "non-rust-a", "--path", path_a.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();

    let path_b = temp.path().join("non-rust-b");
    let out = heddle_output(
        &["start", "non-rust-b", "--path", path_b.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();
    assert!(out.status.success());
    let stderr = std::str::from_utf8(&out.stderr).unwrap_or("");
    assert!(
        !stderr.contains("shared cargo target") && !stderr.contains("--shared-target"),
        "non-Rust workspaces should never see the advisory; got stderr: {stderr}"
    );
}

/// `--shared-target` in a non-Rust repo (no top-level `Cargo.toml`)
/// must be a harmless no-op rather than an error: automation that
/// passes the flag unconditionally across mixed-language repos
/// shouldn't have to special-case every non-cargo project.
#[test]
fn shared_target_is_noop_in_non_rust_repo() {
    let temp = TempDir::new().unwrap();
    // Deliberately do NOT write a Cargo.toml.
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("non-rust");
    heddle(
        &[
            "start",
            "non-rust",
            "--path",
            thread_path.to_str().unwrap(),
            "--shared-target",
        ],
        Some(temp.path()),
    )
    .expect("start --shared-target must not error in a non-Rust repo");

    assert!(
        !thread_path.join(".cargo").join("config.toml").exists(),
        "no .cargo/config.toml should be written when there is no Cargo.toml at root"
    );

    // The flag was silently dropped, so `thread show` must not
    // report a `shared_target_dir` either.
    let json = heddle(
        &["--output", "json", "thread", "show", "non-rust"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let surfaced = parsed
        .get("shared_target_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    assert!(
        surfaced.is_none(),
        "non-Rust repo should not advertise a shared target dir: {json}"
    );
}

/// When the materialized checkout already contains a
/// `.cargo/config.toml` (because the source tree captured one),
/// `write_cargo_config` preserves it and emits a loud warning.
/// `shared_target_dir` is `None` when the writer was a no-op.
#[test]
fn shared_target_dir_unset_when_user_config_preserved() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    // Capture a `.cargo/config.toml` in the source tree. This file
    // will materialize into every thread checkout, beating the
    // shared-target writer to the punch — exercising the no-op
    // branch.
    let user_config = "[net]\noffline = true\n";
    let cargo_dir = temp.path().join(".cargo");
    std::fs::create_dir_all(&cargo_dir).unwrap();
    std::fs::write(cargo_dir.join("config.toml"), user_config).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("preconfigured");
    let out = heddle_output(
        &[
            "start",
            "preconfigured",
            "--path",
            thread_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start should succeed when materialized config exists");
    assert!(out.status.success());
    let stderr = std::str::from_utf8(&out.stderr).unwrap_or("");
    assert!(
        stderr.contains("shared cargo target redirect not applied")
            && stderr.contains("config.toml"),
        "blocked redirect must warn loudly on stderr; got: {stderr}"
    );

    // The user's config must survive verbatim — write_cargo_config
    // must NOT clobber it.
    let after = std::fs::read_to_string(thread_path.join(".cargo").join("config.toml")).unwrap();
    assert_eq!(
        after, user_config,
        "user-managed cargo config must be preserved"
    );

    // And the thread record must NOT advertise a shared dir, since
    // none is in effect.
    let json = heddle(
        &["--output", "json", "thread", "show", "preconfigured"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let surfaced = parsed
        .get("shared_target_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    assert!(
        surfaced.is_none(),
        "shared_target_dir must be absent when write_cargo_config was a no-op: {json}"
    );
}

#[test]
fn shared_target_dir_surfaces_in_thread_show_json() {
    let temp = TempDir::new().unwrap();
    init_rust_workspace(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main").unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("inspect");
    // Default-on: no flag required.
    heddle(
        &["start", "inspect", "--path", thread_path.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();

    let json = heddle(
        &["--output", "json", "thread", "show", "inspect"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let surfaced = parsed.get("shared_target_dir").and_then(|v| v.as_str());
    assert!(
        surfaced.is_some(),
        "thread show JSON should expose `shared_target_dir`: {json}"
    );
    let dir = surfaced.unwrap();
    assert!(
        dir.contains(".heddle"),
        "shared_target_dir should sit under .heddle: got {dir}"
    );
}
