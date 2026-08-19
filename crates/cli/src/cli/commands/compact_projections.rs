// SPDX-License-Identifier: Apache-2.0
//! Compact projections for command outputs defined outside this module.

use heddle_core::{DiffReport, VerifyReport};

use super::compact::{CompactOutput, CompactProjection};
use super::next_action::normalized_action;

impl CompactProjection for DiffReport {
    fn compact(&self) -> CompactOutput {
        let mut compact = CompactOutput::new(self.output_kind);
        compact.status = Some(self.status.to_string());
        compact.changed_path_count = Some(self.changed_path_count);
        compact.changed_paths = Some(
            self.changes
                .iter()
                .map(|change| change.path.clone())
                .collect(),
        );
        compact
    }
}

impl CompactProjection for VerifyReport {
    fn compact(&self) -> CompactOutput {
        let mut compact = CompactOutput::new(self.output_kind);
        compact.status = Some(if self.clean {
            "clean".to_string()
        } else {
            "blocked".to_string()
        });
        let next_action = normalized_action(self.trust.recommended_action.clone());
        compact.next_action_template = next_action
            .as_ref()
            .and_then(|_| self.trust.recommended_action_template.clone());
        compact.next_action = next_action;
        compact.blockers = self
            .trust
            .checks
            .iter()
            .filter(|check| !check.clean && check.status != "not_checked")
            .map(|check| format!("{}: {}", check.name, check.summary))
            .collect();
        compact
    }
}
