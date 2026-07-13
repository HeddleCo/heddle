// SPDX-License-Identifier: Apache-2.0
//! Pure thread-switch planning (no FS I/O).
//!
//! One plan type for verify + HEAD alias + success line. State resolution
//! and materialization stay CLI-owned.

/// Whether switch should require a clean worktree before checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchVerifyPlan {
    /// Refuse dirty worktree; use verified-clean checkout when possible.
    RequireClean,
    /// Skip cleanliness checks (`--force`).
    Skip,
}

/// Pure switch facts derived from flags + resolved short target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchPlan {
    pub verify: SwitchVerifyPlan,
    pub target_is_head_alias: bool,
    pub success_message: String,
}

impl SwitchPlan {
    /// Plan from force flag and target string (caller still resolves the state).
    pub fn plan(force: bool, target: &str, target_short: &str) -> Self {
        Self {
            verify: if force {
                SwitchVerifyPlan::Skip
            } else {
                SwitchVerifyPlan::RequireClean
            },
            target_is_head_alias: is_head_alias(target),
            success_message: format!("Now at: {target_short}"),
        }
    }
}

/// Plan worktree verification from the force flag alone.
pub fn plan_switch_worktree_verify(force: bool) -> SwitchVerifyPlan {
    SwitchPlan::plan(force, "", "").verify
}

/// True when `target` is the symbolic HEAD alias (`HEAD` or `@`).
pub fn is_head_alias(target: &str) -> bool {
    matches!(target, "HEAD" | "@")
}

/// Human success line after switch: `Now at: {short}`.
pub fn switch_success_message(target_short: &str) -> String {
    SwitchPlan::plan(false, "", target_short).success_message
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
        let p = SwitchPlan::plan(true, "HEAD", "abc1234");
        assert_eq!(p.verify, SwitchVerifyPlan::Skip);
        assert!(p.target_is_head_alias);
        assert_eq!(p.success_message, "Now at: abc1234");
    }
}
