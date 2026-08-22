// SPDX-License-Identifier: Apache-2.0
//! Oplog command — operator-facing inspection and recovery.

use anyhow::{Context, Result};
use oplog::OplogRecoveryReport;
use repo::Repository;
use serde::Serialize;
use verbs::oplog_plan::{
    OplogRecoverFacts, oplog_recover_detail_fields, oplog_recover_headline_from_facts,
    oplog_recover_shows_detail, plan_oplog_recover,
};

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
    /// True when the reported numbers come from a sidecar left by an earlier
    /// automatic validation repair or operator recovery.
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
    /// Later immutable containers explicitly dropped because recovery of an
    /// earlier segment broke the contiguous EntryId chain.
    suffix_segments_discarded: u64,
    /// Complete records contained by those dropped suffix containers.
    suffix_entries_discarded: u64,
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
            suffix_segments_discarded: report.suffix_segments_discarded,
            suffix_entries_discarded: report.suffix_entries_discarded,
        }
    }
}

fn recover_facts(report: &OplogRecoveryReport) -> OplogRecoverFacts {
    OplogRecoverFacts {
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

fn cmd_oplog_recover(cli: &Cli) -> Result<()> {
    let cwd;
    let repo_path = match cli.repo.as_ref() {
        Some(path) => path,
        None => {
            cwd = std::env::current_dir().context("get current working directory")?;
            &cwd
        }
    };
    let repo = Repository::open_for_oplog_recovery(repo_path)
        .context("open Heddle repository for oplog recovery")?;
    let report = repo.oplog().recover()?;

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&RecoverOutput::from(&report))?);
        return Ok(());
    }

    let facts = recover_facts(&report);
    let status = plan_oplog_recover(&facts);
    println!(
        "{} {}",
        style::ok_marker(),
        oplog_recover_headline_from_facts(&facts)
    );

    if !oplog_recover_shows_detail(status) {
        return Ok(());
    }

    for (label, value) in oplog_recover_detail_fields(&facts) {
        println!("  {}", style::field(label, &value));
    }
    if report.suffix_segments_discarded > 0 {
        println!(
            "  {}",
            style::field(
                "Suffix segments discarded",
                &report.suffix_segments_discarded.to_string(),
            )
        );
        println!(
            "  {}",
            style::field(
                "Suffix entries discarded",
                &report.suffix_entries_discarded.to_string(),
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with_suffix(segments: u64, entries: u64) -> OplogRecoveryReport {
        OplogRecoveryReport {
            already_healthy: segments == 0,
            prior_recovery: false,
            strategy: None,
            entries_recovered: 0,
            entries_lost: None,
            damaged_byte_start: 0,
            damaged_byte_end: 0,
            quarantine_path: None,
            sidecar_path: None,
            suffix_segments_discarded: segments,
            suffix_entries_discarded: entries,
        }
    }

    #[test]
    fn recover_json_always_reports_zero_or_nonzero_suffix_loss() {
        let zero = serde_json::to_value(RecoverOutput::from(&report_with_suffix(0, 0))).unwrap();
        assert_eq!(zero["suffix_segments_discarded"], 0);
        assert_eq!(zero["suffix_entries_discarded"], 0);

        let loss = serde_json::to_value(RecoverOutput::from(&report_with_suffix(2, 7))).unwrap();
        assert_eq!(loss["suffix_segments_discarded"], 2);
        assert_eq!(loss["suffix_entries_discarded"], 7);
    }
}
