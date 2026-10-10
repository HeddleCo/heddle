// SPDX-License-Identifier: Apache-2.0
//! Hosted `heddle import` operations.

use anyhow::{Context, Result, anyhow};
use api::heddle::api::v1alpha2 as contract;
use heddle_cli_contract::cli::commands::wire::remote::{
    ImportCancelOutput, ImportOperationOutput, ImportReportOutput, ImportRetryOutput,
    SkippedImportRefOutput,
};
use hosted_client::hosted_runtime::{
    auth::resolve_server,
    hosted::{HostedAuthMode, HostedClient, HostedSession, ImportSourceRefs},
};
use objects::Progress;

use super::provision_hosted_source_destination;
use crate::{
    cli::{
        Cli, CliContext, ImportOperationArgs, ImportUrlArgs,
        commands::{
            compact::{CompactOutput, CompactProjection},
            next_action::{NextActionValidationContext, write_command_json},
        },
        output_is_compact,
        progress_render::{TerminalSink, clear_line, format_transfer_bytes},
        should_output_json, style,
    },
    config::UserConfig,
    remote::RemoteTarget,
};

pub async fn cmd_import_url(cli: &Cli, args: ImportUrlArgs) -> Result<()> {
    let (server, destination) = import_destination(&args.to, args.server.as_deref())?;
    let refs = ImportSourceRefs::discover(&args.url)
        .await
        .context("validate hosted import source refs")?;
    let config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let mut client = session.connect(&server).await?;
    let result = import_connected(cli, &mut client, &server, &destination, &args, &refs).await;
    client.close().await;
    result
}

async fn import_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    destination: &str,
    args: &ImportUrlArgs,
    refs: &ImportSourceRefs,
) -> Result<()> {
    client
        .require_import_authority_protocol()
        .await
        .context("negotiate hosted import authority before provisioning")?;
    api::import_authority::validate_import_source(&contract::ImportSourceRequest::default())?;
    let (destination, created) = provision_hosted_source_destination(client, destination)
        .await
        .context("provision hosted import destination")?;
    let started = client
        .import_source(&destination, &args.url, refs, cli.operation_id_wire())
        .await
        .context("submit hosted source import")?;
    let json = should_output_json(cli, None);
    let progress = if json {
        Progress::null()
    } else {
        println!(
            "{} {} hosted spool {}",
            style::ok_marker(),
            if created { "created" } else { "using" },
            style::bold(&destination)
        );
        Progress::with_sink(Box::new(TerminalSink::new()))
    };

    let terminal = client
        .observe_import_source(&started, |record| {
            if !is_terminal(record.state) {
                progress.set_phase(format_progress(record));
            }
            Ok(())
        })
        .await
        .context("observe hosted source import")?;

    let output = operation_output(&terminal, Some(&args.url), &destination);
    clear_line(&progress);
    if json {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["import", "url"]),
        )?;
    } else {
        println!("{}", output.summary);
        if output.success {
            super::super::action_line::print_next(&format!(
                "heddle clone https://{server}/{destination} <dir>"
            ));
        }
    }
    import_outcome(&terminal)
}

fn import_outcome(record: &contract::OperationRecord) -> Result<()> {
    if operation_state(record.state) == "completed"
        && matches!(
            record.import_report.as_ref().and_then(|report| {
                contract::import_report::Fidelity::try_from(report.fidelity).ok()
            }),
            Some(
                contract::import_report::Fidelity::Faithful
                    | contract::import_report::Fidelity::Partial
            )
        )
    {
        Ok(())
    } else {
        Err(crate::exit::OutcomeExit::new(crate::exit::HeddleExitCode::Protocol).into())
    }
}

