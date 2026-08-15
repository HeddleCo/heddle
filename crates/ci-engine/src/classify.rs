// SPDX-License-Identifier: Apache-2.0
//! Failure classification and bounded excerpt extraction.

use std::sync::OnceLock;

use crypto::FailureClass;
use regex::Regex;

/// Maximum bytes stored in a verdict failure excerpt.
pub const EXCERPT_CAP_BYTES: usize = 4096;
const CONTEXT_LINES: usize = 6;

/// Terminal process disposition before output classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Disposition {
    /// Process exited successfully.
    Success,
    /// Process exited nonzero.
    Exited(i32),
    /// Process exceeded its deadline.
    TimedOut,
    /// Process could not run or be observed.
    #[default]
    InfraError,
}

fn typed_build_error() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"error\[E\d+\]").expect("valid static regex"))
}

fn could_not_compile() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"error: could not compile").expect("valid static regex"))
}

fn test_failure() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"test result: FAILED|panicked at|assertion .*failed|\bFAILED\b")
            .expect("valid static regex")
    })
}

fn lint_failure() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"-D warnings|clippy::[a-z_]+|requested on the command line")
            .expect("valid static regex")
    })
}

/// Classify a process result. Disposition always wins over captured text.
#[must_use]
pub fn classify(disposition: Disposition, output: &str) -> Option<FailureClass> {
    match disposition {
        Disposition::Success => None,
        Disposition::TimedOut => Some(FailureClass::Timeout),
        Disposition::InfraError => Some(FailureClass::Infra),
        Disposition::Exited(_) => Some(classify_output(output)),
    }
}

fn classify_output(output: &str) -> FailureClass {
    if typed_build_error().is_match(output) {
        FailureClass::Build
    } else if lint_failure().is_match(output) {
        FailureClass::Lint
    } else if could_not_compile().is_match(output) {
        FailureClass::Build
    } else {
        FailureClass::Test
    }
}

/// Extract the last causal-looking region and cap it without splitting UTF-8.
#[must_use]
pub fn extract_excerpt(output: &str, class: FailureClass) -> String {
    let lines: Vec<_> = output.lines().collect();
    let anchor = last_anchor(&lines, class);
    let excerpt = match anchor {
        Some(index) => {
            let start = index.saturating_sub(2);
            let end = (index + CONTEXT_LINES + 1).min(lines.len());
            lines[start..end].join("\n")
        }
        None => lines[lines.len().saturating_sub(CONTEXT_LINES + 1)..].join("\n"),
    };
    cap_bytes(&excerpt, EXCERPT_CAP_BYTES)
}

fn last_anchor(lines: &[&str], class: FailureClass) -> Option<usize> {
    let matches_class = |line: &&str| match class {
        FailureClass::Build => {
            typed_build_error().is_match(line) || could_not_compile().is_match(line)
        }
        FailureClass::Lint => lint_failure().is_match(line),
        _ => test_failure().is_match(line),
    };
    lines
        .iter()
        .rposition(matches_class)
        .or_else(|| lines.iter().rposition(|line| line.contains("error")))
}

pub(crate) fn cap_bytes(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    const MARKER: &str = "\n… [excerpt truncated]";
    let mut end = cap.saturating_sub(MARKER.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &value[..end])
}
