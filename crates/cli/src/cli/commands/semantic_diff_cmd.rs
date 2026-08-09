// SPDX-License-Identifier: Apache-2.0
//! Parse-free symbol diff rendering for `heddle semantic diff`.

use anyhow::{Context, Result};
use objects::object::{SymbolAnchor, SymbolKindTag};
use repo::SymbolDelta;
use serde::Serialize;

use super::history_target::resolve_state_id;
use crate::cli::{Cli, should_output_json};

#[derive(Debug, Serialize)]
struct SemanticDiffOutput {
    output_kind: &'static str,
    from_state: String,
    to_state: String,
    deltas: Vec<SymbolDeltaOutput>,
}

#[derive(Debug, Serialize)]
struct SymbolDeltaOutput {
    change: &'static str,
    anchor: SymbolAnchor,
    kind: SymbolKindTag,
    old_hash: Option<String>,
    new_hash: Option<String>,
}

impl From<SymbolDelta> for SymbolDeltaOutput {
    fn from(delta: SymbolDelta) -> Self {
        let change = match (delta.old_hash, delta.new_hash) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            (Some(_), Some(_)) => "modified",
            (None, None) => "modified",
        };
        Self {
            change,
            anchor: delta.anchor,
            kind: delta.kind,
            old_hash: delta.old_hash.map(|hash| hash.to_hex()),
            new_hash: delta.new_hash.map(|hash| hash.to_hex()),
        }
    }
}

pub(super) fn cmd_semantic_diff(cli: &Cli, a: String, b: String) -> Result<()> {
    let repo = cli.open_repo()?;
    let a = resolve_state_id(&repo, &a)?;
    let b = resolve_state_id(&repo, &b)?;
    let deltas = repo
        .semantic_diff_symbols(&a, &b)
        .context("querying attached semantic indexes")?
        .into_iter()
        .map(SymbolDeltaOutput::from)
        .collect();
    let output = SemanticDiffOutput {
        output_kind: "semantic_diff",
        from_state: a.to_string_full(),
        to_state: b.to_string_full(),
        deltas,
    };

    if should_output_json(cli, Some(repo.config())) {
        write_json(&output)?;
    } else {
        render_human(&output);
    }
    Ok(())
}

fn write_json(output: &SemanticDiffOutput) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(output).context("serializing semantic diff output")?
    );
    Ok(())
}

fn render_human(output: &SemanticDiffOutput) {
    if output.deltas.is_empty() {
        println!(
            "No symbol changes between {} and {}.",
            output.from_state, output.to_state
        );
        return;
    }

    println!(
        "Symbol changes between {} and {} ({}):",
        output.from_state,
        output.to_state,
        output.deltas.len()
    );
    for delta in &output.deltas {
        let marker = match delta.change {
            "added" => '+',
            "removed" => '-',
            _ => '~',
        };
        println!(
            "  {marker} {:<10} {}::{} ({})",
            delta.change,
            delta.anchor.file,
            delta.anchor.symbol,
            symbol_kind_label(delta.kind)
        );
    }
}

fn symbol_kind_label(kind: SymbolKindTag) -> &'static str {
    match kind {
        SymbolKindTag::Function => "function",
        SymbolKindTag::Type => "type",
        SymbolKindTag::Enum => "enum",
        SymbolKindTag::Trait => "trait",
        SymbolKindTag::Class => "class",
        SymbolKindTag::Interface => "interface",
        SymbolKindTag::TypeAlias => "type_alias",
        SymbolKindTag::Const => "const",
        SymbolKindTag::Module => "module",
        SymbolKindTag::Other => "other",
    }
}