pub async fn cmd_import_status(cli: &Cli, args: ImportOperationArgs) -> Result<()> {
    let (server, destination) = import_destination(&args.to, args.server.as_deref())?;
    let config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let client = session.connect(&server).await?;
    let json = should_output_json(cli, None);
    let compact = output_is_compact(cli);
    let result = client
        .observe_import_operation(&destination, &args.operation, true, |record| {
            let output = operation_output(record, None, &destination);
            if json {
                write_command_json(
                    &output,
                    compact,
                    NextActionValidationContext::without_repo(&["import", "status"]),
                )
                .map_err(|error| {
                    wire::ProtocolError::Io(std::io::Error::other(error.to_string()))
                })?;
            } else {
                println!("{}", format_progress(record));
            }
            Ok(())
        })
        .await
        .context("observe hosted source import");
    client.close().await;
    result.and_then(|record| import_outcome(&record))
}

pub async fn cmd_import_retry(cli: &Cli, args: ImportOperationArgs) -> Result<()> {
    let (server, destination) = import_destination(&args.to, args.server.as_deref())?;
    let config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let client = session.connect(&server).await?;
    let original = client
        .observe_import_operation(&destination, &args.operation, false, |_| Ok(()))
        .await
        .context("observe import operation before retry")?;
    let started = client
        .retry_import_source(&original, cli.operation_id_wire())
        .await
        .context("submit hosted source import retry")?;
    client.close().await;
    let output = ImportRetryOutput {
        output_kind: "import_retry",
        action: "retry",
        status: "submitted",
        success: true,
        destination: destination.clone(),
        original_operation_id: args.operation,
        operation_id: started.operation.id,
        client_operation_id: started.client_operation_id,
    };
    if should_output_json(cli, None) {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["import", "retry"]),
        )
    } else {
        println!(
            "{} submitted retry {} for import operation {} on {}",
            style::ok_marker(),
            style::bold(&output.operation_id),
            style::bold(&output.original_operation_id),
            style::dim(&server)
        );
        super::super::action_line::print_next(&format!(
            "heddle import status {} --to {}",
            output.operation_id, destination
        ));
        Ok(())
    }
}

pub async fn cmd_import_cancel(cli: &Cli, args: ImportOperationArgs) -> Result<()> {
    let (server, destination) = import_destination(&args.to, args.server.as_deref())?;
    let config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let client = session.connect(&server).await?;
    let result = async {
        let original = client
            .observe_import_operation(&destination, &args.operation, false, |_| Ok(()))
            .await
            .context("observe import operation before cancellation")?;
        let terminal = is_terminal(original.state);
        let client_operation_id = if terminal {
            None
        } else {
            Some(
                client
                    .cancel_import_operation(&original, cli.operation_id_wire())
                    .await
                    .context("request hosted import cancellation")?,
            )
        };
        Ok::<_, anyhow::Error>(ImportCancelOutput {
            output_kind: "import_cancel",
            action: "cancel",
            status: if terminal {
                "already_terminal"
            } else {
                "requested"
            },
            state: operation_state(original.state),
            success: true,
            destination,
            operation_id: args.operation,
            client_operation_id,
        })
    }
    .await;
    client.close().await;
    let output = result?;
    if should_output_json(cli, None) {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["import", "cancel"]),
        )
    } else {
        if output.status == "already_terminal" {
            println!(
                "{} import operation {} is already {} on {}",
                style::ok_marker(),
                style::bold(&output.operation_id),
                output.state,
                style::dim(&server)
            );
        } else {
            println!(
                "{} cancellation requested for import operation {} on {}",
                style::ok_marker(),
                style::bold(&output.operation_id),
                style::dim(&server)
            );
        }
        let mut next = format!(
            "heddle import status {} --to {}",
            output.operation_id, args.to
        );
        if let Some(server) = args.server {
            next.push_str(&format!(" --server {server}"));
        }
        super::super::action_line::print_next(&next);
        Ok(())
    }
}

fn import_destination(value: &str, server: Option<&str>) -> Result<(String, String)> {
    if value.starts_with("https://") {
        if server.is_some() {
            return Err(anyhow!(
                "--server cannot be combined with a hosted --to URL"
            ));
        }
        return match RemoteTarget::parse(value) {
            Ok(RemoteTarget::Network {
                authority,
                repo_path: Some(path),
            }) => Ok((authority, path)),
            _ => Err(anyhow!(
                "hosted --to URL must include a spool path, for example https://api.heddle.sh/<handle>/<name>"
            )),
        };
    }
    Ok((
        resolve_server(server).context("resolve hosted import server")?,
        value.to_string(),
    ))
}

