// SPDX-License-Identifier: Apache-2.0
//! `heddle remote import-source` — weft fetches and imports public Git.

use anyhow::{Context, Result, anyhow};
use api::heddle::api::v1alpha2 as contract;
use heddle_cli_contract::cli::commands::wire::remote::ImportSourceOutput;
use hosted_client::hosted_runtime::{
    auth::resolve_server,
    hosted::{HostedAuthMode, HostedClient, HostedSession},
};
use objects::{Progress, object::ThreadName};

use super::provision_hosted_source_destination;
use crate::{
    cli::{
        Cli, CliContext, ImportSourceArgs,
        progress_render::{TerminalSink, finish_line, format_transfer_bytes},
        render::write_json_stdout,
        should_output_json, style,
    },
    config::UserConfig,
    remote::RemoteTarget,
};

pub async fn cmd_import_source(cli: &Cli, args: ImportSourceArgs) -> Result<()> {
    let (server, destination) = import_destination(&args.to, args.server.as_deref())?;
    let config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?
    .with_allow_insecure(args.insecure);
    let mut client = session.connect(&server).await?;
    let result = import_connected(cli, &mut client, &server, &destination, &args).await;
    client.close().await;
    result
}

async fn import_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    destination: &str,
    args: &ImportSourceArgs,
) -> Result<()> {
    let (destination, created) = provision_hosted_source_destination(client, destination)
        .await
        .context("provision hosted import destination")?;
    let thread_name = args.name.as_deref().unwrap_or("main").trim();
    if thread_name.is_empty() {
        return Err(anyhow!(
            "hosted source import thread name must not be empty"
        ));
    }
    let thread_name =
        ThreadName::try_new(thread_name).context("validate hosted source import thread name")?;
    let started = client
        .import_source(
            &destination,
            &args.clone_url,
            thread_name.as_str(),
            cli.operation_id_wire(),
        )
        .await
        .context("submit hosted source import")?;
    let thread = thread_name.into_string();
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
            let output = operation_output(
                record,
                &args.clone_url,
                &destination,
                &thread,
                &started.client_operation_id,
            );
            if json {
                write_json_stdout(&output).map_err(|error| {
                    wire::ProtocolError::Io(std::io::Error::other(error.to_string()))
                })?;
            } else {
                progress.set_phase(format_progress(record));
            }
            Ok(())
        })
        .await
        .context("observe hosted source import")?;

    match contract::operation_record::State::try_from(terminal.state) {
        Ok(contract::operation_record::State::Completed) => {
            finish_line(&progress, "[done] imported Git history on weft");
            if !json {
                println!(
                    "{} imported {} into {} on {}",
                    style::ok_marker(),
                    style::bold(&args.clone_url),
                    style::bold(&destination),
                    style::dim(server)
                );
                println!("{}", style::field("thread", &thread));
                super::super::action_line::print_next(&format!(
                    "heddle clone https://{server}/{destination} <dir>"
                ));
            }
            Ok(())
        }
        Ok(contract::operation_record::State::Failed) => {
            let failure = format_failure(terminal.failure.as_ref());
            Err(anyhow!(
                "hosted source import failed: {failure}. RetryImportSource is available for an explicit retry; no retry was run"
            ))
        }
        Ok(contract::operation_record::State::Canceled) => {
            Err(anyhow!("hosted source import was canceled"))
        }
        _ => Err(anyhow!(
            "hosted source import ended in a nonterminal operation state"
        )),
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
    source: &str,
    destination: &str,
    thread: &str,
    operation_id: &str,
) -> ImportSourceOutput {
    let state = operation_state(record.state);
    ImportSourceOutput {
        output_kind: "import_source",
        event: if is_terminal(record.state) {
            "terminal"
        } else {
            "progress"
        },
        source: source.to_string(),
        destination: destination.to_string(),
        thread: thread.to_string(),
        client_operation_id: operation_id.to_string(),
        state: state.to_string(),
        completed_units: record.completed_units,
        total_units: record.total_units,
        unit: record.unit.clone(),
        terminal: is_terminal(record.state),
        results: record.results.iter().map(format_entity_ref).collect(),
        failure: record
            .failure
            .as_ref()
            .map(|failure| format_failure(Some(failure))),
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
            state: contract::operation_record::State::Failed as i32,
            failure: Some(api::heddle::api::common::CallFailure {
                code: api::heddle::api::common::CallFailureCode::Unavailable as i32,
                message: "source host is unreachable".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let output = operation_output(&record, "source", "dest", "thread", "operation");
        assert!(output.terminal);
        assert_eq!(output.state, "failed");
        assert!(
            output
                .failure
                .is_some_and(|failure| failure.contains("source host is unreachable"))
        );
    }
}
