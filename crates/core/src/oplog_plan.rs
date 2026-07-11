// SPDX-License-Identifier: Apache-2.0
//! Pure oplog recover planning and message assembly (no FS / oplog I/O).
//!
//! Owns status classification and human field strings for `heddle oplog recover`
//! that can be decided from recovery report facts alone. Actual salvage,
//! quarantine paths, and store access stay CLI/repo-owned.

/// Pure facts the recover command needs to classify and print a result.
///
/// Paths are pre-rendered display strings so this module stays free of
/// `Path`/`PathBuf` coupling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OplogRecoverFacts {
    pub already_healthy: bool,
    pub prior_recovery: bool,
    pub strategy: Option<String>,
    pub entries_recovered: u64,
    pub entries_lost: Option<u64>,
    pub damaged_byte_start: u64,
    pub damaged_byte_end: u64,
    pub quarantine_path: Option<String>,
    pub sidecar_path: Option<String>,
}

/// Outcome class for human recover messaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OplogRecoverStatus {
    /// Parsed cleanly this run; no known prior salvage.
    HealthyNothingToRecover,
    /// Healthy now; numbers come from an earlier recovery sidecar.
    HealthyPriorRecovery,
    /// Salvage performed by this invocation.
    Salvaged,
}

/// Classify recover status from the two boolean report flags.
pub fn plan_oplog_recover_status(
    already_healthy: bool,
    prior_recovery: bool,
) -> OplogRecoverStatus {
    if already_healthy && !prior_recovery {
        OplogRecoverStatus::HealthyNothingToRecover
    } else if prior_recovery {
        OplogRecoverStatus::HealthyPriorRecovery
    } else {
        OplogRecoverStatus::Salvaged
    }
}

/// Classify from a facts bundle.
pub fn plan_oplog_recover(facts: &OplogRecoverFacts) -> OplogRecoverStatus {
    plan_oplog_recover_status(facts.already_healthy, facts.prior_recovery)
}

/// Default strategy label when the report omits one (historical CLI fallback).
pub const OPLOG_RECOVER_DEFAULT_STRATEGY: &str = "forward-greedy";

/// Headline body (without style markers) for the recover result line.
pub fn oplog_recover_headline(status: OplogRecoverStatus, strategy: Option<&str>) -> String {
    match status {
        OplogRecoverStatus::HealthyNothingToRecover => {
            "operation log is healthy; nothing to recover".to_string()
        }
        OplogRecoverStatus::HealthyPriorRecovery => {
            "operation log is healthy; a prior recovery had already salvaged it".to_string()
        }
        OplogRecoverStatus::Salvaged => {
            let strategy = strategy.unwrap_or(OPLOG_RECOVER_DEFAULT_STRATEGY);
            format!("salvaged operation log ({strategy} strategy)")
        }
    }
}

/// Headline from facts.
pub fn oplog_recover_headline_from_facts(facts: &OplogRecoverFacts) -> String {
    oplog_recover_headline(plan_oplog_recover(facts), facts.strategy.as_deref())
}

/// Whether the command should print detail fields after the headline.
///
/// Historical CLI returns after the headline when healthy with no prior recovery.
pub fn oplog_recover_shows_detail(status: OplogRecoverStatus) -> bool {
    !matches!(status, OplogRecoverStatus::HealthyNothingToRecover)
}

/// Whether to print the Strategy field line (only on prior-recovery path).
pub fn oplog_recover_shows_strategy_field(status: OplogRecoverStatus) -> bool {
    matches!(status, OplogRecoverStatus::HealthyPriorRecovery)
}

/// Damaged byte span length.
pub fn oplog_recover_damaged_bytes(start: u64, end: u64) -> u64 {
    end.saturating_sub(start)
}

