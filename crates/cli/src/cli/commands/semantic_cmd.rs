// SPDX-License-Identifier: Apache-2.0
//! Semantic-analysis CLI commands (`heddle semantic ...`).
//!
//! Thin shim over [`semantic`] — the analysis lives in the core
//! semantic crate so the same primitives are available to gRPC and the
//! web UI without a CLI round-trip.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use repo::Repository;
use semantic::analysis::{
    HotEventKind, HotSpotKey, HotSpotKeyValue, HotSpotParams, analyze_hot_spots,
};
use serde::Serialize;

use super::snapshot::ensure_current_state;
use crate::{
    cli::{Cli, HotEventKindArg, HotSpotKeyArg, SemanticCommands, should_output_json},
    config::UserConfig,
};

/// Top-level dispatch for `heddle semantic <subcommand>`.
pub fn cmd_semantic(cli: &Cli, command: SemanticCommands) -> Result<()> {
    match command {
        SemanticCommands::Hot {
            from,
            limit,
            by,
            kinds,
            include_paths,
            exclude_paths,
            top,
            include_actors,
        } => cmd_semantic_hot(
            cli,
            from,
            limit,
            by,
            kinds,
            include_paths,
            exclude_paths,
            top,
            include_actors,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_semantic_hot(
    cli: &Cli,
    from: Option<String>,
    limit: usize,
    by: HotSpotKeyArg,
    kinds: Vec<HotEventKindArg>,
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    top: usize,
    include_actors: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    // Resolve `from` (or HEAD) to a concrete ChangeId. Walking from
    // HEAD is the common case; allowing an explicit state lets users
    // ask "what's been hot in the last N commits before tag X?"
    let walk_from = match from.as_ref() {
        Some(spec) => {
            if matches!(spec.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
                ensure_current_state(
                    &repo,
                    &UserConfig::load_default().unwrap_or_default(),
                    Some("Bootstrap git-overlay before semantic analysis".to_string()),
                )?;
            }
            repo.resolve_state(spec)?
                .ok_or_else(|| anyhow!("could not resolve state {spec:?}"))?
        }
        None => ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before semantic analysis".to_string()),
        )?,
    };

    let group_by = match by {
        HotSpotKeyArg::File => HotSpotKey::File,
        HotSpotKeyArg::Function => HotSpotKey::Function,
    };
    let include_kinds: Vec<HotEventKind> = kinds.iter().copied().map(map_event_kind).collect();

    let params = HotSpotParams {
        limit_states: Some(limit),
        group_by,
        include_kinds,
        include_paths,
        exclude_paths,
        top_n: top,
        include_actors,
        diff_options: Default::default(),
    };

    let report = analyze_hot_spots(repo.store(), walk_from, &params)
        .context("computing semantic hot-spots")?;

    let output = HotSpotsOutput::from_report(&report);

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serializing hot-spots output")?
        );
    } else {
        print_human(&output);
    }
    Ok(())
}

fn map_event_kind(arg: HotEventKindArg) -> HotEventKind {
    match arg {
        HotEventKindArg::FileAdded => HotEventKind::FileAdded,
        HotEventKindArg::FileDeleted => HotEventKind::FileDeleted,
        HotEventKindArg::FileModified => HotEventKind::FileModified,
        HotEventKindArg::FileRenamed => HotEventKind::FileRenamed,
        HotEventKindArg::FunctionExtracted => HotEventKind::FunctionExtracted,
        HotEventKindArg::FunctionDeleted => HotEventKind::FunctionDeleted,
        HotEventKindArg::FunctionRenamed => HotEventKind::FunctionRenamed,
        HotEventKindArg::FunctionModified => HotEventKind::FunctionModified,
        HotEventKindArg::FunctionMoved => HotEventKind::FunctionMoved,
        HotEventKindArg::SignatureChanged => HotEventKind::SignatureChanged,
        HotEventKindArg::DependencyChanged => HotEventKind::DependencyChanged,
    }
}

