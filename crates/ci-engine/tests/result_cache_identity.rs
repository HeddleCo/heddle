// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, path::Path};

use ci_engine::{
    run_checks_with, ExecutionContext, FsResultCache, HermeticEnv, MemoryResultCache, NoopProvider,
    ResultCache, RunControls, RunOptions, SpotCheck,
};
use crypto::{Basis, BasisKind, CheckClass, StateRef};

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

fn echo_ok(name: &str, class: &str) -> String {
    format!(
        r#"
[meta]
schema = 1
[[check]]
name = "{name}"
class = "{class}"
command = ["/bin/sh", "-c", "echo ran >> marker; echo ok"]
"#
    )
}

fn run(
    raw: &str,
    workdir: &Path,
    cache: &dyn ResultCache,
) -> Result<Vec<ci_engine::CheckResult>, ci_engine::ResultCacheError> {
    let config = ci_config::parse(raw).expect("config");
    let environment = HermeticEnv::with_host(BTreeMap::new());
    run_checks_with(
        &config,
        &context(),
        &RunOptions {
            workdir,
            services: &NoopProvider,
            now_rfc3339: &|| "2026-08-14T12:00:00Z".to_string(),
        },
        &RunControls {
            hermetic_env: Some(&environment),
            result_cache: Some(cache),
            spot_check: SpotCheck::Never,
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

fn plant_json(root: &Path, entry: &ci_engine::ResultCacheEntry) {
    let slot = std::fs::read_dir(root)
        .expect("cache")
        .filter_map(|entry| entry.ok())
        .filter(|shard| shard.path().is_dir())
        .flat_map(|shard| std::fs::read_dir(shard.path()).ok())
        .flatten()
        .filter_map(|file| file.ok())
        .map(|file| file.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .expect("required cache slot");
    std::fs::write(slot, serde_json::to_vec(entry).expect("plant")).expect("overwrite");
}

#[test]
fn planted_output_digest_from_a_different_check_identity_does_not_hit() {
    let workdir = tempfile::tempdir().expect("workdir");
    let cache_dir = tempfile::tempdir().expect("cache");
    let cache = FsResultCache::new(cache_dir.path());
    let required = echo_ok("required", "required");
    run(&required, workdir.path(), &cache).expect("seed required");

    let planted_cache = MemoryResultCache::new();
    run(
        &echo_ok("untrusted", "informational"),
        workdir.path(),
        &planted_cache,
    )
    .expect("seed untrusted");
    let planted = planted_cache.entries().pop().expect("untrusted entry");
    assert_eq!(planted.body.check.class, CheckClass::Informational);
    assert_eq!(planted.combined_output, "ok\n");

    let mut slotted = planted.clone();
    slotted.check_name = "required".to_string();
    plant_json(cache_dir.path(), &slotted);

    let honest = MemoryResultCache::new();
    run(&required, workdir.path(), &honest).expect("seed honest");
    honest
        .entries()
        .pop()
        .expect("required entry")
        .verify_fresh(&planted.into_check_result())
        .expect_err("same output from another check must not verify");

    let hit = run(&required, workdir.path(), &cache).expect("required must miss the plant");
    assert_eq!(hit[0].body.check.class, CheckClass::Required);
    assert_eq!(
        marker_runs(workdir.path()),
        4,
        "planted informational output must not satisfy the required check"
    );
}