fn operation_output(
    record: &contract::OperationRecord,
    source: Option<&str>,
    destination: &str,
) -> ImportOperationOutput {
    let state = operation_state(record.state);
    ImportOperationOutput {
        output_kind: "import_operation",
        event: if is_terminal(record.state) {
            "terminal"
        } else {
            "progress"
        },
        source: source.map(str::to_string),
        destination: destination.to_string(),
        operation_id: record
            .r#ref
            .as_ref()
            .map(|reference| reference.id.clone())
            .unwrap_or_default(),
        client_operation_id: record.client_operation_id.clone(),
        state: state.to_string(),
        completed_units: record.completed_units,
        total_units: record.total_units,
        unit: record.unit.clone(),
        terminal: is_terminal(record.state),
        success: import_outcome(record).is_ok(),
        summary: format_progress(record),
        import_report: record
            .import_report
            .as_ref()
            .map(|report| ImportReportOutput {
                fidelity: report_fidelity(report.fidelity),
                commits: report.commits,
                branches: report.branches,
                tags: report.tags,
                skipped_refs: report
                    .skipped_refs
                    .iter()
                    .map(|reference| SkippedImportRefOutput {
                        name: reference.name.clone(),
                        reason: reference.reason.clone(),
                    })
                    .collect(),
            }),
        results: record.results.iter().map(format_entity_ref).collect(),
        failure: record
            .failure
            .as_ref()
            .map(|failure| format_failure(Some(failure))),
    }
}

impl CompactProjection for ImportOperationOutput {
    fn compact(&self) -> CompactOutput {
        let mut output = CompactOutput::new(self.output_kind);
        output.status = Some(if self.terminal && !self.success {
            "failed".into()
        } else if let Some(report) = self.import_report.as_ref() {
            report.fidelity.into()
        } else {
            self.state.clone()
        });
        if self.terminal {
            output.blockers = if self.success {
                Vec::new()
            } else {
                vec![self.summary.clone()]
            };
            if let Some(report) = self.import_report.as_ref() {
                output.blockers.extend(
                    report.skipped_refs.iter().map(|reference| {
                        format!("skipped {}: {}", reference.name, reference.reason)
                    }),
                );
            }
        }
        output.operation_id = Some(self.operation_id.clone());
        output
    }
}

impl CompactProjection for ImportCancelOutput {
    fn compact(&self) -> CompactOutput {
        let mut output = CompactOutput::new(self.output_kind);
        output.status = Some(self.status.to_string());
        output.operation_id = Some(self.operation_id.clone());
        output
    }
}

impl CompactProjection for ImportRetryOutput {
    fn compact(&self) -> CompactOutput {
        let mut output = CompactOutput::new(self.output_kind);
        output.status = Some(self.status.to_string());
        output.operation_id = Some(self.operation_id.clone());
        output
    }
}

fn operation_state(value: i32) -> &'static str {
    match contract::operation_record::State::try_from(value) {
        Ok(contract::operation_record::State::Queued) => "queued",
        Ok(contract::operation_record::State::Running) => "running",
        Ok(contract::operation_record::State::Completed) => "completed",
        Ok(contract::operation_record::State::Failed) => "failed",
        Ok(contract::operation_record::State::Canceled) => "canceled",
        Ok(contract::operation_record::State::WaitingForHuman) => "waiting_for_human",
        Ok(contract::operation_record::State::Paused) => "paused",
        _ => "unspecified",
    }
}

fn is_terminal(value: i32) -> bool {
    matches!(
        contract::operation_record::State::try_from(value),
        Ok(contract::operation_record::State::Completed
            | contract::operation_record::State::Failed
            | contract::operation_record::State::Canceled)
    )
}