/// Display for records lost (`"unknown"` when unreadable).
pub fn oplog_recover_entries_lost_display(entries_lost: Option<u64>) -> String {
    entries_lost
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Display for the damaged byte range field.
pub fn oplog_recover_damaged_range_display(start: u64, end: u64) -> String {
    let damaged_bytes = oplog_recover_damaged_bytes(start, end);
    format!("{start}..{end} ({damaged_bytes} bytes)")
}

/// Ordered human body lines (field labels only — CLI wraps with style helpers).
///
/// Returns `(label, value)` pairs the CLI can feed to `style::field`.
/// Empty when [`oplog_recover_shows_detail`] is false.
pub fn oplog_recover_detail_fields(facts: &OplogRecoverFacts) -> Vec<(&'static str, String)> {
    let status = plan_oplog_recover(facts);
    if !oplog_recover_shows_detail(status) {
        return Vec::new();
    }

    let mut fields = Vec::new();
    if oplog_recover_shows_strategy_field(status)
        && let Some(strategy) = facts.strategy.as_ref()
    {
        fields.push(("Strategy", strategy.clone()));
    }
    fields.push(("Records recovered", facts.entries_recovered.to_string()));
    fields.push((
        "Records lost",
        oplog_recover_entries_lost_display(facts.entries_lost),
    ));
    fields.push((
        "Damaged byte range",
        oplog_recover_damaged_range_display(facts.damaged_byte_start, facts.damaged_byte_end),
    ));
    if let Some(quarantine) = &facts.quarantine_path {
        fields.push(("Quarantined to", quarantine.clone()));
    }
    if let Some(sidecar) = &facts.sidecar_path {
        fields.push(("Recovery record", sidecar.clone()));
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_facts() -> OplogRecoverFacts {
        OplogRecoverFacts {
            already_healthy: false,
            prior_recovery: false,
            strategy: Some("footer-guided".into()),
            entries_recovered: 10,
            entries_lost: Some(2),
            damaged_byte_start: 100,
            damaged_byte_end: 150,
            quarantine_path: Some("/tmp/q".into()),
            sidecar_path: Some("/tmp/s".into()),
        }
    }

    #[test]
    fn status_classification() {
        assert_eq!(
            plan_oplog_recover_status(true, false),
            OplogRecoverStatus::HealthyNothingToRecover
        );
        assert_eq!(
            plan_oplog_recover_status(true, true),
            OplogRecoverStatus::HealthyPriorRecovery
        );
        assert_eq!(
            plan_oplog_recover_status(false, false),
            OplogRecoverStatus::Salvaged
        );
        // prior_recovery without already_healthy still classifies as prior path
        // only when already_healthy is true; otherwise salvaged branch is unused
        // in practice but pure rule prioritizes prior when already_healthy.
        assert!(!oplog_recover_shows_detail(
            OplogRecoverStatus::HealthyNothingToRecover
        ));
        assert!(oplog_recover_shows_detail(
            OplogRecoverStatus::HealthyPriorRecovery
        ));
        assert!(oplog_recover_shows_detail(OplogRecoverStatus::Salvaged));
        assert!(oplog_recover_shows_strategy_field(
            OplogRecoverStatus::HealthyPriorRecovery
        ));
        assert!(!oplog_recover_shows_strategy_field(
            OplogRecoverStatus::Salvaged
        ));
    }

    #[test]
    fn headlines_and_field_displays() {
        assert_eq!(
            oplog_recover_headline(OplogRecoverStatus::HealthyNothingToRecover, None),
            "operation log is healthy; nothing to recover"
        );
        assert!(
            oplog_recover_headline(OplogRecoverStatus::Salvaged, None)
                .contains(OPLOG_RECOVER_DEFAULT_STRATEGY)
        );
        assert!(
            oplog_recover_headline(OplogRecoverStatus::Salvaged, Some("footer-guided"))
                .contains("footer-guided")
        );
        assert_eq!(oplog_recover_damaged_bytes(100, 150), 50);
        assert_eq!(oplog_recover_entries_lost_display(None), "unknown");
        assert_eq!(oplog_recover_entries_lost_display(Some(3)), "3");
        assert_eq!(oplog_recover_damaged_range_display(1, 5), "1..5 (4 bytes)");
    }

    #[test]
    fn detail_fields_by_status() {
        let healthy = OplogRecoverFacts {
            already_healthy: true,
            prior_recovery: false,
            ..base_facts()
        };
        assert!(oplog_recover_detail_fields(&healthy).is_empty());
        assert_eq!(
            oplog_recover_headline_from_facts(&healthy),
            "operation log is healthy; nothing to recover"
        );

        let prior = OplogRecoverFacts {
            already_healthy: true,
            prior_recovery: true,
            quarantine_path: None,
            ..base_facts()
        };
        let fields = oplog_recover_detail_fields(&prior);
        assert_eq!(fields[0].0, "Strategy");
        assert_eq!(fields[0].1, "footer-guided");
        assert!(fields.iter().any(|(k, _)| *k == "Records recovered"));
        assert!(fields.iter().any(|(k, _)| *k == "Recovery record"));
        assert!(!fields.iter().any(|(k, _)| *k == "Quarantined to"));

        let salvaged = base_facts();
        let fields = oplog_recover_detail_fields(&salvaged);
        // Strategy line only on prior-recovery path
        assert!(!fields.iter().any(|(k, _)| *k == "Strategy"));
        assert!(
            fields
                .iter()
                .any(|(k, v)| *k == "Quarantined to" && v == "/tmp/q")
        );
    }
}
