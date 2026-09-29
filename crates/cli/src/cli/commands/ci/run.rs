// SPDX-License-Identifier: Apache-2.0
//! CI execution, verdict signing, and hosted evidence recording.

use std::process::Command;

use ::wire::{ProtocolError, RemoteFailureCode};
use anyhow::{Context, Result, anyhow, ensure};
use api::heddle::api::{common::CallFailureCode, v1alpha2 as wire};
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
use hosted_client::{
    client::{HostedAuthMode, HostedClient},
    hosted_runtime::hosted::{HostedError, active_evidence_author},
};
use objects::object::ContentHash;
use prost::Message;
use repo::Repository;
use serde::{Deserialize, Serialize};
use thread_api::evidence::{self, CheckAuthor, CheckEvidence, CheckOutcome};

use super::{compile, render, target::EvaluationTarget};
use crate::{
    cli::{
        commands::{
            RecoveryAdvice,
            next_action::{NextActionValidationContext, write_full_command_json},
        },
        should_output_json,
    },
    config::UserConfig,
    exit::OutcomeExit,
    remote::{RemoteTarget, resolve_remote_with_key},
};

pub(crate) async fn run(cli: &Cli, args: &CiRunArgs) -> Result<()> {
    ensure!(
        args.local || args.record,
        "choose `heddle ci run --local` or `heddle ci run --record`"
    );
    ensure!(
        args.record || cli.op_id.is_none(),
        "--op-id requires `ci run --record`"
    );
    let repo = cli.open_repo()?;
    let path = compile::prepare_definition(&repo, args.config.as_deref())?;
    let raw = std::fs::read(&path)
        .with_context(|| format!("read TreadleDefinition {}", path.display()))?;
    let mut loaded = ci_config::load(&raw)
        .with_context(|| format!("decode canonical TreadleDefinition {}", path.display()))?;
    let lock_path = ci_config::lock_path(&path);
    let lock = ci_config::load_lock_file(&lock_path)
        .with_context(|| format!("treadle.lock.json is required next to {}", path.display()))?;
    ci_config::verify_lock(&lock, &loaded.definition_digest).with_context(|| {
        format!(
            "treadle lockfile {} does not match {}",
            lock_path.display(),
            path.display()
        )
    })?;
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
    let mut target = EvaluationTarget::prepare(&repo, args.state.as_deref(), args.record)?;
    let signer = if args.record {
        None
    } else {
        Some(load_device_signer()?)
    };

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
        .context("run checks (including result-cache spot-check)");
    let unchanged = if results.is_ok() {
        target.ensure_unchanged(&repo)
    } else {
        Ok(())
    };
    let cleanup = target.cleanup(&repo);
    let results = results?;
    unchanged?;
    cleanup?;

    let verdicts = if args.record {
        let (client, address, server) = connect_hosted(&repo).await?;
        let (evidence_signer, author) = active_evidence_author(&client, &server)
            .await
            .map_err(|error| scope_error(error, &address))?;
        let kind = if author.actor.agent_id.is_some() {
            SignerKind::ServiceAccount
        } else {
            SignerKind::Device
        };
        let verdicts = sign_results(results, &target, &evidence_signer, kind)?;
        let identity = EvidenceIdentity {
            author: &author,
            signer: &evidence_signer,
        };
        let (verdicts, recorded) =
            record_evidence(cli, &repo, &target, &client, &address, identity, &verdicts).await?;
        render_recorded(cli, &verdicts, &recorded)?;
        verdicts
    } else {
        let signer = signer.as_ref().context("local CI signer absent")?;
        let verdicts = sign_results(results, &target, signer, SignerKind::Device)?;
        render::render(cli, &verdicts)?;
        verdicts
    };
    eprintln!("heddle ci: ran {}", path.display());
    report_advisory(&verdicts);
    if render::has_required_failure(&verdicts) {
        return Err(OutcomeExit::data_err().into());
    }
    Ok(())
}

