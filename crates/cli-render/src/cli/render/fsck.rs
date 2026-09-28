// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use verbs::FsckReport;

use crate::cli::{render::write_stdout, style};

pub fn fsck_json(report: &FsckReport) -> Result<()> {
    let mut text = serde_json::to_string(report)?;
    text.push('\n');
    write_stdout(&text)
}

pub fn fsck_text(report: &FsckReport) -> Result<()> {
    write_stdout(&format_fsck_text(report))
}

fn format_fsck_text(report: &FsckReport) -> String {
    let mut text = String::new();
    if report.valid {
        let counted = style::count(report.objects_checked, "object");
        text.push_str(&format!(
            "{} repository is valid ({counted} checked)\n",
            style::ok_marker(),
        ));
        if report.git_projection_checked {
            text.push_str(&format!(
                "  {}\n",
                style::field("Git projection", "mapping, notes, and checkout checked")
            ));
        }
    } else {
        text.push_str(&format!(
            "{} repository has {}\n",
            style::error_marker(),
            style::count(report.errors.len(), "integrity error")
        ));
        for error in &report.errors {
            if let Some(obj) = &error.object {
                text.push_str(&format!(
                    "  {} {} {}\n",
                    style::error(&format!("[{}]", style::human_text(&error.kind))),
                    style::human_text(&error.message),
                    style::dim(&format!("({})", style::human_text(obj)))
                ));
            } else {
                text.push_str(&format!(
                    "  {} {}\n",
                    style::error(&format!("[{}]", style::human_text(&error.kind))),
                    style::human_text(&error.message)
                ));
            }
        }
    }
    if let Some(target) = &report.repair_target {
        let status = if report.repaired {
            "repaired"
        } else {
            "no changes"
        };
        text.push_str(&format!(
            "  {}\n",
            style::field(
                "Repair",
                &format!("{}: {status}", style::human_text(target))
            )
        ));
        for repair in &report.repairs {
            if repair.count > 0 || repair.repaired {
                text.push_str(&format!(
                    "    {} {} ({})\n",
                    style::human_text(&repair.name),
                    style::human_text(&repair.detail),
                    repair.count
                ));
            }
        }
    }
    if let Some(provenance) = &report.provenance {
        text.push_str(&format!(
            "  {}\n",
            style::field(
                "Provenance registry",
                &style::human_text(&provenance.registry_status)
            )
        ));
        for state in &provenance.states {
            let short = state
                .state_id
                .strip_prefix("hs-")
                .unwrap_or(&state.state_id);
            let short = &short[..short.len().min(12)];
            text.push_str(&format!(
                "    {short} {:<24} {}\n",
                state.display_status(),
                style::human_text(&state.detail)
            ));
        }
    }
    for warning in &report.warnings {
        text.push_str(&format!(
            "{} {}\n",
            style::warn_marker(),
            style::human_text(warning)
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use verbs::FsckError;

    use super::*;

    #[test]
    fn text_renderer_consumes_the_typed_fsck_report() {
        let report = FsckReport {
            valid: true,
            errors: Vec::new(),
            warnings: vec!["legacy object retained".to_string()],
            objects_checked: 2,
            git_projection_checked: false,
            provenance: None,
            repair_target: None,
            repaired: false,
            repairs: Vec::new(),
        };

        let text = format_fsck_text(&report);
        assert!(text.contains("repository is valid (2 objects checked)"));
        assert!(text.contains("legacy object retained"));
    }

    #[test]
    fn text_renderer_shortens_corrupt_object_id() {
        let id = "a".repeat(64);
        let report = FsckReport {
            valid: false,
            errors: vec![FsckError {
                kind: "missing_tree".to_string(),
                message: format!("missing tree object {id}"),
                object: Some(id.clone()),
            }],
            warnings: Vec::new(),
            objects_checked: 1,
            git_projection_checked: false,
            provenance: None,
            repair_target: None,
            repaired: false,
            repairs: Vec::new(),
        };
        let text = format_fsck_text(&report);
        assert!(text.contains("hs-aaaaaaaa"), "{text}");
        assert!(!text.contains(&id), "{text}");
        assert!(serde_json::to_string(&report).unwrap().contains(&id));
    }
}
