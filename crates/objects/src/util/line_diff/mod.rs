// SPDX-License-Identifier: Apache-2.0
//! Scratch-budgeted equal-run LCS used by native blame and Git-overlay blame.

mod compact;
mod emit;
mod myers;
mod myers_search;
mod scan;
mod scratch;
mod visit;

#[cfg(test)]
mod tests;

use super::budget::{BudgetExceeded, ResourceUsage};

pub use scratch::scratch_bytes_for_line_counts;
pub use visit::visit_lcs_equal_runs;

/// Split UTF-8 content into the same logical lines used by blame.
pub fn split_text_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let content = std::str::from_utf8(bytes).ok()?;
    Some(content.lines().map(str::to_string).collect())
}

/// Admission caps for one equal-run LCS visit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineDiffLimits {
    pub scratch_bytes: u64,
    pub max_lines: u64,
    pub max_work: u64,
}

impl LineDiffLimits {
    pub fn unlimited() -> Self {
        Self {
            scratch_bytes: u64::MAX,
            max_lines: u64::MAX,
            max_work: u64::MAX,
        }
    }

    pub fn budget(self, scratch_len: usize) -> crate::util::ResourceBudget {
        crate::util::ResourceBudget::new(crate::util::ResourceUsage {
            scratch_bytes: self.scratch_bytes.min(scratch_len as u64),
            lines: self.max_lines,
            work: self.max_work,
            states: u64::MAX,
            decoded_bytes: u64::MAX,
        })
    }
}

/// One inclusive-length run of equal lines in Myers order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EqualRun {
    pub old_start: usize,
    pub new_start: usize,
    pub len: usize,
}

/// Typed LCS failure. [`BudgetExceeded`] is distinct from UTF-8 rejection
/// and from a visitor that cancelled.
#[derive(Debug)]
pub enum LineDiffError<E = std::convert::Infallible> {
    InvalidUtf8,
    BudgetExceeded(BudgetExceeded),
    Visitor(E),
}

impl<E> LineDiffError<E> {
    pub fn from_budget(error: BudgetExceeded) -> Self {
        Self::BudgetExceeded(error)
    }
}

impl<E: std::fmt::Display> std::fmt::Display for LineDiffError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("input is not valid UTF-8"),
            Self::BudgetExceeded(error) => write!(f, "{error}"),
            Self::Visitor(error) => write!(f, "lcs visitor: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for LineDiffError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BudgetExceeded(error) => Some(error),
            Self::Visitor(error) => Some(error),
            Self::InvalidUtf8 => None,
        }
    }
}

impl<E> From<BudgetExceeded> for LineDiffError<E> {
    fn from(error: BudgetExceeded) -> Self {
        Self::BudgetExceeded(error)
    }
}

/// Successful visit: equal runs were emitted and usage is observable.
pub type LcsVisitResult<E> = Result<ResourceUsage, LineDiffError<E>>;