fn format_progress(record: &contract::OperationRecord) -> String {
    let state = operation_state(record.state);
    if is_terminal(record.state) {
        return format_import_report(record);
    }
    let unit = record.unit.trim();
    match record.total_units {
        Some(total) if total > 0 => {
            let percent = record.completed_units.saturating_mul(100) / total;
            let (done, rendered_total) = if unit == "bytes" {
                (
                    format_transfer_bytes(record.completed_units),
                    format_transfer_bytes(total),
                )
            } else {
                (record.completed_units.to_string(), total.to_string())
            };
            format!("[{state}] importing Git history: {done}/{rendered_total} {unit} ({percent}%)")
        }
        _ if record.completed_units > 0 => {
            let done = if unit == "bytes" {
                format_transfer_bytes(record.completed_units)
            } else {
                record.completed_units.to_string()
            };
            format!("[{state}] importing Git history: {done} {unit}")
        }
        _ => format!("[{state}] importing Git history"),
    }
}

fn report_fidelity(value: i32) -> &'static str {
    match contract::import_report::Fidelity::try_from(value) {
        Ok(contract::import_report::Fidelity::Faithful) => "faithful",
        Ok(contract::import_report::Fidelity::Partial) => "partial",
        Ok(contract::import_report::Fidelity::Failed) => "failed",
        _ => "unspecified",
    }
}

fn format_import_report(record: &contract::OperationRecord) -> String {
    let state = operation_state(record.state);
    let Some(report) = record.import_report.as_ref() else {
        let wording = if state == "failed" {
            "Git import failed; no fidelity report"
        } else if state == "canceled" {
            "Git import canceled; no fidelity report"
        } else {
            "Git import has no fidelity report; success is unverified"
        };
        let mut text = format!("[{state}] {wording}");
        if let Some(failure) = record.failure.as_ref() {
            text.push_str(&format!("\n  {}", format_failure(Some(failure))));
        }
        return text;
    };
    let fidelity = report_fidelity(report.fidelity);
    let wording = match (state, fidelity) {
        ("completed", "faithful") => "imported Git history",
        ("completed", "partial") => "partial Git import",
        (_, "failed") | ("failed" | "canceled", _) => "Git import failed",
        _ => "Git import fidelity is unverified",
    };
    let mut text = format!(
        "[{state}] {wording} (fidelity: {fidelity}): {} commits, {} branches, {} tags",
        report.commits, report.branches, report.tags
    );
    for reference in &report.skipped_refs {
        text.push_str(&format!(
            "\n  skipped {}: {}",
            reference.name, reference.reason
        ));
    }
    if let Some(failure) = record.failure.as_ref() {
        text.push_str(&format!("\n  {}", format_failure(Some(failure))));
    }
    text
}

fn format_failure(failure: Option<&api::heddle::api::common::CallFailure>) -> String {
    failure.map_or_else(
        || "the executor did not provide failure details".to_string(),
        |failure| format!("{:?}: {}", failure.code(), failure.message),
    )
}

fn format_thread_ref(reference: &contract::ThreadRef) -> String {
    let spool = reference
        .spool
        .as_ref()
        .map(|spool| spool.id.as_str())
        .unwrap_or("unknown-spool");
    let id = reference
        .id
        .as_ref()
        .map(|id| hex::encode(&id.value))
        .unwrap_or_else(|| "unknown-thread".to_string());
    format!("{spool}/{id}")
}

