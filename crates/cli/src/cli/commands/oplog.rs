// SPDX-License-Identifier: Apache-2.0
//! Oplog command — operator-facing inspection and recovery.

use anyhow::Result;
use oplog::OplogRecoveryReport;
use serde::Serialize;

use crate::cli::{Cli, OplogCommands, should_output_json, style};

pub fn cmd_oplog(cli: &Cli, command: OplogCommands) -> Result<()> {
    match command {
        OplogCommands::Recover => cmd_oplog_recover(cli),
    }
}

#[derive(Serialize)]
struct RecoverOutput {
    /// Wire-format discriminator for this report shape.
    output_kind: &'static str,
    /// True when the oplog parsed cleanly and no salvage ran this invocation.
    already_healthy: bool,
    /// True when the reported numbers come from a sidecar left by an EARLIER
    /// recovery (the silent auto-fallback ran before this command).
    prior_recovery: bool,
    /// Strategy that located the recovered prefix: `footer-guided` or
    /// `forward-greedy` (absent when no recovery is known).
    #[serde(skip_serializing_if = "Option::is_none")]
    strategy: Option<String>,
    /// Complete oplog records kept.
    entries_recovered: u64,
    /// Records the damaged file claimed but that could not be salvaged
    /// (absent when the original count was itself unreadable, or healthy).
    #[serde(skip_serializing_if = "Option::is_none")]
    entries_lost: Option<u64>,
    /// First byte of the damaged tail (the truncation/tear offset).
    damaged_byte_start: u64,
    /// One-past-the-last damaged byte (the original file length).
    damaged_byte_end: u64,
    /// Where the damaged original was quarantined (absent when healthy).
    #[serde(skip_serializing_if = "Option::is_none")]
    quarantine_path: Option<String>,
    /// Where the `.oplog.recovery` sidecar lives (absent when healthy with no
    /// prior recovery).
    #[serde(skip_serializing_if = "Option::is_none")]
    sidecar_path: Option<String>,
}

impl From<&OplogRecoveryReport> for RecoverOutput {
    fn from(report: &OplogRecoveryReport) -> Self {
        Self {
            output_kind: "oplog_recover",
            already_healthy: report.already_healthy,
            prior_recovery: report.prior_recovery,
            strategy: report.strategy.clone(),
            entries_recovered: report.entries_recovered,
            entries_lost: report.entries_lost,
            damaged_byte_start: report.damaged_byte_start,
            damaged_byte_end: report.damaged_byte_end,
            quarantine_path: report
                .quarantine_path
                .as_ref()
                .map(|p| p.display().to_string()),
            sidecar_path: report
                .sidecar_path
                .as_ref()
                .map(|p| p.display().to_string()),
        }
    }
}

fn cmd_oplog_recover(cli: &Cli) -> Result<()> {
    let repo = cli.open_repo()?;
    let report = repo.oplog().recover()?;

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&RecoverOutput::from(&report))?);
        return Ok(());
    }

    if report.already_healthy && !report.prior_recovery {
        println!(
            "{} operation log is healthy; nothing to recover",
            style::ok_marker()
        );
        return Ok(());
    }

    if report.prior_recovery {
        println!(
            "{} operation log is healthy; a prior recovery had already salvaged it",
            style::ok_marker()
        );
    } else {
        println!(
            "{} salvaged operation log ({} strategy)",
            style::ok_marker(),
            report.strategy.as_deref().unwrap_or("forward-greedy")
        );
    }

    let damaged_bytes = report
        .damaged_byte_end
        .saturating_sub(report.damaged_byte_start);
    if report.prior_recovery
        && let Some(strategy) = &report.strategy
    {
        println!("  {}", style::field("Strategy", strategy));
    }
    println!(
        "  {}",
        style::field("Records recovered", &report.entries_recovered.to_string())
    );
    println!(
        "  {}",
        style::field(
            "Records lost",
            &report
                .entries_lost
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "Damaged byte range",
            &format!(
                "{}..{} ({} bytes)",
                report.damaged_byte_start, report.damaged_byte_end, damaged_bytes
            )
        )
    );
    if let Some(quarantine) = &report.quarantine_path {
        println!(
            "  {}",
            style::field("Quarantined to", &quarantine.display().to_string())
        );
    }
    if let Some(sidecar) = &report.sidecar_path {
        println!(
            "  {}",
            style::field("Recovery record", &sidecar.display().to_string())
        );
    }
    Ok(())
}
