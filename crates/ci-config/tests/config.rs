// SPDX-License-Identifier: Apache-2.0

use api::{
    heddle::api::v1alpha1::{
        TreadleCheckClass, TreadleNetworkAccess, TreadlePlatform, TreadleSecretRef,
        TreadleSecretTier, TreadleTrigger, TreadleTriggerKind, treadle_env_entry,
    },
    treadle::treadle_definition_blake3,
};
use ci_config::{
    CheckClass, ConfigError, Trigger, admit_host_exec, argv_check, cache_path_is_worktree_relative,
    canonical_definition, definition, host_oci_platform, host_pipeline_fixture,
    host_pipeline_with_required_failure, load, load_lock_file, lock_json, read_lock, verify_lock,
};

fn unix_true() -> (String, Vec<&'static str>) {
    ("/bin/true".to_string(), Vec::new())
}

fn encode_true(name: &str) -> (Vec<u8>, String) {
    let (command, args) = unix_true();
    let args: Vec<&str> = args;
    let definition = definition("local", "local", vec![argv_check(name, &command, &args)]);
    canonical_definition(&definition).expect("canonical definition")
}

#[test]
fn loads_a_canonical_definition_and_maps_the_check() {
    let mut lint = argv_check("lint", "cargo", &["clippy", "--", "-D", "warnings"]);
    lint.class = TreadleCheckClass::Advisory as i32;
    lint.timeout_seconds = 30;
    lint.retry.as_mut().expect("retry").max_retries = 1;
    lint.retry
        .as_mut()
        .expect("retry")
        .flake_signatures
        .push("dns error".to_string());
    lint.triggers = vec![
        TreadleTrigger {
            kind: TreadleTriggerKind::Push as i32,
            cron_expression: String::new(),
        },
        TreadleTrigger {
            kind: TreadleTriggerKind::Cron as i32,
            cron_expression: "0 2 * * 1".to_string(),
        },
    ];
    let definition = definition("heddle-ci", "build", vec![lint]);
    let (bytes, digest) = canonical_definition(&definition).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert_eq!(loaded.definition_digest, digest);
    assert_eq!(
        loaded.definition_digest,
        hex::encode(treadle_definition_blake3(&loaded.definition).expect("blake3"))
    );
    assert_eq!(loaded.config.checks.len(), 1);
    assert_eq!(loaded.config.checks[0].class, CheckClass::Advisory);
    assert_eq!(
        loaded.config.checks[0].command,
        ["cargo", "clippy", "--", "-D", "warnings"]
    );
    assert_eq!(loaded.config.checks[0].retry.max, 1);
    assert!(matches!(loaded.config.checks[0].triggers[0], Trigger::Push));
}

#[test]
fn rejects_duplicate_names_across_jobs() {
    let first = argv_check("unit", "/bin/true", &[]);
    let second = argv_check("unit", "/bin/true", &[]);
    let mut definition = definition("local", "alpha", vec![first]);
    definition
        .jobs
        .push(api::heddle::api::v1alpha1::TreadleJob {
            name: "beta".to_string(),
            matrix: Vec::new(),
            checks: vec![second],
        });
    let (bytes, _) = canonical_definition(&definition).expect("canonical");
    let error = load(&bytes).expect_err("duplicate");
    assert!(matches!(error, ConfigError::DuplicateCheckName { .. }));
}

#[test]
fn non_canonical_bytes_fail_closed() {
    let bytes = ci_config::non_canonical_bytes();
    assert!(matches!(
        load(&bytes).expect_err("non-canonical"),
        ConfigError::NonCanonicalBytes
    ));
}

#[test]
fn lock_digest_must_match() {
    let (bytes, digest) = encode_true("ok");
    let loaded = load(&bytes).expect("load");
    let lock = read_lock(lock_json(&digest).as_bytes()).expect("lock");
    verify_lock(&lock, &loaded.definition_digest).expect("match");

    let mutated = lock_json(&"ab".repeat(32));
    let lock = read_lock(mutated.as_bytes()).expect("mutated lock parses");
    assert!(matches!(
        verify_lock(&lock, &loaded.definition_digest).expect_err("mismatch"),
        ConfigError::LockMismatch { .. }
    ));
}

#[test]
fn platform_mismatch_refuses_host_exec() {
    let (host_os, host_arch) = host_oci_platform();
    let mut check = argv_check("unit", "/bin/true", &[]);
    let foreign_os = if host_os == "linux" {
        "darwin"
    } else {
        "linux"
    };
    check.target_environment.as_mut().expect("target").platform = Some(TreadlePlatform {
        os: foreign_os.to_string(),
        arch: host_arch.clone(),
    });
    let definition = definition("local", "local", vec![check]);
    let (bytes, _) = canonical_definition(&definition).expect("canonical");
    let loaded = load(&bytes).expect("load");
    let error = admit_host_exec(&loaded.definition, &[]).expect_err("platform");
    assert!(matches!(
        error,
        ConfigError::PlatformMismatch { name, wanted_os, .. }
            if name == "unit" && wanted_os == foreign_os
    ));
    assert_ne!(host_os, foreign_os);
}