fn human_event_kind(kind: HotEventKind) -> &'static str {
    match kind {
        HotEventKind::FileAdded => "file_added",
        HotEventKind::FileDeleted => "file_deleted",
        HotEventKind::FileModified => "file_modified",
        HotEventKind::FileRenamed => "file_renamed",
        HotEventKind::FunctionExtracted => "function_extracted",
        HotEventKind::FunctionDeleted => "function_deleted",
        HotEventKind::FunctionRenamed => "function_renamed",
        HotEventKind::FunctionModified => "function_modified",
        HotEventKind::FunctionMoved => "function_moved",
        HotEventKind::SignatureChanged => "signature_changed",
        HotEventKind::DependencyChanged => "dependency_changed",
    }
}

/// JSON-friendly mirror of [`semantic::HotSpotsReport`]. The
/// `semantic` types are deliberately not `Serialize` (keeps that
/// crate's deps minimal); we map them at the CLI boundary.
#[derive(Debug, Serialize)]
struct HotSpotsOutput {
    spots: Vec<HotSpotEntry>,
    states_walked: usize,
    total_events: usize,
}

#[derive(Debug, Serialize)]
struct HotSpotEntry {
    key_kind: &'static str,
    path: String,
    function: Option<String>,
    event_count: usize,
    state_count: usize,
    first_seen: String,
    last_seen: String,
    by_kind: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    by_actor: Option<BTreeMap<String, usize>>,
}

impl HotSpotsOutput {
    fn from_report(report: &semantic::HotSpotsReport) -> Self {
        let spots = report
            .spots
            .iter()
            .map(|spot| {
                let (key_kind, path, function) = match &spot.key {
                    HotSpotKeyValue::File { path } => {
                        ("file", path.to_string_lossy().into_owned(), None)
                    }
                    HotSpotKeyValue::Function { path, name } => (
                        "function",
                        path.to_string_lossy().into_owned(),
                        Some(name.clone()),
                    ),
                };
                let by_kind = spot
                    .by_kind
                    .iter()
                    .map(|(k, v)| (human_event_kind(*k).to_string(), *v))
                    .collect();
                HotSpotEntry {
                    key_kind,
                    path,
                    function,
                    event_count: spot.event_count,
                    state_count: spot.state_count,
                    first_seen: spot.first_seen.to_string_full(),
                    last_seen: spot.last_seen.to_string_full(),
                    by_kind,
                    by_actor: spot.by_actor.clone(),
                }
            })
            .collect();
        Self {
            spots,
            states_walked: report.states_walked,
            total_events: report.total_events,
        }
    }
}

fn print_human(output: &HotSpotsOutput) {
    if output.spots.is_empty() {
        println!(
            "no hot-spots found ({} states walked, {} total events)",
            output.states_walked, output.total_events
        );
        return;
    }
    println!(
        "Top {} hot-spots — walked {} state pair(s), aggregated {} event(s):",
        output.spots.len(),
        output.states_walked,
        output.total_events
    );
    println!();
    for entry in &output.spots {
        let label = match &entry.function {
            Some(name) => format!("{name} in {}", entry.path),
            None => entry.path.clone(),
        };
        println!(
            "  {:>4} events  {:>3} states  {}",
            entry.event_count, entry.state_count, label
        );
        if !entry.by_kind.is_empty() {
            let breakdown: Vec<String> = entry
                .by_kind
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            println!("            kinds: {}", breakdown.join(" "));
        }
        if let Some(actors) = &entry.by_actor
            && !actors.is_empty()
        {
            let mut top_actors: Vec<(&String, &usize)> = actors.iter().collect();
            top_actors.sort_by(|a, b| b.1.cmp(a.1));
            let summary: Vec<String> = top_actors
                .iter()
                .take(3)
                .map(|(name, count)| format!("{name} ({count})"))
                .collect();
            println!("            actors: {}", summary.join(", "));
        }
    }
}