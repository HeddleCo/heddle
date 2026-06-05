// SPDX-License-Identifier: Apache-2.0
//! `heddle visibility` — declare and inspect a state's audience tier.
//!
//! Mirrors `redact`: each mutation writes a per-state `StateVisibility`
//! sidecar record plus an `OpRecord` audit entry. `set` declares a tier,
//! `promote` appends a superseding less-restrictive declaration, `show`
//! reports the effective tier (public-by-absence when no record exists), and
//! `list` enumerates every non-public state.
//!
//! Respects `--output json` via `should_output_json`.

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use objects::object::{ChangeId, StateVisibility, VisibilityTier};
use oplog::{OpLogBackend, VisibilitySidecarSnapshots};
use repo::Repository;
use serde::Serialize;

use crate::cli::{
    Cli, VisibilityCommands, VisibilityListArgs, VisibilityPromoteArgs, VisibilitySetArgs,
    VisibilityShowArgs, should_output_json,
};

pub fn cmd_visibility(cli: &Cli, command: VisibilityCommands) -> Result<()> {
    let repo = cli.open_repo()?;
    match command {
        VisibilityCommands::Set(args) => cmd_visibility_set(cli, &repo, args),
        VisibilityCommands::Promote(args) => cmd_visibility_promote(cli, &repo, args),
        VisibilityCommands::Show(args) => cmd_visibility_show(cli, &repo, args),
        VisibilityCommands::List(args) => cmd_visibility_list(cli, &repo, args),
    }
}

/// The team id / scope label carried by a non-public tier, for output.
fn tier_label(tier: &VisibilityTier) -> Option<&str> {
    match tier {
        VisibilityTier::TeamScoped { team_id } => Some(team_id),
        VisibilityTier::Restricted { scope_label } => Some(scope_label),
        VisibilityTier::Public | VisibilityTier::Internal => None,
    }
}

fn resolve_state(repo: &Repository, spec: &str) -> Result<ChangeId> {
    repo.resolve_state(spec)
        .with_context(|| format!("resolve state '{}'", spec))?
        .ok_or_else(|| anyhow!("state '{}' not found", spec))
}

#[derive(Serialize)]
struct VisibilityMutationOutput {
    output_kind: &'static str,
    state: String,
    tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    record_id: String,
    declarer: String,
    declared_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
}

fn cmd_visibility_set(cli: &Cli, repo: &Repository, args: VisibilitySetArgs) -> Result<()> {
    let state = resolve_state(repo, &args.state)?;
    let tier = args
        .tier
        .into_tier(args.label)
        .map_err(|msg| anyhow!(msg))?;
    let declarer = repo
        .get_principal()
        .with_context(|| "resolve current principal")?;
    let declared_at = Utc::now();

    let record = StateVisibility {
        state,
        tier: tier.clone(),
        embargo_until: None,
        declarer: declarer.clone(),
        declared_at,
        signature: None,
        supersedes: None,
    };
    // Snapshot the whole per-state sidecar before/after the put so undo can
    // restore the before-image and redo the after-image (PR #529 P1).
    let prior = repo.get_state_visibility_bytes_for_state(&state)?;
    let record_id = repo.put_state_visibility(record)?;
    let new = repo.get_state_visibility_bytes_for_state(&state)?;
    let scope = repo.op_scope();
    repo.oplog().record_state_visibility_set(
        &state,
        &record_id,
        &tier,
        VisibilitySidecarSnapshots { prior, new },
        Some(&scope),
    )?;

    let output = VisibilityMutationOutput {
        output_kind: "visibility_set",
        state: state.short(),
        tier: tier.as_str().to_string(),
        label: tier_label(&tier).map(str::to_string),
        record_id: record_id.short(),
        declarer: format!("{} <{}>", declarer.name, declarer.email),
        declared_at: declared_at.to_rfc3339(),
        supersedes: None,
    };
    emit_mutation(cli, repo, &output, "set")
}

