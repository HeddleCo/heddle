// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use ci_config::{Check, CiConfig, Retry, Service};
use ci_engine::{
    ExecutionContext, HermeticEnv, NoopProvider, RunControls, RunOptions, run_checks,
    run_checks_with,
};
use crypto::{Basis, BasisKind, Conclusion, FailureClass, StateRef};

fn context() -> ExecutionContext {
    ExecutionContext {
        repo: "test/repo".to_string(),
        state: StateRef {
            content_hash: "state-content".to_string(),
            change_id: "change".to_string(),
            logical_change_id: None,
        },
        basis: Basis {
            kind: BasisKind::Branch,
            evaluated_tree_digest: "tree".to_string(),
        },
        definition_digest: "definition".to_string(),
        toolchain: None,
        pick_id: None,
        attempt: 1,
        runner: None,
        image_digest: None,
    }
}

fn fixed_clock() -> String {
    "2026-08-14T12:00:00Z".to_string()
}

fn run(config: &CiConfig, workdir: &std::path::Path) -> Vec<ci_engine::CheckResult> {
    let provider = NoopProvider;
    run_checks(
        config,
        &context(),
        &RunOptions {
            workdir,
            services: &provider,
            now_rfc3339: &fixed_clock,
        },
    )
    .expect("run")
}

fn sh(name: &str, script: &str) -> Check {
    Check::new(
        name,
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
    )
}

#[test]
fn executes_argv_and_classifies_treadle_failure_shapes() {
    let workdir = tempfile::tempdir().expect("workdir");
    let results = run(
        &CiConfig::from_checks(vec![
            sh("ok", r#"printf "\033[32mok\033[0m""#),
            sh(
                "compile",
                "echo 'error[E0432]: unresolved import'; exit 101",
            ),
        ]),
        workdir.path(),
    );
    assert_eq!(results[0].conclusion(), Conclusion::Success);
    assert_eq!(results[0].combined_output, "ok");
    assert_eq!(results[1].conclusion(), Conclusion::Failure);
    assert_eq!(
        results[1]
            .body
            .outcome
            .failure
            .as_ref()
            .map(|failure| failure.class),
        Some(FailureClass::Build)
    );
}

#[test]
fn retries_only_when_output_matches_a_flake_signature() {
    let workdir = tempfile::tempdir().expect("workdir");
    let mut check = sh(
        "flaky",
        "if test -e retry-marker; then exit 0; else : > retry-marker; echo FLAKE; exit 1; fi",
    );
    check.retry = Retry {
        max: 1,
        flake_signatures: vec!["FLAKE".to_string()],
    };
    let config = CiConfig::from_checks(vec![check]);
    let provider = NoopProvider;
    let environment = HermeticEnv::with_host(BTreeMap::new());
    let controls = RunControls {
        hermetic_env: Some(&environment),
        ..RunControls::default()
    };
    let results = run_checks_with(
        &config,
        &context(),
        &RunOptions {
            workdir: workdir.path(),
            services: &provider,
            now_rfc3339: &fixed_clock,
        },
        &controls,
    )
    .expect("run");
    assert_eq!(results[0].conclusion(), Conclusion::Success);
    assert_eq!(results[0].attempts, 2);
    assert!(results[0].recovered_after_flake());
}

#[test]
fn unsupported_local_service_is_an_honest_infra_verdict() {
    let workdir = tempfile::tempdir().expect("workdir");
    let mut check = Check::new("database", vec!["/bin/true".to_string()]);
    check.services.push(Service {
        name: "postgres".to_string(),
        image: "postgres:17".to_string(),
        ports: Vec::new(),
        env: BTreeMap::new(),
        ready_cmd: None,
    });
    let results = run(&CiConfig::from_checks(vec![check]), workdir.path());
    assert_eq!(results[0].conclusion(), Conclusion::InfraError);
    let failure = results[0].body.outcome.failure.as_ref().expect("failure");
    assert_eq!(failure.class, FailureClass::Infra);
    assert_eq!(failure.subclass.as_deref(), Some("service_provisioning"));
}

#[test]
fn cache_environment_points_outside_the_source_tree() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = tempfile::tempdir().expect("cache");
    let mut check = Check::new("cache", vec!["/usr/bin/env".to_string()]);
    check.cache_paths = vec!["cargo".to_string()];
    let config = CiConfig::from_checks(vec![check]);
    let expected = cache.path().join("CARGO").display().to_string();
    let environment = HermeticEnv::with_host(BTreeMap::from([("EXPECTED".to_string(), expected)]));
    let provider = NoopProvider;
    let controls = RunControls {
        cache_root: Some(cache.path()),
        hermetic_env: Some(&environment),
        ..RunControls::default()
    };
    let results = run_checks_with(
        &config,
        &context(),
        &RunOptions {
            workdir: workdir.path(),
            services: &provider,
            now_rfc3339: &fixed_clock,
        },
        &controls,
    )
    .expect("run");
    assert_eq!(results[0].conclusion(), Conclusion::Success);
    assert!(
        results[0]
            .combined_output
            .lines()
            .any(|line| line == format!("HCI_CACHE_CARGO={}", cache.path().join("CARGO").display()))
    );
}

