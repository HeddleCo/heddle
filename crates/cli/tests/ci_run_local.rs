// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crypto::{Conclusion, Ed25519Signer, SignedVerdict, Signer, SignerKind};
use repo::{Repository, identity::DeviceIdentity};

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    repo: Repository,
    home: PathBuf,
}

impl Fixture {
    fn new(config: &str) -> Self {
        let root = tempfile::tempdir().expect("repo root");
        let home = tempfile::tempdir().expect("heddle home");
        let repo = Repository::init_default(root.path()).expect("init repo");
        std::fs::write(repo.heddle_dir().join("ci.toml"), config).expect("write CI config");
        write_device(home.path());
        Self {
            repo,
            home: home.path().to_path_buf(),
            _root: root,
            _home: home,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
        command
            .args(["--repo", self.repo.root().to_str().expect("UTF-8 root")])
            .args(args)
            .env("HEDDLE_HOME", &self.home)
            .env("HEDDLE_FSMONITOR", "off")
            .env("HEDDLE_PRINCIPAL_NAME", "CI Test")
            .env("HEDDLE_PRINCIPAL_EMAIL", "ci@example.invalid");
        command.output().expect("run heddle")
    }
}

fn write_device(home: &Path) {
    let signer = Ed25519Signer::generate().expect("signer");
    let device = DeviceIdentity {
        public_key: hex::encode(signer.public_key()),
        private_key_pem: signer.to_pem().expect("PEM"),
        server: "https://test.invalid".to_string(),
        linked_at: "2026-08-14T12:00:00Z".to_string(),
    };
    let path = home.join(repo::identity::DEVICE_IDENTITY_FILE);
    std::fs::write(&path, toml::to_string(&device).expect("device TOML")).expect("write device");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("protect device");
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn json_verdicts_are_device_signed_and_verify() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "build"
class = "required"
command = ["/bin/sh", "-c", "echo ok"]
[[check]]
name = "advice"
class = "advisory"
command = ["/bin/sh", "-c", "echo 'test result: FAILED'; exit 1"]
"#,
    );
    let output = fixture.run(&["--output", "json", "ci", "run", "--local"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let verdicts: Vec<SignedVerdict> =
        serde_json::from_slice(&output.stdout).expect("signed verdict JSON");
    assert_eq!(verdicts.len(), 2);
    for verdict in &verdicts {
        verdict.verify().expect("verdict verifies");
        assert_eq!(verdict.signer_kind, SignerKind::Device);
        assert!(verdict.is_advisory_only());
        assert_eq!(
            verdict.body.basis.evaluated_tree_digest,
            verdict.tree_digest.to_hex()
        );
    }
    assert_eq!(verdicts[0].body.outcome.conclusion, Conclusion::Success);
    assert_eq!(verdicts[1].body.outcome.conclusion, Conclusion::Failure);
    assert!(stderr(&output).contains("advisory"));
}

#[test]
fn required_failure_renders_once_and_exits_nonzero() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "build"
class = "required"
command = ["/bin/false"]
"#,
    );
    let output = fixture.run(&["--output", "json", "ci", "run", "--local"]);
    assert_eq!(
        output.status.code(),
        Some(65),
        "stderr: {}",
        stderr(&output)
    );
    let verdicts: Vec<SignedVerdict> =
        serde_json::from_slice(&output.stdout).expect("single JSON report");
    assert_eq!(verdicts[0].body.outcome.conclusion, Conclusion::Failure);
    assert!(!stderr(&output).contains("\"error\""));
}

#[test]
fn named_state_runs_in_an_exact_isolated_checkout() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "clean-state"
class = "required"
command = ["/bin/sh", "-c", "test ! -e dirty.txt"]
"#,
    );
    std::fs::write(fixture.repo.root().join("dirty.txt"), "working tree only")
        .expect("write dirty file");
    let output = fixture.run(&[
        "--output", "json", "ci", "run", "--local", "--state", "HEAD",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let verdicts: Vec<SignedVerdict> =
        serde_json::from_slice(&output.stdout).expect("signed verdict JSON");
    assert_eq!(verdicts[0].body.outcome.conclusion, Conclusion::Success);
    let head = fixture.repo.current_state().unwrap().unwrap();
    assert_eq!(verdicts[0].tree_digest, head.tree);
}

#[test]
fn refuses_to_sign_when_a_check_mutates_the_working_tree() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "mutating"
class = "required"
command = ["/bin/sh", "-c", "echo changed > tracked.txt"]
"#,
    );
    let output = fixture.run(&["--output", "json", "ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "no stale verdict may be emitted");
    assert!(stderr(&output).contains("refusing to sign a stale tree digest"));
}

#[test]
fn missing_device_identity_fails_before_running_checks() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "would-run"
command = ["/bin/sh", "-c", "touch ran.txt"]
"#,
    );
    std::fs::remove_file(fixture.home.join(repo::identity::DEVICE_IDENTITY_FILE))
        .expect("remove identity");
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(!fixture.repo.root().join("ran.txt").exists());
    assert!(stderr(&output).contains("heddle auth login"));
}

#[test]
fn check_filter_is_exact_and_a_typo_is_an_error() {
    let fixture = Fixture::new(
        r#"
[meta]
schema = 1
[[check]]
name = "selected"
command = ["/bin/true"]
[[check]]
name = "not-selected"
command = ["/bin/false"]
"#,
    );
    let output = fixture.run(&[
        "--output", "json", "ci", "run", "--local", "--check", "selected",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let verdicts: Vec<SignedVerdict> =
        serde_json::from_slice(&output.stdout).expect("filtered verdict JSON");
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].body.check.name, "selected");

    let typo = fixture.run(&["ci", "run", "--local", "--check", "missing"]);
    assert!(!typo.status.success());
    assert!(stderr(&typo).contains("available checks: selected, not-selected"));
}

#[test]
fn local_mode_must_be_selected_explicitly() {
    let fixture = Fixture::new("[meta]\nschema = 1\n");
    let output = fixture.run(&["ci", "run"]);
    assert_eq!(output.status.code(), Some(64));
    assert!(stderr(&output).contains("--local"));
}
