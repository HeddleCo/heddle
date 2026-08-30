// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, path::Path};

use ci_config::{Check, CiConfig};
use ci_engine::{
    CheckResult, ExecutionContext, FsResultCache, HermeticEnv, MemoryResultCache, NoopProvider,
    ResultCache, ResultCacheEntry, ResultCacheError, RunControls, RunOptions, SpotCheck,
    SpotCheckDivergence, run_checks_with,
};
use crypto::{Basis, BasisKind, Conclusion, StateRef};

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

fn marker_config() -> CiConfig {
    env_config(None)
}

fn env_config(foo: Option<&str>) -> CiConfig {
    let mut check = Check::new(
        "marker",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo ran >> marker; echo ok".to_string(),
        ],
    );
    if let Some(value) = foo {
        check.env.insert("FOO".to_string(), value.to_string());
    }
    CiConfig::from_checks(vec![check])
}

fn run_cached(
    config: &CiConfig,
    workdir: &Path,
    context: &ExecutionContext,
    cache: &dyn ResultCache,
    spot_check: SpotCheck,
) -> Result<Vec<CheckResult>, ResultCacheError> {
    let provider = NoopProvider;
    let environment = HermeticEnv::with_host(BTreeMap::new());
    run_checks_with(
        config,
        context,
        &RunOptions {
            workdir,
            services: &provider,
            now_rfc3339: &|| "2026-08-14T12:00:00Z".to_string(),
        },
        &RunControls {
            hermetic_env: Some(&environment),
            result_cache: Some(cache),
            spot_check,
            ..RunControls::default()
        },
    )
}

fn marker_runs(workdir: &Path) -> usize {
    std::fs::read_to_string(workdir.join("marker"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

fn seed(config: &CiConfig, workdir: &Path, context: &ExecutionContext, cache: &dyn ResultCache) {
    run_cached(config, workdir, context, cache, SpotCheck::Never).expect("seed");
}

#[test]
fn cache_hit_reuses_result_and_does_not_rerun() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    let config = marker_config();
    let first = run_cached(
        &config,
        workdir.path(),
        &context(),
        &cache,
        SpotCheck::Never,
    )
    .expect("seed");
    assert_eq!(first[0].conclusion(), Conclusion::Success);
    assert_eq!(first[0].combined_output, "ok\n");
    assert_eq!(marker_runs(workdir.path()), 1);

    let second = run_cached(
        &config,
        workdir.path(),
        &context(),
        &cache,
        SpotCheck::Never,
    )
    .expect("hit");
    assert_eq!(second[0].combined_output, first[0].combined_output);
    assert_eq!(second[0].body.outcome, first[0].body.outcome);
    assert_eq!(marker_runs(workdir.path()), 1, "cache hit must not re-run");
}

#[test]
fn changed_env_is_a_cache_miss() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    seed(&env_config(Some("one")), workdir.path(), &context(), &cache);
    seed(&env_config(Some("two")), workdir.path(), &context(), &cache);
    assert_eq!(
        marker_runs(workdir.path()),
        2,
        "a different declared env must not reuse the cached result"
    );
}

#[test]
fn changed_input_is_a_cache_miss() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    let config = marker_config();
    seed(&config, workdir.path(), &context(), &cache);

    let mut changed = context();
    changed.basis.evaluated_tree_digest = "tree-changed".to_string();
    seed(&config, workdir.path(), &changed, &cache);
    assert_eq!(
        marker_runs(workdir.path()),
        2,
        "tree digest change is a miss"
    );

    changed = context();
    changed.state.content_hash = "state-changed".to_string();
    seed(&config, workdir.path(), &changed, &cache);
    assert_eq!(
        marker_runs(workdir.path()),
        3,
        "state content hash change is a miss"
    );
}

#[test]
fn changed_definition_is_a_cache_miss() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    let config = marker_config();
    seed(&config, workdir.path(), &context(), &cache);
    let mut changed = context();
    changed.definition_digest = "definition-changed".to_string();
    seed(&config, workdir.path(), &changed, &cache);
    assert_eq!(
        marker_runs(workdir.path()),
        2,
        "a different definition digest must not reuse the cached result"
    );
}

#[test]
fn serialized_entry_seeds_a_separate_cache_instance() {
    let workdir = tempfile::tempdir().expect("workdir");
    let seed_cache = MemoryResultCache::new();
    let config = marker_config();
    seed(&config, workdir.path(), &context(), &seed_cache);
    assert_eq!(marker_runs(workdir.path()), 1);

    let stored = seed_cache.entries();
    assert_eq!(stored.len(), 1);
    let bytes = serde_json::to_vec(&stored[0]).expect("serialize portable entry");
    let restored: ResultCacheEntry = serde_json::from_slice(&bytes).expect("deserialize");
    let peer_dir = tempfile::tempdir().expect("peer cache");
    let peer = FsResultCache::new(peer_dir.path());
    peer.put(&restored).expect("seed peer");

    let hit = run_cached(&config, workdir.path(), &context(), &peer, SpotCheck::Never)
        .expect("cross-instance hit");
    assert_eq!(hit[0].conclusion(), Conclusion::Success);
    assert_eq!(hit[0].combined_output, "ok\n");
    assert_eq!(marker_runs(workdir.path()), 1, "portable entry must hit");
}

#[test]
fn spot_check_fails_closed_on_a_divergent_entry() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    let config = marker_config();
    seed(&config, workdir.path(), &context(), &cache);

    let mut tampered = cache.entries().pop().expect("seeded entry");
    tampered.body.outcome.conclusion = Conclusion::Failure;
    cache.put(&tampered).expect("store divergent entry");

    let error = run_cached(
        &config,
        workdir.path(),
        &context(),
        &cache,
        SpotCheck::Always,
    )
    .expect_err("divergent spot-check must be a hard error");
    match &error {
        ResultCacheError::SpotCheckDivergence(divergence) => {
            let SpotCheckDivergence {
                check_name,
                cached_conclusion,
                fresh_conclusion,
                ..
            } = &**divergence;
            assert_eq!(check_name, "marker");
            assert_eq!(cached_conclusion, "failure");
            assert_eq!(fresh_conclusion, "success");
        }
        other => panic!("expected spot-check divergence, got {other}"),
    }
    assert_eq!(marker_runs(workdir.path()), 2, "spot-check re-runs fresh");
    assert!(
        error
            .to_string()
            .contains("refusing to trust the cache entry"),
        "{error}"
    );
}

#[test]
fn honest_spot_check_accepts_a_matching_entry() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache = MemoryResultCache::new();
    let config = marker_config();
    seed(&config, workdir.path(), &context(), &cache);
    let verified = run_cached(
        &config,
        workdir.path(),
        &context(),
        &cache,
        SpotCheck::Always,
    )
    .expect("matching spot-check must not fail");
    assert_eq!(verified[0].conclusion(), Conclusion::Success);
    assert_eq!(marker_runs(workdir.path()), 2);
}
