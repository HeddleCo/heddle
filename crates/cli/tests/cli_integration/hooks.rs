// SPDX-License-Identifier: Apache-2.0
use std::fs;

use tempfile::TempDir;

use super::*;

fn warm_native_monitor(root: &std::path::Path) {
    let envs = [("HEDDLE_FSMONITOR", "native")];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut last_monitor = Value::Null;

    while std::time::Instant::now() < deadline {
        heddle_env(&["status", "--short"], Some(root), &envs).unwrap();
        let inspect = heddle_env(
            &["maintenance", "inspect", "--output", "json"],
            Some(root),
            &envs,
        )
        .unwrap();
        let report: Value = serde_json::from_str(&inspect).unwrap();
        last_monitor = report["change_monitor"].clone();
        if last_monitor["backend"] == "native-helper" && last_monitor["status"] == "usable" {
            heddle_env(&["status", "--short"], Some(root), &envs).unwrap();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    panic!("native monitor did not become usable; last monitor: {last_monitor}");
}

#[cfg(unix)]
#[test]
#[serial_test::serial(fsmonitor)]
fn pre_snapshot_hook_changes_are_captured_with_native_and_off_fsmonitor() {
    for mode in ["native", "off"] {
        let temp = TempDir::new().unwrap();
        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(temp.path().join("a.txt"), "baseline a\n").unwrap();
        fs::write(temp.path().join("b.txt"), "baseline b\n").unwrap();
        heddle(
            &["capture", "-m", "baseline before pre-snapshot hook"],
            Some(temp.path()),
        )
        .unwrap();

        let repo = Repository::open(temp.path()).unwrap();
        let mut config = repo.config().clone();
        config.worktree.fsmonitor.mode = match mode {
            "native" => repo::FsMonitorMode::Native,
            "off" => repo::FsMonitorMode::Off,
            _ => unreachable!(),
        };
        config.save(&repo.heddle_dir().join("config.toml")).unwrap();
        drop(repo);

        if mode == "native" {
            warm_native_monitor(temp.path());
        }

        fs::write(temp.path().join("a.txt"), "user change\n").unwrap();
        if mode == "native" {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let hook_script = "#!/bin/sh\nif [ -z \"$HEDDLE_HOOK_PROTOCOL\" ]; then\n  printf 'hook change\\n' > b.txt\nfi\n";
        let install = heddle_output_with_stdin(
            &["hook", "install", "pre-snapshot", "--from-stdin"],
            temp.path(),
            hook_script,
        )
        .unwrap();
        assert!(
            install.status.success(),
            "install hook for {mode}: {}",
            String::from_utf8_lossy(&install.stderr)
        );

        let envs = [("HEDDLE_FSMONITOR", mode)];
        let capture = heddle_output_env(
            &[
                "--output",
                "json",
                "capture",
                "-m",
                "capture user and hook changes",
            ],
            Some(temp.path()),
            &envs,
        )
        .unwrap();
        assert!(
            capture.status.success(),
            "capture with {mode}: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
        let report: Value = serde_json::from_slice(&capture.stdout).unwrap();
        assert_eq!(
            report["captured_path_count"], 2,
            "capture with {mode} must include the path changed by the hook: {report}"
        );

        let authoritative = heddle_env(
            &["--output", "json", "status"],
            Some(temp.path()),
            &[("HEDDLE_FSMONITOR", "off")],
        )
        .unwrap();
        let authoritative: Value = serde_json::from_str(&authoritative).unwrap();
        for kind in ["added", "modified", "deleted"] {
            assert_eq!(
                authoritative["changes"][kind],
                serde_json::json!([]),
                "authoritative fsmonitor-off status after {mode} capture must be clean: {authoritative}"
            );
        }
    }
}

#[test]
fn hook_install_reads_script_from_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let script_path = temp.path().join("hook.sh");
    fs::write(&script_path, "#!/bin/sh\necho from file\n").unwrap();

    heddle(
        &[
            "hook",
            "install",
            "pre-snapshot",
            "--from-file",
            script_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .unwrap();

    let installed = fs::read_to_string(temp.path().join(".heddle/hooks/pre-snapshot")).unwrap();
    assert_eq!(installed, "#!/bin/sh\necho from file\n");
}

#[test]
fn hook_install_reads_script_from_stdin() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_stdin(
        &["hook", "install", "pre-push", "--from-stdin"],
        temp.path(),
        "#!/bin/sh\necho from stdin\n",
    )
    .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = fs::read_to_string(temp.path().join(".heddle/hooks/pre-push")).unwrap();
    assert_eq!(installed, "#!/bin/sh\necho from stdin\n");
}

#[test]
fn hook_install_without_file_or_input_fails() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(&["hook", "install", "pre-snapshot"], Some(temp.path())).unwrap();
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hook install requires --from-file <path> or stdin input")
            || stderr.contains("received empty stdin"),
        "stderr was {stderr}"
    );
}

#[test]
fn hook_install_empty_stdin_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_stdin(
        &[
            "--output",
            "json",
            "hook",
            "install",
            "pre-push",
            "--from-stdin",
        ],
        temp.path(),
        "",
    )
    .expect("invoke empty stdin hook install");
    assert!(!output.status.success(), "empty hook stdin should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode hook install refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("empty hook stdin should emit JSON envelope");
    assert_eq!(envelope["kind"], "hook_install_empty_stdin");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("hook install received empty stdin")),
        "empty hook stdin should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--from-file")),
        "empty hook stdin hint should name a script source: {stderr}"
    );
}

#[test]
fn pre_capture_hook_veto_uses_typed_advice_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("blocked.txt"), "blocked").unwrap();

    let output = heddle_output_with_stdin(
        &["hook", "install", "pre-snapshot", "--from-stdin"],
        temp.path(),
        "#!/bin/sh\nif [ \"$HEDDLE_HOOK_PROTOCOL\" = \"json\" ]; then\n  printf '{\"abort\":\"policy says no\"}\\n'\nfi\n",
    )
    .unwrap();
    assert!(
        output.status.success(),
        "install stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = heddle_output(
        &["--output", "json", "capture", "-m", "blocked"],
        Some(temp.path()),
    )
    .expect("invoke hook-vetoed capture");
    assert!(!output.status.success(), "hook veto should fail capture");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode hook veto must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("hook veto should emit JSON envelope");
    assert_eq!(envelope["kind"], "hook_veto");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("pre_capture hook vetoed: policy says no")),
        "hook veto should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle hook list")),
        "hook veto hint should name hook inspection: {stderr}"
    );
}