#[test]
fn trusted_runner_secret_and_full_network_refuse_host_exec() {
    let mut secret_check = argv_check("needs-token", "/bin/true", &[]);
    secret_check
        .env
        .push(api::heddle::api::v1alpha1::TreadleEnvEntry {
            name: "TOKEN".to_string(),
            source: Some(treadle_env_entry::Source::SecretRef(
                "registry-token".to_string(),
            )),
        });
    let mut secret_def = definition("local", "local", vec![secret_check]);
    secret_def.secret_refs.push(TreadleSecretRef {
        name: "registry-token".to_string(),
        provider: String::new(),
        tier: TreadleSecretTier::TrustedRunnerOnly as i32,
    });
    let (bytes, _) = canonical_definition(&secret_def).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("secret"),
        ConfigError::TrustedRunnerSecret { .. }
    ));

    let mut full = argv_check("open-net", "/bin/true", &[]);
    full.isolation.as_mut().expect("isolation").network_access = TreadleNetworkAccess::Full as i32;
    let full_def = definition("local", "local", vec![full]);
    let (bytes, _) = canonical_definition(&full_def).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("full"),
        ConfigError::FullNetwork { .. }
    ));
}

#[test]
fn host_matching_definition_is_admitted() {
    let (bytes, _) = encode_true("unit");
    let loaded = load(&bytes).expect("load");
    admit_host_exec(&loaded.definition, &[]).expect("host-exec eligible");
    admit_host_exec(&loaded.definition, &["unit".to_string()]).expect("selected");
}

#[test]
fn two_job_pipeline_flattens_to_three_host_exec_checks() {
    let definition = host_pipeline_fixture();
    assert_eq!(definition.jobs.len(), 2);
    let (bytes, digest) = canonical_definition(&definition).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert_eq!(loaded.definition_digest, digest);
    admit_host_exec(&loaded.definition, &[]).expect("host-exec");
    let names: Vec<_> = loaded
        .config
        .checks
        .iter()
        .map(|check| check.name.as_str())
        .collect();
    assert_eq!(names, ["docs-ok", "echo", "ok"]);
    assert_eq!(loaded.config.checks[1].command, ["/bin/echo", "pipeline"]);

    let failing = host_pipeline_with_required_failure();
    let (bytes, _) = canonical_definition(&failing).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert_eq!(
        loaded
            .config
            .checks
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>(),
        ["fail", "later", "sibling"]
    );
}

#[test]
fn cpu_millis_and_named_profile_refuse_host_exec() {
    let mut cpu = argv_check("cpu", "/bin/true", &[]);
    cpu.isolation.as_mut().expect("isolation").cpu_millis = 1000;
    let authored = definition("local", "local", vec![cpu]);
    let (bytes, _) = canonical_definition(&authored).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("cpu"),
        ConfigError::UnsupportedIsolation { name, detail }
            if name == "cpu" && detail.contains("cpu_millis")
    ));

    let mut memory = argv_check("mem", "/bin/true", &[]);
    memory.isolation.as_mut().expect("isolation").memory_bytes = 1;
    let authored = definition("local", "local", vec![memory]);
    let (bytes, _) = canonical_definition(&authored).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("memory"),
        ConfigError::UnsupportedIsolation { detail, .. } if detail.contains("memory_bytes")
    ));

    let mut processes = argv_check("nproc", "/bin/true", &[]);
    processes
        .isolation
        .as_mut()
        .expect("isolation")
        .process_limit = 8;
    let authored = definition("local", "local", vec![processes]);
    let (bytes, _) = canonical_definition(&authored).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("nproc"),
        ConfigError::UnsupportedIsolation { detail, .. } if detail.contains("process_limit")
    ));

    let mut profile = argv_check("bench", "/bin/true", &[]);
    profile.isolation.as_mut().expect("isolation").profile = "bench".to_string();
    let authored = definition("local", "local", vec![profile]);
    let (bytes, _) = canonical_definition(&authored).expect("canonical");
    let loaded = load(&bytes).expect("load");
    assert!(matches!(
        admit_host_exec(&loaded.definition, &[]).expect_err("profile"),
        ConfigError::UnsupportedIsolation { detail, .. } if detail.contains("profile=bench")
    ));
}

#[test]
fn network_none_is_still_admitted() {
    let mut check = argv_check("unit", "/bin/true", &[]);
    check.isolation.as_mut().expect("isolation").network_access = TreadleNetworkAccess::None as i32;
    let definition = definition("local", "local", vec![check]);
    let (bytes, _) = canonical_definition(&definition).expect("canonical");
    let loaded = load(&bytes).expect("load");
    admit_host_exec(&loaded.definition, &[]).expect("NONE is host-ok in v0");
}

#[test]
fn escaping_cache_path_refuses_host_exec() {
    assert!(!cache_path_is_worktree_relative("/tmp/target"));
    assert!(!cache_path_is_worktree_relative("../escape"));
    assert!(!cache_path_is_worktree_relative("foo/../../etc"));
    assert!(cache_path_is_worktree_relative("target"));
    assert!(cache_path_is_worktree_relative("foo/bar"));

    let mut check = argv_check("unit", "/bin/true", &[]);
    check.cache_paths.push("../escape".to_string());
    let authored = definition("local", "local", vec![check]);
    // The api canonical writer already refuses this path; admit still
    // guards the proto in case a caller skips that writer.
    assert!(matches!(
        admit_host_exec(&authored, &[]).expect_err("escape"),
        ConfigError::InvalidCachePath { path, .. } if path == "../escape"
    ));
}

#[test]
fn missing_lock_file_fails_closed() {
    let error = load_lock_file(std::path::Path::new("/no/such/treadle.lock.json"))
        .expect_err("missing lock");
    assert!(matches!(error, ConfigError::LockMissing { .. }));
}
