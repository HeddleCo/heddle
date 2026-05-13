// SPDX-License-Identifier: Apache-2.0
use std::fs;

use tempfile::TempDir;

use super::*;

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