fn cmd_visibility_promote(
    cli: &Cli,
    repo: &Repository,
    args: VisibilityPromoteArgs,
) -> Result<()> {
    let state = resolve_state(repo, &args.state)?;
    let tier = args
        .tier
        .into_tier(args.label)
        .map_err(|msg| anyhow!(msg))?;

    // A promotion supersedes the current effective declaration, so one must
    // exist. The content id of that record is what the new record points at.
    let existing = repo.get_state_visibility_for_state(&state)?;
    let superseded = match existing.latest() {
        Some(latest) => repo.state_visibility_record_id(latest)?,
        None => {
            return Err(anyhow!(
                "state '{}' has no visibility record to promote (it is public-by-absence)",
                args.state
            ));
        }
    };

    let declarer = repo
        .get_principal()
        .with_context(|| "resolve current principal")?;
    let declared_at = Utc::now();
    let record = StateVisibility {
        state,
        tier: tier.clone(),
        embargo_until: None,
        declarer: declarer.clone(),
        declared_at,
        signature: None,
        supersedes: Some(superseded),
    };
    let prior = repo.get_state_visibility_bytes_for_state(&state)?;
    let record_id = repo.put_state_visibility(record)?;
    let new = repo.get_state_visibility_bytes_for_state(&state)?;
    let scope = repo.op_scope();
    repo.oplog().record_state_visibility_promote(
        &state,
        &superseded,
        &record_id,
        &tier,
        VisibilitySidecarSnapshots { prior, new },
        Some(&scope),
    )?;

    let output = VisibilityMutationOutput {
        output_kind: "visibility_promote",
        state: state.short(),
        tier: tier.as_str().to_string(),
        label: tier_label(&tier).map(str::to_string),
        record_id: record_id.short(),
        declarer: format!("{} <{}>", declarer.name, declarer.email),
        declared_at: declared_at.to_rfc3339(),
        supersedes: Some(superseded.short()),
    };
    emit_mutation(cli, repo, &output, "promoted")
}

fn cmd_visibility_show(cli: &Cli, repo: &Repository, args: VisibilityShowArgs) -> Result<()> {
    let state = resolve_state(repo, &args.state)?;
    let blob = repo.get_state_visibility_for_state(&state)?;
    let effective = blob.latest();
    let tier = effective
        .map(|r| r.tier.clone())
        .unwrap_or(VisibilityTier::Public);
    let effective_public = tier == VisibilityTier::Public;

    #[derive(Serialize)]
    struct ShowOutput {
        output_kind: &'static str,
        state: String,
        tier: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        effective_public: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        declarer: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        declared_at: Option<String>,
        record_count: usize,
    }
    let output = ShowOutput {
        output_kind: "visibility_show",
        state: state.short(),
        tier: tier.as_str().to_string(),
        label: tier_label(&tier).map(str::to_string),
        effective_public,
        declarer: effective.map(|r| format!("{} <{}>", r.declarer.name, r.declarer.email)),
        declared_at: effective.map(|r| r.declared_at.to_rfc3339()),
        record_count: blob.records.len(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("state {}", output.state);
        match &output.label {
            Some(label) => println!("  tier:    {} ({})", output.tier, label),
            None => println!("  tier:    {}", output.tier),
        }
        if output.effective_public {
            println!("  (public-by-absence — no visibility record)");
        } else {
            if let Some(declarer) = &output.declarer {
                println!("  by:      {}", declarer);
            }
            if let Some(at) = &output.declared_at {
                println!("  at:      {}", at);
            }
        }
    }
    Ok(())
}

fn cmd_visibility_list(cli: &Cli, repo: &Repository, _args: VisibilityListArgs) -> Result<()> {
    let listing = repo.list_all_state_visibility()?;

    #[derive(Serialize)]
    struct Row {
        state: String,
        tier: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        declarer: String,
        declared_at: String,
    }
    #[derive(Serialize)]
    struct Listing {
        output_kind: &'static str,
        states: Vec<Row>,
        count: usize,
    }

    let mut rows: Vec<Row> = Vec::new();
    for (state, blob) in &listing {
        // Only states with a non-public effective tier reach disk, but read
        // the effective record defensively rather than the raw first entry.
        let Some(latest) = blob.latest() else {
            continue;
        };
        if latest.tier == VisibilityTier::Public {
            continue;
        }
        rows.push(Row {
            state: state.short(),
            tier: latest.tier.as_str().to_string(),
            label: tier_label(&latest.tier).map(str::to_string),
            declarer: format!("{} <{}>", latest.declarer.name, latest.declarer.email),
            declared_at: latest.declared_at.to_rfc3339(),
        });
    }
    rows.sort_by(|a, b| a.state.cmp(&b.state));

    let count = rows.len();
    let payload = Listing {
        output_kind: "visibility_list",
        states: rows,
        count,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&payload)?);
    } else if count == 0 {
        println!("no non-public states in repo");
    } else {
        println!("{} non-public state(s):", count);
        for row in &payload.states {
            match &row.label {
                Some(label) => println!("  {} {} ({})", row.state, row.tier, label),
                None => println!("  {} {}", row.state, row.tier),
            }
        }
    }
    Ok(())
}

fn emit_mutation(
    cli: &Cli,
    repo: &Repository,
    output: &VisibilityMutationOutput,
    verb: &str,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        match &output.label {
            Some(label) => println!(
                "{} visibility of {} -> {} ({})",
                verb, output.state, output.tier, label
            ),
            None => println!("{} visibility of {} -> {}", verb, output.state, output.tier),
        }
    }
    Ok(())
}