#[test]
fn failure_excerpt_is_utf8_safe_and_capped() {
    let output = "error[E0001]: ".to_string() + &"é".repeat(3_000);
    let excerpt = ci_engine::extract_excerpt(&output, FailureClass::Build);
    assert!(excerpt.len() <= ci_engine::EXCERPT_CAP_BYTES);
    assert!(excerpt.is_char_boundary(excerpt.len()));
    assert!(excerpt.ends_with("[excerpt truncated]"));
}

#[cfg(unix)]
#[test]
fn timeout_kills_the_entire_process_group() {
    use std::time::Duration;

    let workdir = tempfile::tempdir().expect("workdir");
    let pidfile = workdir.path().join("grandchild.pid");
    let mut check = sh(
        "timeout",
        &format!(
            "/bin/sleep 120 & echo $! > '{}'; /bin/sleep 120",
            pidfile.display()
        ),
    );
    check.timeout_secs = 1;
    let results = run(&CiConfig::from_checks(vec![check]), workdir.path());
    assert_eq!(results[0].conclusion(), Conclusion::TimedOut);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("grandchild pid")
        .trim()
        .parse()
        .expect("numeric pid");
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: signal zero only checks whether the recorded process still exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        // SAFETY: cleanup is limited to the exact child pid recorded by the test.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        panic!("grandchild {pid} survived the timeout process-group kill");
    }
}

#[test]
fn proto_definition_digest_is_bound_on_an_executed_check() {
    let workdir = tempfile::tempdir().expect("workdir");
    let definition = ci_config::definition(
        "local",
        "local",
        vec![ci_config::argv_check("unit", "/bin/true", &[])],
    );
    let (bytes, digest) = ci_config::canonical_definition(&definition).expect("canonical");
    let loaded = ci_config::load(&bytes).expect("load");
    assert_eq!(loaded.definition_digest, digest);
    ci_config::admit_host_exec(&loaded.definition, &[]).expect("host-exec");

    let mut context = context();
    context.definition_digest = loaded.definition_digest.clone();
    let results = run_checks(
        &loaded.config,
        &context,
        &RunOptions {
            workdir: workdir.path(),
            services: &NoopProvider,
            now_rfc3339: &fixed_clock,
        },
    )
    .expect("run");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].conclusion(), Conclusion::Success);
    assert_eq!(results[0].body.check.name, "unit");
    assert_eq!(results[0].body.check.class, crypto::CheckClass::Required);
    assert_eq!(results[0].body.check.definition_digest, digest);
    assert_eq!(results[0].body.check.command, ["/bin/true"]);
}

#[test]
fn proto_working_directory_is_honored() {
    let workdir = tempfile::tempdir().expect("workdir");
    let nested = workdir.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    let mut check = Check::new(
        "cwd",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test -f here.txt".to_string(),
        ],
    );
    check.working_directory = "nested".to_string();
    std::fs::write(nested.join("here.txt"), "ok").expect("marker");
    let results = run(&CiConfig::from_checks(vec![check]), workdir.path());
    assert_eq!(results[0].conclusion(), Conclusion::Success);
}
