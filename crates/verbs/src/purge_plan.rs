// SPDX-License-Identifier: Apache-2.0
//! Pure purge apply planning (force gate + message assembly).

/// Purge apply preflight: force is required for destructive purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeApplyPlan {
    /// Proceed with purge mutation.
    Proceed,
    /// Refuse until `--force` is supplied.
    RequiresForce,
}

/// Plan purge apply from the force flag alone.
pub fn plan_purge_apply(force: bool) -> PurgeApplyPlan {
    if force {
        PurgeApplyPlan::Proceed
    } else {
        PurgeApplyPlan::RequiresForce
    }
}

/// Force command template for recovery advice.
pub fn purge_force_command(state_short: &str, path: &str) -> String {
    format!("heddle redact purge apply {state_short} --path {path} --force")
}

/// Human message after a successful purge.
pub fn purge_apply_message(
    blob_short: &str,
    path: &str,
    state_short: &str,
    redactions_marked: usize,
    blob_bytes_removed: bool,
    blob_remains_in_pack: bool,
) -> String {
    let mut message = format!(
        "purged blob {blob_short} at {path} in {state_short} ({redactions_marked} redaction(s) marked)"
    );
    if !blob_bytes_removed {
        message.push_str("\n  note: no loose copy was on disk (already gone or only in a pack)");
    }
    if blob_remains_in_pack {
        message.push_str(
            "\n  warning: bytes remain in a pack file — repack required for full removal",
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_gate_and_messages() {
        assert_eq!(plan_purge_apply(false), PurgeApplyPlan::RequiresForce);
        assert_eq!(plan_purge_apply(true), PurgeApplyPlan::Proceed);
        let cmd = purge_force_command("abc", "secrets.txt");
        assert!(cmd.contains("--force"));
        let msg = purge_apply_message("blob", "p", "st", 1, false, true);
        assert!(msg.contains("no loose copy"));
        assert!(msg.contains("pack file"));
    }
}