fn report_advisory(verdicts: &[SignedVerdict]) {
    let advisory = render::non_passing_advisory(verdicts);
    if !advisory.is_empty() {
        eprintln!(
            "heddle ci: warning: {} advisory check(s) did not pass (not gating): {}",
            advisory.len(),
            advisory.join(", ")
        );
    }
}

const RECORD_EVIDENCE: &str = "/heddle.api.v1alpha2.EvidenceService/RecordEvidence";

#[derive(Serialize)]
struct RecordedEvidence {
    id: String,
    revision: String,
    check: String,
}

#[derive(Serialize, Deserialize)]
struct PendingCiRun {
    address: String,
    thread: [u8; 32],
    revision: String,
    checks: Vec<String>,
    verdicts: Vec<SignedVerdict>,
    requests: Vec<Vec<u8>>,
}

impl PendingCiRun {
    fn matches(
        &self,
        address: &str,
        thread: [u8; 32],
        revision: &str,
        checks: &[String],
        verdicts: &[SignedVerdict],
    ) -> bool {
        self.address == address
            && self.thread == thread
            && self.revision == revision
            && self.checks == checks
            && self.verdicts.len() == verdicts.len()
            && self.verdicts.iter().zip(verdicts).all(|(saved, current)| {
                saved.body.check.definition_digest == current.body.check.definition_digest
            })
    }
}

#[derive(Clone, Copy)]
struct EvidenceIdentity<'a> {
    author: &'a CheckAuthor,
    signer: &'a Ed25519Signer,
}

struct EvidenceDestination<'a> {
    address: &'a str,
    spool: uuid::Uuid,
    thread: [u8; 32],
}

async fn connect_hosted(repo: &Repository) -> Result<(HostedClient, String, String)> {
    ensure!(
        repo.hosted_enabled(),
        "recording requires a hosted-linked repository"
    );
    let name = verbs::resolve_default_remote_name(repo, None)?;
    let (target, server_key) = resolve_remote_with_key(repo, Some(&name))?;
    let RemoteTarget::Network {
        authority,
        repo_path: Some(address),
    } = target
    else {
        return Err(anyhow!("recording requires a hosted Spool remote"));
    };
    let server = server_key.clone().unwrap_or_else(|| authority.clone());
    let client = HostedClient::open_session(
        &authority,
        &UserConfig::load_default()?,
        server_key,
        HostedAuthMode::CredentialFallback,
    )
    .await?;
    Ok((client, address, server))
}

