// SPDX-License-Identifier: Apache-2.0
//! Pure Git projection import/export/sync summary strings (no Git I/O).
//!
//! Owns plain-text commit and ref listing summaries for `export git` /
//! `sync git` human output. Styling, destination I/O, and RecoveryAdvice
//! stay CLI-owned.

/// Plain commits summary for export/sync: total plus newly-written vs already-in-sync.
///
/// Examples:
/// - `0 total`
/// - `5 total (already in sync)` when `newly == 0` and `total > 0`
/// - `3 total (3 newly written)` when everything is new
/// - `5 total (2 newly written, 3 already in sync)` when mixed
pub fn export_commits_summary(total: usize, newly: usize) -> String {
    let already = total.saturating_sub(newly);
    let breakdown = if total == 0 {
        String::new()
    } else if newly == 0 {
        " (already in sync)".to_string()
    } else if already == 0 {
        format!(" ({newly} newly written)")
    } else {
        format!(" ({newly} newly written, {already} already in sync)")
    };
    format!("{total} total{breakdown}")
}

/// One exported ref fact for plain listing (name + tip hex, any length).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportedRefSummaryFact<'a> {
    pub name: &'a str,
    /// Full tip hex (or any string); truncated to 7 chars for display.
    pub tip_hex: &'a str,
}

/// Plain branches/tags summary: count, then `name shorttip · …`.
///
/// Empty refs yields just the count string (e.g. `"0"`).
pub fn exported_refs_summary(refs: &[ExportedRefSummaryFact<'_>]) -> String {
    let count = refs.len();
    if refs.is_empty() {
        return count.to_string();
    }
    let listing = refs
        .iter()
        .map(|r| {
            let short_tip: String = r.tip_hex.chars().take(7).collect();
            format!("{} {short_tip}", r.name)
        })
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{count}   {listing}")
}

/// Short tip hex for display (first 7 characters).
pub fn short_git_tip(tip_hex: &str) -> String {
    tip_hex.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_commits_summary_variants() {
        assert_eq!(export_commits_summary(0, 0), "0 total");
        assert_eq!(export_commits_summary(5, 0), "5 total (already in sync)");
        assert_eq!(export_commits_summary(3, 3), "3 total (3 newly written)");
        assert_eq!(
            export_commits_summary(5, 2),
            "5 total (2 newly written, 3 already in sync)"
        );
    }

    #[test]
    fn exported_refs_summary_lists_and_empty() {
        assert_eq!(exported_refs_summary(&[]), "0");
        let refs = [
            ExportedRefSummaryFact {
                name: "main",
                tip_hex: "af25b9d1234",
            },
            ExportedRefSummaryFact {
                name: "spike-ok",
                tip_hex: "7f1002c",
            },
        ];
        assert_eq!(
            exported_refs_summary(&refs),
            "2   main af25b9d · spike-ok 7f1002c"
        );
        assert_eq!(short_git_tip("abcdef0123"), "abcdef0");
    }
}
