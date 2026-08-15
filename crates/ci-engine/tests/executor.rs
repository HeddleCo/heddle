// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

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

fn run(raw: &str, workdir: &std::path::Path) -> Vec<ci_engine::CheckResult> {
    let config = ci_config::parse(raw).expect("config");
    let provider = NoopProvider;
    run_checks(
        &config,
        &context(),
        &RunOptions {
            workdir,
            services: &provider,
            now_rfc3339: &fixed_clock,
        },
    )
}

#[test]
fn executes_argv_and_classifies_treadle_failure_shapes() {
    let workdir = tempfile::tempdir().expect("workdir");
    let results = run(
        r#"
[meta]
schema = 1
[[check]]
name = "ok"
command = ['/bin/sh', '-c', 'printf "\033[32mok\033[0m"']
[[check]]
name = "compile"
command = ["/bin/sh", "-c", "echo 'error[E0432]: unresolved import'; exit 101"]
"#,
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
    let config = ci_config::parse(
        r#"
[meta]
schema = 1
[[check]]
name = "flaky"
command = ["/bin/sh", "-c", "if test -e retry-marker; then exit 0; else : > retry-marker; echo FLAKE; exit 1; fi"]
[check.retry]
max = 1
flake_signatures = ["FLAKE"]
"#,
    )
    .expect("config");
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
    );
    assert_eq!(results[0].conclusion(), Conclusion::Success);
    assert_eq!(results[0].attempts, 2);
    assert!(results[0].recovered_after_flake());
}

#[test]
fn unsupported_local_service_is_an_honest_infra_verdict() {
    let workdir = tempfile::tempdir().expect("workdir");
    let results = run(
        r#"
[meta]
schema = 1
[[check]]
name = "database"
command = ["/bin/true"]
[[check.services]]
name = "postgres"
image = "postgres:17"
"#,
        workdir.path(),
    );
    assert_eq!(results[0].conclusion(), Conclusion::InfraError);
    let failure = results[0].body.outcome.failure.as_ref().expect("failure");
    assert_eq!(failure.class, FailureClass::Infra);
    assert_eq!(failure.subclass.as_deref(), Some("service_provisioning"));
}

#[test]
fn cache_environment_points_outside_the_source_tree() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = tempfile::tempdir().expect("cache");
    let config = ci_config::parse(
        r#"
[meta]
schema = 1
[[check]]
name = "cache"
command = ["/usr/bin/env"]
cache_paths = ["cargo"]
"#,
    )
    .expect("config");
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
    );
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
    let config = format!(
        r#"
[meta]
schema = 1
[[check]]
name = "timeout"
command = ["/bin/sh", "-c", "/bin/sleep 120 & echo $! > '{}'; /bin/sleep 120"]
timeout_secs = 1
"#,
        pidfile.display()
    );
    let results = run(&config, workdir.path());
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