fn format_entity_ref(reference: &contract::EntityRef) -> String {
    match reference.entity.as_ref() {
        Some(contract::entity_ref::Entity::Spool(spool)) => format!("spool:{}", spool.id),
        Some(contract::entity_ref::Entity::Thread(thread)) => {
            format!("thread:{}", format_thread_ref(thread))
        }
        _ => "other".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_terminal_report(
        fidelity: contract::import_report::Fidelity,
        wording: &str,
        success: bool,
    ) {
        let record = contract::OperationRecord {
            state: contract::operation_record::State::Completed as i32,
            import_report: Some(contract::ImportReport {
                fidelity: fidelity as i32,
                commits: 5,
                branches: 3,
                tags: 2,
                skipped_refs: if fidelity == contract::import_report::Fidelity::Partial {
                    vec![
                        contract::SkippedImportRef {
                            name: "refs/tags/blob".into(),
                            reason: "non-commit tag".into(),
                        },
                        contract::SkippedImportRef {
                            name: "refs/pull/12/head".into(),
                            reason: "provider ref".into(),
                        },
                    ]
                } else {
                    vec![]
                },
            }),
            ..Default::default()
        };
        let output = operation_output(&record, None, "dest");
        assert_eq!(output.success, success);
        assert!(output.summary.contains(wording), "{}", output.summary);
        assert!(output.summary.contains("5 commits, 3 branches, 2 tags"));
        if fidelity != contract::import_report::Fidelity::Faithful {
            assert!(!output.summary.contains("imported Git history"));
        }
        let json = serde_json::to_value(&output).expect("JSON output");
        assert_eq!(json["output_kind"], "import_operation");
        assert_eq!(json["success"], success);
        assert_eq!(json["import_report"]["commits"], 5);
        if fidelity == contract::import_report::Fidelity::Partial {
            for reference in &record.import_report.as_ref().expect("report").skipped_refs {
                assert!(output.summary.contains(&reference.name));
                assert!(output.summary.contains(&reference.reason));
            }
            assert_eq!(
                json["import_report"]["skipped_refs"]
                    .as_array()
                    .expect("skipped refs")
                    .len(),
                2
            );
        }
    }

    #[test]
    fn faithful_report_is_the_only_imported_git_history_claim() {
        assert_terminal_report(
            contract::import_report::Fidelity::Faithful,
            "imported Git history",
            true,
        );
    }

    #[test]
    fn partial_report_lists_every_skipped_ref_and_reason() {
        assert_terminal_report(
            contract::import_report::Fidelity::Partial,
            "partial Git import",
            true,
        );
    }

    #[test]
    fn failed_report_on_completed_operation_is_not_success() {
        assert_terminal_report(
            contract::import_report::Fidelity::Failed,
            "Git import failed",
            false,
        );
    }

    #[test]
    fn completed_operation_without_report_is_not_success() {
        let record = contract::OperationRecord {
            state: contract::operation_record::State::Completed as i32,
            ..Default::default()
        };
        let output = operation_output(&record, None, "dest");
        assert!(!output.success);
        assert!(output.summary.contains("no fidelity report"));
        assert!(!output.summary.contains("imported Git history"));
        assert!(import_outcome(&record).is_err());
    }

    #[test]
    fn running_progress_includes_live_units_and_percentage() {
        let record = contract::OperationRecord {
            state: contract::operation_record::State::Running as i32,
            completed_units: 8 * 1024 * 1024,
            total_units: Some(32 * 1024 * 1024),
            unit: "bytes".into(),
            ..Default::default()
        };
        assert_eq!(
            format_progress(&record),
            "[running] importing Git history: 8.0 MiB/32.0 MiB bytes (25%)"
        );
    }

    #[test]
    fn failed_operation_is_terminal_and_retains_executor_message() {
        let record = contract::OperationRecord {
            r#ref: Some(contract::RecordRef {
                spool: Some(contract::SpoolRef {
                    id: "destination".into(),
                }),
                id: "durable-operation".into(),
            }),
            state: contract::operation_record::State::Failed as i32,
            failure: Some(api::heddle::api::common::CallFailure {
                code: api::heddle::api::common::CallFailureCode::Unavailable as i32,
                message: "source host is unreachable".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let output = operation_output(&record, Some("source"), "dest");
        assert!(output.terminal);
        assert_eq!(output.event, "terminal");
        assert_eq!(output.operation_id, "durable-operation");
        assert_eq!(output.state, "failed");
        assert!(
            output
                .failure
                .is_some_and(|failure| failure.contains("source host is unreachable"))
        );
    }
}