async fn record_evidence(
    cli: &Cli,
    repo: &Repository,
    target: &EvaluationTarget,
    client: &HostedClient,
    address: &str,
    identity: EvidenceIdentity<'_>,
    verdicts: &[SignedVerdict],
) -> Result<(Vec<SignedVerdict>, Vec<RecordedEvidence>)> {
    let spool = client
        .resolve_spool_ref(address)
        .await
        .map_err(|error| scope_error(error.into(), address))?;
    let spool_id = uuid::Uuid::parse_str(&spool.id)?;
    check_local_spool(repo, spool_id)?;
    let thread_name = super::super::thread_cmd::resolve_thread_name_or_current(
        repo,
        None,
        "ci run",
        "heddle ci run --record",
    )?;
    let thread = client
        .resolve_thread_ref(address, &thread_name)
        .await
        .map_err(|error| scope_error(error.into(), address))?;
    if thread.spool.as_ref() != Some(&spool) {
        return Err(record_refusal(
            "ci_record_spool_mismatch",
            "resolved Thread belongs to another Spool".to_string(),
            "Link this repository to its original hosted Spool before recording evidence.",
            "heddle status",
        ));
    }
    let thread_id: [u8; 32] = thread
        .id
        .as_ref()
        .context("hosted Thread has no ID")?
        .value
        .as_slice()
        .try_into()
        .context("hosted Thread ID must be 32 bytes")?;
    let destination = EvidenceDestination {
        address,
        spool: spool_id,
        thread: thread_id,
    };
    let revision = target.state.id();
    let operation = match cli.op_id.as_deref() {
        Some(raw) => uuid::Uuid::parse_str(raw).context("--op-id must be a UUID")?,
        None => uuid::Uuid::now_v7(),
    };
    let checks: Vec<String> = verdicts
        .iter()
        .map(|verdict| verdict.body.check.name.clone())
        .collect();
    let saved = if let Some(raw) = cli.op_id.as_deref() {
        let path = repo
            .heddle_dir()
            .join("state/ci-evidence")
            .join(format!("{operation}.bin"));
        if path.try_exists()? {
            let bytes = std::fs::read(&path).context("read pending CI evidence")?;
            let pending: PendingCiRun =
                rmp_serde::from_slice(&bytes).context("decode pending CI evidence")?;
            ensure!(
                pending.matches(
                    address,
                    thread_id,
                    &revision.to_string_full(),
                    &checks,
                    verdicts
                ),
                "--op-id {raw} belongs to another Spool, Thread, revision, check selection or CI definition"
            );
            pending
        } else {
            let pending = prepare_ci_run(
                &destination,
                revision,
                operation,
                target.state.tree,
                identity,
                verdicts,
            )?;
            let bytes = rmp_serde::to_vec_named(&pending)?;
            let directory = path.parent().context("CI evidence directory absent")?;
            objects::fs_atomic::create_private_dir_all(directory)?;
            objects::fs_atomic::write_file_atomic_secret(&path, &bytes)?;
            pending
        }
    } else {
        prepare_ci_run(
            &destination,
            revision,
            operation,
            target.state.tree,
            identity,
            verdicts,
        )?
    };
    let mut recorded = Vec::with_capacity(saved.requests.len());
    ensure!(
        saved.requests.len() == saved.verdicts.len(),
        "pending CI evidence is incomplete"
    );
    for (bytes, verdict) in saved.requests.iter().zip(&saved.verdicts) {
        verdict.verify()?;
        ensure!(
            verdict.body.state.content_hash == revision.to_string_full()
                && verdict.tree_digest == target.state.tree,
            "pending verdict differs from the exact evaluated State"
        );
        let request = wire::RecordEvidenceRequest::decode(bytes.as_slice())?;
        let projection = request
            .evidence
            .as_ref()
            .context("pending signed evidence absent")?;
        let value = evidence::verify_evidence(
            projection
                .evidence
                .as_ref()
                .context("pending original absent")?,
        )?;
        ensure!(
            value.spool == spool_id
                && value.thread == ContentHash::from_bytes(thread_id)
                && value.revision == revision
                && value.check == verdict.body.check.name
                && value.outcome == check_outcome(verdict.body.outcome.conclusion)
                && hex::encode(value.author.publisher) == verdict.public_key
                && value.id
                    == uuid::Uuid::new_v5(
                        &operation,
                        format!("evidence:{}", value.check).as_bytes()
                    )
                && request.client_operation_id
                    == uuid::Uuid::new_v5(&operation, format!("record:{}", value.check).as_bytes())
                        .to_string(),
            "pending evidence differs from the exact Spool, Thread, revision or verdict"
        );
        ensure!(
            projection
                .r#ref
                .as_ref()
                .is_some_and(|reference| reference.id == value.id.to_string()),
            "pending evidence ID differs from the signed record"
        );
        let response: wire::MutationResponse =
            client
                .call_unary(RECORD_EVIDENCE, &request)
                .await
                .map_err(|error| evidence_error(error, &revision.to_string_full(), address))?;
        let receipt = response
            .receipt
            .context("RecordEvidence returned no receipt")?;
        ensure!(
            receipt.client_operation_id == request.client_operation_id,
            "RecordEvidence returned a different operation ID"
        );
        let Some(wire::mutation_receipt::Outcome::Applied(applied)) = receipt.outcome else {
            return Err(anyhow!(
                "RecordEvidence did not apply evidence {}",
                value.id
            ));
        };
        ensure!(
            applied.resulting_versions.iter().any(|version| {
                matches!(
                    version.resource.as_ref().and_then(|resource| resource.entity.as_ref()),
                    Some(wire::entity_ref::Entity::Evidence(reference))
                        if reference.id == value.id.to_string()
                            && reference.spool.as_ref() == Some(&spool)
                )
            }),
            "RecordEvidence receipt did not name accepted evidence {}",
            value.id
        );
        recorded.push(RecordedEvidence {
            id: value.id.to_string(),
            revision: revision.to_string_full(),
            check: value.check,
        });
    }
    Ok((saved.verdicts, recorded))
}

