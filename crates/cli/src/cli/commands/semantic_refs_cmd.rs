// SPDX-License-Identifier: Apache-2.0
//! Parse-free graph queries for `heddle semantic refs`.

use anyhow::{Context, Result, anyhow};
use objects::object::{
    SemanticGraphQueryKind, SemanticGraphQueryResponse, SemanticGraphRef, StateId, SymbolAnchor,
};
use repo::Repository;
use serde::Serialize;

use super::{history_target::resolve_state_id, snapshot::ensure_current_state};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[derive(Debug, Serialize)]
struct SemanticRefsOutput {
    output_kind: &'static str,
    #[serde(flatten)]
    body: SemanticGraphQueryResponse,
}

pub(super) fn cmd_semantic_refs(
    cli: &Cli,
    at: Option<String>,
    importers: Option<String>,
    callers: bool,
    anchor: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let state = match at.as_deref() {
        Some(spec) => resolve_state_id(&repo, spec)?,
        None => ensure_current_state(
            &repo,
            &UserConfig::load_default()?,
            Some("Bootstrap git-overlay before semantic graph query".to_string()),
        )?,
    };

    let (kind, parsed_anchor, path) = if let Some(path) = importers {
        (SemanticGraphQueryKind::ImportersOf, None, Some(path))
    } else if callers {
        (
            SemanticGraphQueryKind::CallersOf,
            Some(parse_anchor(&anchor.unwrap_or_default())?),
            None,
        )
    } else {
        (
            SemanticGraphQueryKind::RefsOf,
            Some(parse_anchor(&anchor.unwrap_or_default())?),
            None,
        )
    };

    let (refs, importers, index_present) = match kind {
        SemanticGraphQueryKind::ImportersOf => {
            let hits = repo
                .importers_of(&state, path.as_deref().unwrap_or_default())
                .context("querying attached importer index")?;
            (Vec::new(), hits.clone().unwrap_or_default(), hits.is_some())
        }
        SemanticGraphQueryKind::RefsOf | SemanticGraphQueryKind::CallersOf => {
            let hits = query_refs(&repo, &state, parsed_anchor.as_ref().expect("anchor"), kind)?;
            (hits.clone().unwrap_or_default(), Vec::new(), hits.is_some())
        }
    };

    let output = SemanticRefsOutput {
        output_kind: "semantic_refs",
        body: SemanticGraphQueryResponse {
            state_id: state.to_string_full(),
            kind,
            anchor: parsed_anchor,
            path,
            index_present,
            refs,
            importers,
        },
    };

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serializing semantic refs output")?
        );
    } else {
        render_human(&output);
    }
    Ok(())
}

fn query_refs(
    repo: &Repository,
    state: &StateId,
    anchor: &SymbolAnchor,
    kind: SemanticGraphQueryKind,
) -> Result<Option<Vec<SemanticGraphRef>>> {
    match kind {
        SemanticGraphQueryKind::CallersOf => repo
            .callers_of(state, anchor)
            .context("querying attached callers"),
        SemanticGraphQueryKind::RefsOf => repo
            .refs_of(state, anchor)
            .context("querying attached refs"),
        SemanticGraphQueryKind::ImportersOf => unreachable!("importers use a separate path"),
    }
}

fn parse_anchor(spec: &str) -> Result<SymbolAnchor> {
    let (file, symbol) = spec.split_once(':').ok_or_else(|| {
        anyhow!("symbol anchor must be path:symbol (for example src/api.rs:greet), got {spec:?}")
    })?;
    if file.is_empty() || symbol.is_empty() {
        return Err(anyhow!(
            "symbol anchor must be path:symbol (for example src/api.rs:greet), got {spec:?}"
        ));
    }
    Ok(SymbolAnchor::new(file, symbol))
}

fn render_human(output: &SemanticRefsOutput) {
    if !output.body.index_present {
        println!("No attached semantic index at {}.", output.body.state_id);
        return;
    }
    match output.body.kind {
        SemanticGraphQueryKind::ImportersOf => {
            let path = output.body.path.as_deref().unwrap_or_default();
            if output.body.importers.is_empty() {
                println!("No importers of {path} at {}.", output.body.state_id);
                return;
            }
            println!(
                "Importers of {path} at {} ({}):",
                output.body.state_id,
                output.body.importers.len()
            );
            for importer in &output.body.importers {
                println!("  {importer}");
            }
        }
        SemanticGraphQueryKind::RefsOf | SemanticGraphQueryKind::CallersOf => {
            let label = match output.body.kind {
                SemanticGraphQueryKind::CallersOf => "Callers",
                _ => "Refs",
            };
            let anchor = output.body.anchor.as_ref();
            let target = anchor
                .map(|anchor| format!("{}:{}", anchor.file, anchor.symbol))
                .unwrap_or_default();
            if output.body.refs.is_empty() {
                println!("{label} of {target} at {}: none.", output.body.state_id);
                return;
            }
            println!(
                "{label} of {target} at {} ({}):",
                output.body.state_id,
                output.body.refs.len()
            );
            for graph_ref in &output.body.refs {
                println!(
                    "  {}#{} {} {:?}",
                    graph_ref.source_path,
                    graph_ref.source_occurrence,
                    graph_ref.name,
                    graph_ref.kind
                );
            }
        }
    }
}
