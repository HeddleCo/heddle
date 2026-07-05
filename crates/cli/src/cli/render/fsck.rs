// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use heddle_core::FsckReport;

use crate::cli::{render::write_stdout, style};

pub fn fsck_json(report: &FsckReport) -> Result<()> {
    let mut text = serde_json::to_string(report)?;
    text.push('\n');
    write_stdout(&text)
}

pub fn fsck_text(report: &FsckReport) -> Result<()> {
    let mut text = String::new();
    if report.valid {
        let counted = style::count(report.objects_checked, "object");
        text.push_str(&format!(
            "{} repository is valid ({counted} checked)\n",
            style::ok_marker(),
        ));
        if report.bridge_checked {
            text.push_str(&format!(
                "  {}\n",
                style::field("Bridge", "mirror and mapping checked")
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
                    style::error(&format!("[{}]", error.kind)),
                    error.message,
                    style::dim(&format!("({obj})"))
                ));
            } else {
                text.push_str(&format!(
                    "  {} {}\n",
                    style::error(&format!("[{}]", error.kind)),
                    error.message
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
            style::field("Repair", &format!("{target}: {status}"))
        ));
        for repair in &report.repairs {
            if repair.count > 0 || repair.repaired {
                text.push_str(&format!(
                    "    {} {} ({})\n",
                    repair.name, repair.detail, repair.count
                ));
            }
        }
    }
    for warning in &report.warnings {
        text.push_str(&format!("{} {}\n", style::warn_marker(), warning));
    }
    write_stdout(&text)
}
