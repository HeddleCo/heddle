// SPDX-License-Identifier: Apache-2.0
//! heddle#2034: a `.heddle` that is checked-out content of an enclosing
//! worktree must not take over discovery. Commands run inside it refuse with
//! a typed envelope; `[safe] repositories` in the user config opts in.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use tempfile::TempDir;

use super::{assert_json_recovery_advice_fields, heddle, heddle_output, heddle_output_with_env};

const ATTACKER_UPSTREAM: &str = "https://attacker.example.test";

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create copy target");
    for entry in fs::read_dir(from).expect("read copy source") {
        let entry = entry.expect("copy entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// `<tmp>/outer` is the user's repository; `outer/sub/.heddle` is a complete
/// hostile repository planted as ordinary content, with a redirecting
/// upstream, its own CA, and a pre-push hook.
fn enclosing_with_planted_repo() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let outer = temp.path().join("outer");
    let source = temp.path().join("hostile-source");
    fs::create_dir_all(&outer).expect("create outer");
    fs::create_dir_all(&source).expect("create source");
    heddle(&["init"], Some(&outer)).expect("init outer");
    heddle(&["init"], Some(&source)).expect("init hostile source");

    let config_path = source.join(".heddle/config.toml");
    let config = fs::read_to_string(&config_path).expect("read hostile config");
    assert!(config.contains("[hosted]\n"), "{config}");
    let config = config.replace(
        "[hosted]\n",
        &format!("[hosted]\nupstream_url = \"{ATTACKER_UPSTREAM}\"\n"),
    ) + "\n[remote]\ntls_ca_certificate_path = \"attacker-ca.pem\"\n";
    fs::write(&config_path, config).expect("write hostile config");
    fs::create_dir_all(source.join(".heddle/hooks")).expect("create hooks");
    fs::write(
        source.join(".heddle/hooks/pre-push"),
        "#!/bin/sh\necho pwned\n",
    )
    .expect("write hook");

    let planted = outer.join("sub");
    copy_tree(&source.join(".heddle"), &planted.join(".heddle"));
    fs::write(planted.join("README.md"), "fixture\n").expect("write fixture file");
    fs::write(planted.join("attacker-ca.pem"), "not a CA\n").expect("write fixture CA");
    let outer = outer.canonicalize().expect("canonical outer");
    let planted = planted.canonicalize().expect("canonical planted");
    (temp, outer, planted)
}

fn error_envelope(output: &std::process::Output, context: &str) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("{context}: envelope not JSON: {err}\n  stderr: {stderr}"))
}

#[test]
fn commands_inside_planted_nested_repository_refuse_it() {
    let (_temp, outer, planted) = enclosing_with_planted_repo();
    let oplog_before = fs::read(planted.join(".heddle/oplog/oplog.bin")).ok();

    for args in [
        &["--output", "json", "status"][..],
        &["--output", "json", "capture", "-m", "planted"][..],
    ] {
        let output = heddle_output(args, Some(&planted)).expect("spawn heddle");
        assert_eq!(
            output.status.code(),
            Some(78),
            "{args:?} must refuse the planted repository; stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope = error_envelope(&output, &format!("{args:?}"));
        assert_eq!(envelope["kind"], "untrusted_repository", "{envelope}");
        assert_json_recovery_advice_fields(&envelope, "untrusted repository refusal");
        let hint = envelope["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("[safe] repositories"), "{envelope}");
        assert!(hint.contains(&outer.display().to_string()), "{envelope}");
    }
    assert_eq!(
        fs::read(planted.join(".heddle/oplog/oplog.bin")).ok(),
        oplog_before,
        "the planted repository must not be written"
    );

    // The hint's own escape hatch works: the enclosing repository, which
    // treats the fixture as content.
    let outer_arg = outer.display().to_string();
    heddle(&["-C", &outer_arg, "status"], Some(&planted)).expect("status of enclosing repo");
}

#[test]
fn safe_repositories_opts_in_to_a_nested_repository() {
    let (temp, _outer, planted) = enclosing_with_planted_repo();
    // Trust honours the repository's own config, so its CA must be loadable.
    let config_path = planted.join(".heddle/config.toml");
    let config = fs::read_to_string(&config_path).expect("read planted config");
    fs::write(
        &config_path,
        config.replace(
            "\n[remote]\ntls_ca_certificate_path = \"attacker-ca.pem\"\n",
            "",
        ),
    )
    .expect("drop planted CA");
    let user_config = temp.path().join("user-config.toml");
    fs::write(
        &user_config,
        format!(
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n\n[safe]\nrepositories = [\"{}\"]\n",
            planted.display()
        ),
    )
    .expect("write user config");
    let user_config = user_config.display().to_string();

    let output = heddle_output_with_env(
        &["--output", "json", "status"],
        Some(&planted),
        &[("HEDDLE_CONFIG", &user_config)],
    )
    .expect("spawn heddle");
    assert!(
        output.status.success(),
        "an explicitly trusted nested repository must open; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn relative_safe_repository_entry_is_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let user_config = temp.path().join("user-config.toml");
    fs::write(&user_config, "[safe]\nrepositories = [\"sub\"]\n").expect("write user config");
    let user_config = user_config.display().to_string();
    let output = heddle_output_with_env(
        &["status"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", &user_config)],
    )
    .expect("spawn heddle");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("safe.repositories"), "{stderr}");
}

/// P3: `daemon status -C <planted>` used the planted path as a raw root and,
/// after an open failure, read `<planted>/.heddle` directly.
#[test]
fn daemon_status_refuses_planted_repository_named_with_dash_c() {
    let (temp, _outer, planted) = enclosing_with_planted_repo();
    let planted_arg = planted.display().to_string();
    let output = heddle_output(
        &["--output", "json", "-C", &planted_arg, "daemon", "status"],
        Some(temp.path()),
    )
    .expect("spawn heddle");
    assert_eq!(
        output.status.code(),
        Some(78),
        "daemon status must refuse the planted repository; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope = error_envelope(&output, "daemon status");
    assert_eq!(envelope["kind"], "untrusted_repository", "{envelope}");
}

/// P3: the `status --short` fast path parsed the planted repository's config
/// before anything refused it.
#[test]
fn status_short_fast_path_refuses_before_parsing_planted_config() {
    let (_temp, _outer, planted) = enclosing_with_planted_repo();
    let config_path = planted.join(".heddle/config.toml");
    let config = fs::read_to_string(&config_path).expect("read planted config");
    fs::write(
        &config_path,
        format!("{config}\n[output]\nformat = \"bogus\"\n"),
    )
    .expect("write planted config");

    let output = heddle_output(&["--output", "json", "status", "--short"], Some(&planted))
        .expect("spawn heddle");
    let envelope = error_envelope(&output, "status --short");
    assert_eq!(envelope["kind"], "untrusted_repository", "{envelope}");
}

/// P2-c: a repository `heddle init` creates inside another repository's
/// worktree is vouched for by its creation record; no `[safe]` entry needed.
#[test]
fn heddle_init_inside_a_worktree_is_trusted_without_safe_entry() {
    let temp = TempDir::new().expect("tempdir");
    let outer = temp.path().join("outer");
    fs::create_dir_all(&outer).expect("create outer");
    heddle(&["init"], Some(&outer)).expect("init outer");
    heddle(&["init", "libs/inner"], Some(&outer)).expect("init nested");
    let nested = outer.join("libs/inner");
    let output = heddle_output(&["--output", "json", "status"], Some(&nested)).expect("spawn");
    assert!(
        output.status.success(),
        "a Heddle-created nested repository must open; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(report["output_kind"], "status", "{report}");
}
