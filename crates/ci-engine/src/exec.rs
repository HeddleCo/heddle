// SPDX-License-Identifier: Apache-2.0
//! Sequential local check orchestration.

use std::{collections::BTreeMap, path::Path, time::Instant};

use ci_config::{Check, CiConfig, Trigger};
use crypto::{Conclusion, FailureClass};

use crate::{
    cache::{prepare_caches, restore_worktree_cache_dirs, save_caches},
    classify::{Disposition, classify},
    env::HermeticEnv,
    model::{AttemptRecord, CheckResult, ExecutionContext, RunControls, RunOptions},
    proc_group::ProcGroupRegistry,
    process::{RunOutput, run_process},
    result::{CompletedRun, finalize, infra_result, skipped_result},
    result_cache::{ResultCache, ResultCacheError, SpotCheck, with_cache},
};

/// Run every check with default controls.
pub fn run_checks(
    config: &CiConfig,
    context: &ExecutionContext,
    options: &RunOptions<'_>,
) -> Result<Vec<CheckResult>, ResultCacheError> {
    run_checks_with(config, context, options, &RunControls::default())
}

/// Run checks with explicit trigger/cache/environment controls.
pub fn run_checks_with(
    config: &CiConfig,
    context: &ExecutionContext,
    options: &RunOptions<'_>,
    controls: &RunControls<'_>,
) -> Result<Vec<CheckResult>, ResultCacheError> {
    let default_environment = HermeticEnv::new();
    let environment = controls.hermetic_env.unwrap_or(&default_environment);
    let default_cache_root = options.workdir.join(".hci-cache");
    let cache_root = controls.cache_root.unwrap_or(&default_cache_root);
    let resolved = ResolvedRun {
        options,
        environment,
        cache_root,
        proc_groups: controls.proc_groups.as_ref(),
        result_cache: controls.result_cache,
        spot_check: controls.spot_check,
    };
    let results = config
        .checks
        .iter()
        .map(|check| match &controls.trigger {
            Some(trigger) if !check_runs_for_trigger(&check.triggers, trigger) => {
                Ok(skipped_result(check, context, &resolved))
            }
            _ => run_one_check(check, context, &resolved),
        })
        .collect::<Result<Vec<_>, _>>()?;
    restore_worktree_cache_dirs(options.workdir, &config.checks);
    Ok(results)
}

pub(crate) struct ResolvedRun<'a> {
    pub(crate) options: &'a RunOptions<'a>,
    pub(crate) environment: &'a HermeticEnv,
    pub(crate) cache_root: &'a Path,
    pub(crate) proc_groups: Option<&'a ProcGroupRegistry>,
    pub(crate) result_cache: Option<&'a dyn ResultCache>,
    pub(crate) spot_check: SpotCheck,
}

fn run_one_check(
    check: &Check,
    context: &ExecutionContext,
    run: &ResolvedRun<'_>,
) -> Result<CheckResult, ResultCacheError> {
    let service_environment = declared_service_env(check);
    let key_environment = run
        .environment
        .build(&check.env, &service_environment, &BTreeMap::new());
    with_cache(check, context, run, &key_environment, || {
        run_one_check_uncached(check, context, run, &service_environment)
    })
}

fn declared_service_env(check: &Check) -> BTreeMap<String, String> {
    check
        .services
        .iter()
        .flat_map(|service| {
            service
                .env
                .iter()
                .map(|entry| (entry.0.clone(), entry.1.clone()))
        })
        .collect()
}

fn run_one_check_uncached(
    check: &Check,
    context: &ExecutionContext,
    run: &ResolvedRun<'_>,
    service_environment: &BTreeMap<String, String>,
) -> CheckResult {
    let started_at = (run.options.now_rfc3339)();
    let started = Instant::now();
    let caches = match prepare_caches(
        &check.name,
        &check.cache_paths,
        run.options.workdir,
        run.cache_root,
    ) {
        Ok(caches) => caches,
        Err(error) => {
            return infra_result(
                check,
                context,
                run,
                started_at,
                started.elapsed(),
                "cache_paths",
                &error.to_string(),
            );
        }
    };
    let services = match run.options.services.up(&check.services) {
        Ok(services) => services,
        Err(error) => {
            return infra_result(
                check,
                context,
                run,
                started_at,
                started.elapsed(),
                "service_provisioning",
                &format!("service provisioning failed: {error}"),
            );
        }
    };
    let environment = run
        .environment
        .build(&check.env, service_environment, &caches.env);
    let (last, attempts) = run_attempts(check, run, &environment);
    let _ = run.options.services.down(services);
    if let Err(error) = save_caches(&caches) {
        return infra_result(
            check,
            context,
            run,
            started_at,
            started.elapsed(),
            "cache_save",
            &error.to_string(),
        );
    }
    finalize(
        check,
        context,
        CompletedRun {
            output: last,
            attempts,
            environment,
            started_at,
            finished_at: (run.options.now_rfc3339)(),
            duration: started.elapsed(),
        },
    )
}

fn run_attempts(
    check: &Check,
    run: &ResolvedRun<'_>,
    environment: &BTreeMap<String, String>,
) -> (RunOutput, Vec<AttemptRecord>) {
    let mut last = RunOutput::default();
    let mut records = Vec::new();
    for attempt in 1..=check.retry.max.saturating_add(1) {
        let started = Instant::now();
        last = run_process(check, run.options.workdir, environment, run.proc_groups);
        let flake = last.disposition != Disposition::Success
            && matches_flake_signature(check, &last.combined_output);
        records.push(AttemptRecord {
            attempt,
            conclusion: disposition_conclusion(last.disposition, &last.combined_output),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            flake_matched: flake,
        });
        if last.disposition == Disposition::Success || !flake {
            break;
        }
    }
    (last, records)
}

fn matches_flake_signature(check: &Check, output: &str) -> bool {
    check.retry.flake_signatures.iter().any(|pattern| {
        regex::Regex::new(pattern)
            .map(|regex| regex.is_match(output))
            .unwrap_or(false)
    })
}

fn disposition_conclusion(disposition: Disposition, output: &str) -> Conclusion {
    match classify(disposition, output) {
        None => Conclusion::Success,
        Some(FailureClass::Timeout) => Conclusion::TimedOut,
        Some(FailureClass::Infra) => Conclusion::InfraError,
        Some(_) => Conclusion::Failure,
    }
}

fn check_runs_for_trigger(check: &[Trigger], pick: &Trigger) -> bool {
    if check.is_empty() {
        return matches!(pick, Trigger::Push);
    }
    check.iter().any(|trigger| {
        matches!(
            (trigger, pick),
            (Trigger::Push, Trigger::Push)
                | (Trigger::Manual, Trigger::Manual)
                | (Trigger::Cron(_), Trigger::Cron(_))
        )
    })
}
