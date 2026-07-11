// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle switch` message and worktree-verify planning (no FS I/O).
//!
//! Owns success message text, HEAD alias detection, and whether the
//! checkout path should require a clean worktree. State resolution and
//! materialization stay CLI-owned.

/// Whether switch should require a clean worktree before checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchVerifyPlan {
    /// Refuse dirty worktree; use verified-clean checkout when possible.
    RequireClean,
    /// Skip cleanliness checks (`--force`).
    Skip,
}

/// Plan worktree verification from the force flag alone.
pub fn plan_switch_worktree_verify(force: bool) -> SwitchVerifyPlan {
    if force {
        SwitchVerifyPlan::Skip
    } else {
        SwitchVerifyPlan::RequireClean
    }
}

/// True when `target` is the symbolic HEAD alias (`HEAD` or `@`).
pub fn is_head_alias(target: &str) -> bool {
    matches!(target, "HEAD" | "@")
}

/// Human success line after switch: `Now at: {short}`.
pub fn switch_success_message(target_short: &str) -> String {
    format!("Now at: {target_short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_aliases() {
        assert!(is_head_alias("HEAD"));
        assert!(is_head_alias("@"));
        assert!(!is_head_alias("head"));
        assert!(!is_head_alias("main"));
        assert!(!is_head_alias(""));
    }

    #[test]
    fn verify_plan_from_force() {
        assert_eq!(
            plan_switch_worktree_verify(false),
            SwitchVerifyPlan::RequireClean
        );
        assert_eq!(plan_switch_worktree_verify(true), SwitchVerifyPlan::Skip);
    }

    #[test]
    fn success_message() {
        assert_eq!(switch_success_message("abc1234"), "Now at: abc1234");
    }
}