fn prepare_ci_run(
    destination: &EvidenceDestination<'_>,
    revision: objects::object::StateId,
    operation: uuid::Uuid,
    tree: ContentHash,
    identity: EvidenceIdentity<'_>,
    verdicts: &[SignedVerdict],
) -> Result<PendingCiRun> {
    let mut requests = Vec::with_capacity(verdicts.len());
    for verdict in verdicts {
        ensure!(
            verdict.body.state.content_hash == revision.to_string_full()
                && verdict.tree_digest == tree,
            "verdict differs from exact evaluated State; refusing hosted recording"
        );
        let check = &verdict.body.check.name;
        let id = uuid::Uuid::new_v5(&operation, format!("evidence:{check}").as_bytes());
        let command = uuid::Uuid::new_v5(&operation, format!("record:{check}").as_bytes());
        let value = CheckEvidence {
            version: 2,
            id,
            spool: destination.spool,
            thread: ContentHash::from_bytes(destination.thread),
            revision,
            check: check.clone(),
            outcome: check_outcome(verdict.body.outcome.conclusion),
            detail: verdict
                .body
                .outcome
                .failure
                .as_ref()
                .map(|failure| failure.excerpt.clone())
                .unwrap_or_default(),
            artifacts: Vec::new(),
            supersedes: Vec::new(),
            author: identity.author.clone(),
            completed_at_ms: chrono::DateTime::parse_from_rfc3339(
                &verdict.body.execution.finished_at,
            )?
            .timestamp_millis(),
            canonical_body: Default::default(),
        };
        let signed = evidence::sign_evidence(&value, identity.signer)?;
        let projected = evidence::project(&signed)?;
        requests.push(
            wire::RecordEvidenceRequest {
                client_operation_id: command.to_string(),
                evidence: Some(projected),
            }
            .encode_to_vec(),
        );
    }
    Ok(PendingCiRun {
        address: destination.address.to_string(),
        thread: destination.thread,
        revision: revision.to_string_full(),
        checks: verdicts
            .iter()
            .map(|verdict| verdict.body.check.name.clone())
            .collect(),
        verdicts: verdicts.to_vec(),
        requests,
    })
}

fn check_outcome(conclusion: crypto::Conclusion) -> CheckOutcome {
    match conclusion {
        crypto::Conclusion::Success => CheckOutcome::Passed,
        crypto::Conclusion::Failure => CheckOutcome::Failed,
        crypto::Conclusion::Skipped | crypto::Conclusion::Cancelled => CheckOutcome::Skipped,
        crypto::Conclusion::TimedOut | crypto::Conclusion::InfraError => CheckOutcome::Error,
    }
}

fn evidence_error(error: HostedError, revision: &str, address: &str) -> anyhow::Error {
    match error {
        HostedError::Call {
            code: CallFailureCode::PermissionDenied,
            ..
        } => scope_refusal(address),
        HostedError::Call {
            code: CallFailureCode::FailedPrecondition,
            ref message,
            ..
        } if message == "evidence source is not available in its original Thread" => {
            record_refusal(
                "ci_record_revision_unpublished",
                format!(
                    "revision {revision} is not published in its original Thread on hosted Spool {address}"
                ),
                "Push the exact State before recording its CI verdict.",
                "heddle push",
            )
        }
        HostedError::Call {
            code: CallFailureCode::NotFound,
            ..
        } => record_refusal(
            "ci_record_revision_unpublished",
            format!("revision {revision} is unavailable on hosted Spool {address}"),
            "Confirm the Spool and publish the exact State before recording its CI verdict.",
            "heddle push",
        ),
        other => other.into(),
    }
}

