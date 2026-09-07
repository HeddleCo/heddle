// SPDX-License-Identifier: Apache-2.0
//! Terminal Adapter for structured library warnings.

use objects::{Warning, WarningSink};

/// Render structured warnings on stderr for the human CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrWarningSink;

impl WarningSink for StderrWarningSink {
    fn warn(&self, warning: Warning) {
        eprintln!("{} {}", super::style::warn_marker(), warning.message);
    }
}
