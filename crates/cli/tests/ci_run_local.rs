// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use api::heddle::api::v1alpha1::{
    TreadleCheck, TreadleCheckClass, TreadleNetworkAccess, TreadlePlatform,
};
use ci_config::{
    DEFAULT_DEFINITION_FILE, DEFAULT_LOCK_FILE, argv_check, canonical_definition, definition,
    host_oci_platform, lock_json, non_canonical_bytes,
};
use crypto::{Conclusion, Ed25519Signer, SignedVerdict, Signer, SignerKind};
use repo::{Repository, identity::DeviceIdentity};

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    repo: Repository,
    home: PathBuf,
    digest: String,
}

impl Fixture {
    fn new(checks: Vec<TreadleCheck>) -> Self {
        Self::write(checks, true)
    }

    fn write(checks: Vec<TreadleCheck>, with_lock: bool) -> Self {
        let root = tempfile::tempdir().expect("repo root");
        let home = tempfile::tempdir().expect("heddle home");
        let repo = Repository::init_default(root.path()).expect("init repo");
        let definition = definition("local", "local", checks);
        let (bytes, digest) = canonical_definition(&definition).expect("canonical definition");
        std::fs::write(repo.heddle_dir().join(DEFAULT_DEFINITION_FILE), bytes)
            .expect("write definition");
        if with_lock {
            std::fs::write(
                repo.heddle_dir().join(DEFAULT_LOCK_FILE),
                lock_json(&digest),
            )
            .expect("write lock");
        }
        write_device(home.path());
        Self {
            repo,
            home: home.path().to_path_buf(),
            digest,
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

fn sh(name: &str, script: &str) -> TreadleCheck {
    argv_check(name, "/bin/sh", &["-c", script])
}

fn verdict_named<'a>(verdicts: &'a [SignedVerdict], name: &str) -> &'a SignedVerdict {
    verdicts
        .iter()
        .find(|verdict| verdict.body.check.name == name)
        .unwrap_or_else(|| panic!("missing check {name}"))
}

#[test]
fn json_verdicts_are_device_signed_and_verify() {
    let mut advice = sh("advice", "echo 'test result: FAILED'; exit 1");
    advice.class = TreadleCheckClass::Advisory as i32;
    let fixture = Fixture::new(vec![sh("build", "echo ok"), advice]);
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
        assert_eq!(verdict.body.check.definition_digest, fixture.digest);
    }
    assert_eq!(
        verdict_named(&verdicts, "build").body.outcome.conclusion,
        Conclusion::Success
    );
    assert_eq!(
        verdict_named(&verdicts, "advice").body.outcome.conclusion,
        Conclusion::Failure
    );
    assert!(stderr(&output).contains("advisory"));
}

#[test]
fn required_failure_renders_once_and_exits_nonzero() {
    let fixture = Fixture::new(vec![argv_check("build", "/bin/false", &[])]);
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
    assert_eq!(verdicts[0].body.check.definition_digest, fixture.digest);
    assert!(!stderr(&output).contains("\"error\""));
}

#[test]
fn named_state_runs_in_an_exact_isolated_checkout() {
    let fixture = Fixture::new(vec![sh("clean-state", "test ! -e dirty.txt")]);
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
    let fixture = Fixture::new(vec![sh("mutating", "echo changed > tracked.txt")]);
    let output = fixture.run(&["--output", "json", "ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "no stale verdict may be emitted");
    assert!(stderr(&output).contains("refusing to sign a stale tree digest"));
}

#[test]
fn missing_device_identity_fails_before_running_checks() {
    let fixture = Fixture::new(vec![sh("would-run", "touch ran.txt")]);
    std::fs::remove_file(fixture.home.join(repo::identity::DEVICE_IDENTITY_FILE))
        .expect("remove identity");
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(!fixture.repo.root().join("ran.txt").exists());
    assert!(stderr(&output).contains("heddle auth login"));
}

#[test]
fn check_filter_is_exact_and_a_typo_is_an_error() {
    let fixture = Fixture::new(vec![
        argv_check("selected", "/bin/true", &[]),
        argv_check("not-selected", "/bin/false", &[]),
    ]);
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
    assert!(stderr(&typo).contains("available checks:"));
    assert!(stderr(&typo).contains("selected"));
    assert!(stderr(&typo).contains("not-selected"));
}

#[test]
fn local_mode_must_be_selected_explicitly() {
    let fixture = Fixture::new(vec![argv_check("unit", "/bin/true", &[])]);
    let output = fixture.run(&["ci", "run"]);
    assert_eq!(output.status.code(), Some(64));
    assert!(stderr(&output).contains("--local"));
}

#[test]
fn proto_digest_on_a_passing_local_run_matches_the_lock() {
    let fixture = Fixture::new(vec![argv_check("unit", "/bin/true", &[])]);
    let output = fixture.run(&["--output", "json", "ci", "run", "--local"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let verdicts: Vec<SignedVerdict> =
        serde_json::from_slice(&output.stdout).expect("signed verdict JSON");
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].body.outcome.conclusion, Conclusion::Success);
    assert_eq!(verdicts[0].body.check.definition_digest, fixture.digest);
    assert_eq!(verdicts[0].body.check.command, ["/bin/true"]);
}

#[test]
fn mutated_lock_digest_fails_closed() {
    let fixture = Fixture::new(vec![argv_check("unit", "/bin/true", &[])]);
    std::fs::write(
        fixture.repo.heddle_dir().join(DEFAULT_LOCK_FILE),
        lock_json(&"ab".repeat(32)),
    )
    .expect("mutate lock");
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("does not match"));
}

#[test]
fn non_canonical_definition_fails_closed() {
    let fixture = Fixture::write(vec![argv_check("unit", "/bin/true", &[])], false);
    let path = fixture.repo.heddle_dir().join(DEFAULT_DEFINITION_FILE);
    std::fs::write(&path, non_canonical_bytes()).expect("write non-canonical definition");
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("not canonical"));
}

#[test]
fn platform_mismatch_refuses_without_running() {
    let (host_os, host_arch) = host_oci_platform();
    let foreign_os = if host_os == "linux" {
        "darwin"
    } else {
        "linux"
    };
    let mut check = argv_check("unit", "/bin/sh", &["-c", "touch ran.txt"]);
    check.target_environment.as_mut().expect("target").platform = Some(TreadlePlatform {
        os: foreign_os.to_string(),
        arch: host_arch,
    });
    let fixture = Fixture::new(vec![check]);
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(!fixture.repo.root().join("ran.txt").exists());
    assert!(stderr(&output).contains("host-exec"));
}

#[test]
fn full_network_refuses_without_pretending_hermeticity() {
    let mut check = argv_check("unit", "/bin/sh", &["-c", "touch ran.txt"]);
    check.isolation.as_mut().expect("isolation").network_access = TreadleNetworkAccess::Full as i32;
    let fixture = Fixture::new(vec![check]);
    let output = fixture.run(&["ci", "run", "--local"]);
    assert!(!output.status.success());
    assert!(!fixture.repo.root().join("ran.txt").exists());
    assert!(stderr(&output).contains("FULL"));
}