fn scope_error(error: anyhow::Error, address: &str) -> anyhow::Error {
    let denied = error.downcast_ref::<ProtocolError>().is_some_and(|source| {
        matches!(
            source,
            ProtocolError::AuthorizationFailed(_)
                | ProtocolError::RemoteFailure {
                    code: RemoteFailureCode::PermissionDenied,
                    ..
                }
        )
    }) || error.downcast_ref::<HostedError>().is_some_and(|source| {
        matches!(
            source,
            HostedError::Call {
                code: CallFailureCode::PermissionDenied,
                ..
            }
        )
    });
    if denied {
        scope_refusal(address)
    } else {
        error
    }
}

fn scope_refusal(address: &str) -> anyhow::Error {
    let path = address.strip_prefix("spool/").unwrap_or(address);
    record_refusal(
        "ci_record_scope_denied",
        format!("CI evidence scope denied for Spool {address}"),
        format!(
            "Derive a runner credential for `spool:{path}` using your hosted server, or use your own credential."
        ),
        "heddle auth derive-agent --help",
    )
}

fn check_local_spool(repo: &Repository, hosted: uuid::Uuid) -> Result<()> {
    let path = repo.heddle_dir().join("spool-id");
    let local = match std::fs::read_to_string(&path) {
        Ok(value) => uuid::Uuid::parse_str(value.trim())
            .with_context(|| format!("parse local Spool identity {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if local != hosted {
        return Err(record_refusal(
            "ci_record_spool_mismatch",
            format!("local Spool {local} differs from hosted Spool {hosted}"),
            "Link this repository to its original hosted Spool before recording evidence.",
            "heddle status",
        ));
    }
    Ok(())
}

fn record_refusal(
    kind: &'static str,
    error: String,
    hint: impl Into<String>,
    command: impl Into<String>,
) -> anyhow::Error {
    let command = command.into();
    anyhow!(RecoveryAdvice::safety_refusal(
        kind,
        error.clone(),
        hint,
        error,
        "recording evidence on an unverified Spool or revision",
        "the signed CI verdict and local State remain available",
        command.clone(),
        vec![command],
    ))
}

fn render_recorded(
    cli: &Cli,
    verdicts: &[SignedVerdict],
    recorded: &[RecordedEvidence],
) -> Result<()> {
    if should_output_json(cli, None) {
        write_full_command_json(
            &serde_json::json!({"output_kind": "ci_run", "verdicts": verdicts, "recorded": recorded}),
            NextActionValidationContext::without_repo(&["ci", "run"]),
        )
    } else {
        render::render(cli, verdicts)?;
        for item in recorded {
            println!(
                "Recorded {} for {} ({})",
                item.id, item.revision, item.check
            );
        }
        Ok(())
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
    signer: &impl Signer,
    kind: SignerKind,
) -> Result<Vec<SignedVerdict>> {
    results
        .into_iter()
        .map(|result| {
            let signed_at = result.body.execution.finished_at.clone();
            let verdict = signed_verdict_from_signer(
                result.body,
                &target.state.change_id,
                &target.tree_digest,
                kind,
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

#[cfg(test)]
mod tests {
    use api::heddle::api::common::CallFailureCode;
    use hosted_client::hosted_runtime::hosted::HostedError;

    use super::{PendingCiRun, check_local_spool, evidence_error, scope_error};

    fn verdict(digest: &str) -> crypto::SignedVerdict {
        let body = crypto::CiVerdictBody {
            check: crypto::CheckDescriptor {
                name: "unit".into(),
                definition_digest: digest.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        crypto::SignedVerdict {
            format_version: crypto::SIGNED_VERDICT_FORMAT_VERSION,
            body,
            content_hash: objects::object::ContentHash::from_bytes([0; 32]),
            change_id: objects::object::ChangeId::from_bytes([0; 16]),
            tree_digest: objects::object::ContentHash::from_bytes([0; 32]),
            signer_kind: crypto::SignerKind::Device,
            signed_at: "2026-01-01T00:00:00Z".into(),
            algorithm: String::new(),
            public_key: String::new(),
            signature: String::new(),
        }
    }

    #[test]
    fn replay_rejects_changed_definition_with_same_revision_and_check() {
        let thread = [1; 32];
        let checks = vec!["unit".to_string()];
        let pending = PendingCiRun {
            address: "team/repo".into(),
            thread,
            revision: "hs-revision".into(),
            checks: checks.clone(),
            verdicts: vec![verdict("old-digest")],
            requests: Vec::new(),
        };
        assert!(pending.matches(
            "team/repo",
            thread,
            "hs-revision",
            &checks,
            &[verdict("old-digest")]
        ));
        assert!(!pending.matches(
            "team/repo",
            thread,
            "hs-revision",
            &checks,
            &[verdict("new-digest")]
        ));
    }

    #[test]
    fn resource_scope_denial_is_typed_without_disclosing_resource_availability() {
        let denied = ::wire::ProtocolError::RemoteFailure {
            code: ::wire::RemoteFailureCode::PermissionDenied,
            message: "scope denied".into(),
            details: Vec::new(),
        };
        let error = scope_error(denied.into(), "team/repo");
        let advice = error
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("typed scope refusal");
        assert_eq!(advice.kind, "ci_record_scope_denied");

        let unavailable = ::wire::ProtocolError::ObjectNotFound("unavailable".into());
        let error = scope_error(unavailable.into(), "team/repo");
        assert!(error.downcast_ref::<::wire::ProtocolError>().is_some());

        let identity_denied = failure(CallFailureCode::PermissionDenied);
        let error = scope_error(identity_denied.into(), "team/repo");
        let advice = error
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("typed identity scope refusal");
        assert_eq!(advice.kind, "ci_record_scope_denied");
    }

    fn failure(code: CallFailureCode) -> HostedError {
        HostedError::Call {
            code,
            message: "evidence source is not available in its original Thread".into(),
            error: None,
        }
    }

    #[test]
    fn hosted_refusals_have_distinct_recovery_kinds() {
        for (code, expected) in [
            (CallFailureCode::PermissionDenied, "ci_record_scope_denied"),
            (CallFailureCode::NotFound, "ci_record_revision_unpublished"),
            (
                CallFailureCode::FailedPrecondition,
                "ci_record_revision_unpublished",
            ),
        ] {
            let error = evidence_error(failure(code), "hs-example", "team/repo");
            let advice = error
                .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
                .expect("typed recovery advice");
            assert_eq!(advice.kind, expected);
        }
        let unrelated = HostedError::Call {
            code: CallFailureCode::FailedPrecondition,
            message: "original author refused".into(),
            error: None,
        };
        assert!(
            evidence_error(unrelated, "hs-example", "team/repo")
                .downcast_ref::<HostedError>()
                .is_some()
        );
    }

    #[test]
    fn local_spool_identity_must_match_hosted_spool() {
        let root = tempfile::tempdir().expect("repository directory");
        let repo = repo::Repository::init_default(root.path()).expect("init repository");
        let local = repo.native_spool_id().expect("local identity");
        check_local_spool(&repo, local).expect("matching Spool");
        let other = uuid::Uuid::now_v7();
        let error = check_local_spool(&repo, other).expect_err("mismatched Spool");
        let advice = error
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("typed recovery advice");
        assert_eq!(advice.kind, "ci_record_spool_mismatch");
    }
}
