// SPDX-License-Identifier: Apache-2.0
//! `heddle redact purge` — physically remove the bytes referenced by an
//! existing redaction. Irreversible by design.
//!
//! Workspace-owner capability is a documented constraint in the build
//! brief; the Biscuit verifier rule is a future-work item. For now,
//! `--force` is the explicit confirmation step.

use anyhow::{Context, Result, anyhow};
use objects::object::ChangeId;
use oplog::OpLogRecorder;
use repo::Repository;
use serde::Serialize;

use super::advice::RecoveryAdvice;
use crate::{
    cli::{Cli, PurgeApplyArgs, PurgeCommands, PurgeListArgs, should_output_json},
    config::UserConfig,
};

pub fn cmd_purge(cli: &Cli, command: PurgeCommands) -> Result<()> {
    let _user = UserConfig::load_default().unwrap_or_default();
    let repo = cli.open_repo()?;
    match command {
        PurgeCommands::Apply(args) => cmd_purge_apply(cli, &repo, args),
        PurgeCommands::List(args) => cmd_purge_list(cli, &repo, args),
    }
}

#[derive(Serialize)]
struct PurgeApplyOutput {
    output_kind: &'static str,
    redaction_id: Option<String>,
    blob: String,
    state: String,
    path: String,
    redactions_marked: usize,
    blob_bytes_removed: bool,
    /// Future-work flag: the loose-bytes purge today doesn't repack
    /// pack files. `true` means the bytes survive in a packfile and an
    /// operator must rerun a pack rewrite to fully eliminate them.
    blob_remains_in_pack: bool,
    purger: String,
    message: String,
    /// Hint to add the purged path to `.heddleignore` / `.gitignore`
    /// so subsequent captures don't re-import the leaked bytes from
    /// the working tree. `None` when the path is already covered by a
    /// glob rule in either file.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_hint: Option<super::redact::IgnoreHint>,
}

fn cmd_purge_apply(cli: &Cli, repo: &Repository, args: PurgeApplyArgs) -> Result<()> {
    let state = resolve_state(repo, &args.state)?;
    let blob = blob_at_path(repo, &state, &args.path)?;
    let principal = repo
        .get_principal()
        .with_context(|| "resolve current principal")?;

    if !args.force {
        let force_command = format!(
            "heddle redact purge apply {} --path {} --force",
            state.short(),
            args.path
        );
        return Err(anyhow!(RecoveryAdvice::destructive_requires_force(
            "purge",
            format!(
                "purge is irreversible for blob {} ({}) in state {}",
                blob.short(),
                args.path,
                state.short()
            ),
            "purge removes local blob bytes referenced by an existing redaction",
            "heddle redact list",
            force_command,
            "nothing was removed; the redaction record and blob bytes were left untouched",
        )));
    }

    let outcome = repo.purge_blob(&blob, &principal)?;

    if let Some(redaction_id) = &outcome.redaction_id {
        let scope = repo.op_scope();
        repo.oplog()
            .record_purge(redaction_id, &blob, Some(&scope))?;
    }

    let mut message = format!(
        "purged blob {} at {} in {} ({} redaction(s) marked)",
        blob.short(),
        args.path,
        state.short(),
        outcome.redactions_marked,
    );
    if !outcome.blob_bytes_removed {
        message.push_str("\n  note: no loose copy was on disk (already gone or only in a pack)");
    }
    if outcome.blob_remains_in_pack {
        message.push_str(
            "\n  warning: bytes remain in a pack file — repack required for full removal",
        );
    }

    let ignore_hint = super::redact::ignore_hint_for_path(repo, &args.path)?;

    let output = PurgeApplyOutput {
        output_kind: "purge_apply",
        redaction_id: outcome.redaction_id.map(|h| h.short()),
        blob: blob.short(),
        state: state.short(),
        path: args.path,
        redactions_marked: outcome.redactions_marked,
        blob_bytes_removed: outcome.blob_bytes_removed,
        blob_remains_in_pack: outcome.blob_remains_in_pack,
        purger: format!("{} <{}>", principal.name, principal.email),
        message,
        ignore_hint,
    };

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        if let Some(hint) = &output.ignore_hint {
            println!("  {}", hint.message);
        }
    }
    Ok(())
}

fn cmd_purge_list(cli: &Cli, repo: &Repository, _args: PurgeListArgs) -> Result<()> {
    // List every redaction in the repo that's been purged. The
    // companion view in the oplog (`heddle log --output json` filtered to
    // `OpRecord::Purge`) covers the audit trail; this surface is the
    // "which blobs are purged right now" view.
    let listing = repo.list_all_redactions()?;
    #[derive(Serialize)]
    struct Row {
        redaction_id: String,
        blob: String,
        state: String,
        path: String,
        purged_at: String,
        purger: String,
    }
    #[derive(Serialize)]
    struct Listing {
        output_kind: &'static str,
        purges: Vec<Row>,
        count: usize,
    }

    let mut rows: Vec<Row> = Vec::new();
    for (blob, redactions_blob) in &listing {
        for redaction in &redactions_blob.redactions {
            if let Some(purged_at) = redaction.purged_at {
                let id = super::redact::canonical_id_for(redaction)?;
                rows.push(Row {
                    redaction_id: id.short(),
                    blob: blob.short(),
                    state: redaction.state.short(),
                    path: redaction.path.clone(),
                    purged_at: purged_at.to_rfc3339(),
                    purger: format!("{} <{}>", redaction.redactor.name, redaction.redactor.email),
                });
            }
        }
    }
    let count = rows.len();
    let payload = Listing {
        output_kind: "purge_list",
        purges: rows,
        count,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&payload)?);
    } else if count == 0 {
        println!("no purges in repo");
    } else {
        println!("{} purge(s):", count);
        for row in &payload.purges {
            println!(
                "  {} blob={} state={} path={} at {}",
                row.redaction_id, row.blob, row.state, row.path, row.purged_at
            );
        }
    }
    Ok(())
}

fn resolve_state(repo: &Repository, spec: &str) -> Result<ChangeId> {
    repo.resolve_state(spec)
        .with_context(|| format!("resolve state '{}'", spec))?
        .ok_or_else(|| anyhow!("state '{}' not found", spec))
}

fn blob_at_path(
    repo: &Repository,
    state: &ChangeId,
    path: &str,
) -> Result<objects::object::ContentHash> {
    super::redact::blob_at_path(repo, state, path)
}
