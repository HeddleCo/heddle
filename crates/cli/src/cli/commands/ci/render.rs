// SPDX-License-Identifier: Apache-2.0
//! Machine and operator rendering for signed local verdicts.

use std::fmt::Write as _;

use anyhow::Result;
use crypto::{CheckClass, Conclusion, SignedVerdict};

use super::super::next_action::{NextActionValidationContext, write_full_command_json};
use crate::cli::{Cli, render::write_stdout, should_output_json};

pub(crate) fn render(cli: &Cli, verdicts: &[SignedVerdict]) -> Result<()> {
    if should_output_json(cli, None) {
        write_full_command_json(
            &verdicts,
            NextActionValidationContext::without_repo(&["ci", "run"]),
        )
    } else {
        if let Some(digest) = definition_digest(verdicts) {
            eprintln!("heddle ci: definition_digest {digest}");
        }
        write_stdout(&render_table(verdicts))
    }
}

pub(crate) fn non_passing_advisory(verdicts: &[SignedVerdict]) -> Vec<&str> {
    verdicts
        .iter()
        .filter(|verdict| {
            verdict.body.check.class != CheckClass::Required
                && is_failing(verdict.body.outcome.conclusion)
        })
        .map(|verdict| verdict.body.check.name.as_str())
        .collect()
}

pub(crate) fn has_required_failure(verdicts: &[SignedVerdict]) -> bool {
    verdicts.iter().any(|verdict| {
        verdict.body.check.class == CheckClass::Required
            && is_failing(verdict.body.outcome.conclusion)
    })
}

fn is_failing(conclusion: Conclusion) -> bool {
    matches!(
        conclusion,
        Conclusion::Failure | Conclusion::TimedOut | Conclusion::InfraError
    )
}

fn definition_digest(verdicts: &[SignedVerdict]) -> Option<&str> {
    verdicts
        .first()
        .map(|verdict| verdict.body.check.definition_digest.as_str())
        .filter(|digest| !digest.is_empty())
}

fn render_table(verdicts: &[SignedVerdict]) -> String {
    let rows: Vec<_> = verdicts.iter().map(Row::new).collect();
    let headers = ["CHECK", "CLASS", "CONCLUSION", "DURATION", "FAILING STEP"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (index, value) in row.values().iter().enumerate() {
            widths[index] = widths[index].max(value.len());
        }
    }
    let mut output = String::new();
    write_row(&mut output, &widths, headers);
    for row in &rows {
        let values = row.values();
        write_row(&mut output, &widths, values.map(|value| value.as_str()));
    }
    output
}

fn write_row(output: &mut String, widths: &[usize; 5], values: [&str; 5]) {
    for (index, value) in values.iter().enumerate() {
        if index + 1 == values.len() {
            let _ = write!(output, "{value}");
        } else {
            let _ = write!(output, "{value:<width$}  ", width = widths[index]);
        }
    }
    output.push('\n');
}

struct Row {
    check: String,
    class: String,
    conclusion: String,
    duration: String,
    failing_step: String,
}

impl Row {
    fn new(verdict: &SignedVerdict) -> Self {
        let body = &verdict.body;
        Self {
            check: body.check.name.clone(),
            class: class_name(body.check.class).to_string(),
            conclusion: conclusion_name(body.outcome.conclusion).to_string(),
            duration: format_duration(body.execution.duration_ms),
            failing_step: body
                .outcome
                .failure
                .as_ref()
                .and_then(|failure| failure.failing_step.clone())
                .unwrap_or_default(),
        }
    }

    fn values(&self) -> [&String; 5] {
        [
            &self.check,
            &self.class,
            &self.conclusion,
            &self.duration,
            &self.failing_step,
        ]
    }
}

fn class_name(class: CheckClass) -> &'static str {
    match class {
        CheckClass::Required => "required",
        CheckClass::Advisory => "advisory",
        CheckClass::Informational => "informational",
    }
}

fn conclusion_name(conclusion: Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Success => "success",
        Conclusion::Failure => "failure",
        Conclusion::Cancelled => "cancelled",
        Conclusion::Skipped => "skipped",
        Conclusion::TimedOut => "timed_out",
        Conclusion::InfraError => "infra_error",
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        return format!("{}.{:01}s", seconds, (milliseconds % 1_000) / 100);
    }
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_match_treadle_surface() {
        assert_eq!(format_duration(850), "850ms");
        assert_eq!(format_duration(3_400), "3.4s");
        assert_eq!(format_duration(125_000), "2m05s");
    }

    #[test]
    fn empty_verdicts_have_no_digest_line() {
        assert_eq!(definition_digest(&[]), None);
    }
}
