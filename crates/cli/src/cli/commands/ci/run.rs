// SPDX-License-Identifier: Apache-2.0
//! End-to-end local CI execution and verdict signing.

use std::process::Command;

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use ci_config::CiConfig;
use ci_engine::{
    BASE_ALLOWLIST, ExecutionContext, FsResultCache, NoopProvider, RunControls, RunOptions,
    run_checks_with,
};
use crypto::{
    Basis, BasisKind, Ed25519Signer, SignedVerdict, Signer, SignerKind, StateRef,
    signed_verdict_from_signer,
};
use heddle_cli_args::{CiRunArgs, Cli};
use repo::Repository;

use super::{
    render,
    target::{EvaluationTarget, definition_path},
};
use crate::exit::OutcomeExit;

pub(crate) fn run_local(cli: &Cli, args: &CiRunArgs) -> Result<()> {
    ensure!(args.local, "`heddle ci run` currently requires `--local`");
    let repo = cli.open_repo()?;
    let path = definition_path(&repo, args.config.as_deref());
    let raw = std::fs::read(&path)
        .with_context(|| format!("read TreadleDefinition {}", path.display()))?;
    let mut loaded = ci_config::load(&raw)
        .with_context(|| format!("decode canonical TreadleDefinition {}", path.display()))?;
    if let Some(lock) = read_optional_lock(&ci_config::lock_path(&path))? {
        ci_config::verify_lock(&lock, &loaded.definition_digest).with_context(|| {
            format!(
                "treadle lockfile {} does not match {}",
                ci_config::lock_path(&path).display(),
                path.display()
            )
        })?;
    }
    apply_check_filter(&mut loaded.config, &args.checks)?;
    if !args.checks.is_empty() {
        let omitted: Vec<&str> = loaded
            .definition
            .jobs
            .iter()
            .flat_map(|job| job.checks.iter().map(|check| check.name.as_str()))
            .filter(|name| !args.checks.iter().any(|selected| selected == name))
            .collect();
        if !omitted.is_empty() {
            eprintln!(
                "heddle ci: --check selected {}; omitted {}",
                args.checks.join(", "),
                omitted.join(", ")
            );
        }
    }
    let selected: Vec<String> = loaded
        .config
        .checks
        .iter()
        .map(|check| check.name.clone())
        .collect();
    ci_config::admit_host_exec(&loaded.definition, &selected)
        .context("local host-exec refused this definition")?;
    let signer = load_device_signer()?;
    let mut target = EvaluationTarget::prepare(&repo, args.state.as_deref())?;

    let context = execution_context(&repo, &target, loaded.definition_digest.clone());
    let provider = NoopProvider;
    let now = now_rfc3339;
    let options = RunOptions {
        workdir: &target.workdir,
        services: &provider,
        now_rfc3339: &now,
    };
    let cache_root = repo.heddle_dir().join("cache/ci");
    let result_cache_root = repo.heddle_dir().join("cache/ci-results");
    let result_cache = FsResultCache::new(&result_cache_root);
    let controls = RunControls {
        cache_root: Some(&cache_root),
        result_cache: Some(&result_cache),
        ..RunControls::default()
    };
    let results = run_checks_with(&loaded.config, &context, &options, &controls)
        .context("run checks (including result-cache spot-check)")?;
    target.ensure_unchanged(&repo)?;
    target.cleanup(&repo)?;

    let verdicts = sign_results(results, &target, &signer)?;
    render::render(cli, &verdicts)?;
    eprintln!("heddle ci: ran {}", path.display());
    let advisory = render::non_passing_advisory(&verdicts);
    if !advisory.is_empty() {
        eprintln!(
            "heddle ci: warning: {} advisory check(s) did not pass (not gating): {}",
            advisory.len(),
            advisory.join(", ")
        );
    }
    if render::has_required_failure(&verdicts) {
        return Err(OutcomeExit::data_err().into());
    }
    Ok(())
}

fn read_optional_lock(path: &std::path::Path) -> Result<Option<ci_config::TreadleLockfile>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(ci_config::read_lock(&bytes).with_context(|| {
            format!("parse treadle lockfile {}", path.display())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read treadle lockfile {}", path.display()))
        }
    }
}

fn apply_check_filter(config: &mut CiConfig, filter: &[String]) -> Result<()> {
    if filter.is_empty() {
        return Ok(());
    }
    let available: Vec<_> = config
        .checks
        .iter()
        .map(|check| check.name.clone())
        .collect();
    for name in filter {
        ensure!(
            available.contains(name),
            "no check named {name:?}; available checks: {}",
            available.join(", ")
        );
    }
    config.checks.retain(|check| filter.contains(&check.name));
    Ok(())
}

fn load_device_signer() -> Result<Ed25519Signer> {
    let path = repo::identity::device_identity_path();
    let device = repo::identity::load_device(&path)
        .with_context(|| format!("load device identity {}", path.display()))?
        .with_context(|| {
            format!(
                "no linked device identity at {}; run `heddle auth login` first",
                path.display()
            )
        })?;
    let signer = Ed25519Signer::from_pem(&device.private_key_pem)
        .context("parse linked device signing key")?;
    ensure!(
        hex::encode(signer.public_key()) == device.public_key,
        "linked device identity public key does not match its private key"
    );
    Ok(signer)
}

fn execution_context(
    repo: &Repository,
    target: &EvaluationTarget,
    definition_digest: String,
) -> ExecutionContext {
    ExecutionContext {
        repo: repository_name(repo),
        state: StateRef {
            content_hash: target.state.id().to_string_full(),
            change_id: target.state.change_id.to_string_full(),
            logical_change_id: None,
        },
        basis: Basis {
            kind: BasisKind::Branch,
            evaluated_tree_digest: target.tree_digest.to_hex(),
        },
        definition_digest,
        toolchain: detect_toolchain(),
        pick_id: None,
        attempt: 1,
        runner: None,
        image_digest: None,
    }
}

fn repository_name(repo: &Repository) -> String {
    repo.config().hosted.namespace.clone().unwrap_or_else(|| {
        repo.root()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local")
            .to_string()
    })
}

fn detect_toolchain() -> Option<String> {
    let mut command = Command::new("rustc");
    command.arg("--version").env_clear();
    for name in BASE_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

fn sign_results(
    results: Vec<ci_engine::CheckResult>,
    target: &EvaluationTarget,
    signer: &dyn Signer,
) -> Result<Vec<SignedVerdict>> {
    results
        .into_iter()
        .map(|result| {
            let signed_at = result.body.execution.finished_at.clone();
            let verdict = signed_verdict_from_signer(
                result.body,
                &target.state.change_id,
                &target.tree_digest,
                SignerKind::Device,
                signed_at,
                signer,
            )?;
            verdict.verify()?;
            Ok(verdict)
        })
        .collect()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}
