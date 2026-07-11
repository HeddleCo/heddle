// SPDX-License-Identifier: Apache-2.0
//! Pure git-overlay OSS guide content (no styling / stdout I/O).
//!
//! Owns the machine-facing summary, step list, and JSON payload for
//! `heddle oss` / git-overlay guide. Human formatting with ANSI styles
//! stays CLI-owned.

/// One-line machine summary for the git-overlay daily loop.
pub fn git_overlay_guide_summary() -> &'static str {
    "Use Heddle as the daily loop with explicit Git projection compatibility: status, diff, commit, start --path, ready, land, push, undo, verify."
}

/// Ordered command steps for the git-overlay guide (JSON / machine path).
pub fn git_overlay_guide_steps() -> &'static [&'static str] {
    &[
        "heddle status",
        "heddle init",
        "heddle diff",
        "heddle commit -m <message>",
        "heddle start <name> --path ../<name>",
        "heddle ready",
        "heddle land --thread <name> --no-push",
        "heddle push",
        "heddle undo",
        "heddle verify",
    ]
}

/// Topic key used in machine JSON.
pub fn git_overlay_guide_topic() -> &'static str {
    "git-overlay"
}

/// Machine JSON object for the git-overlay guide.
pub fn git_overlay_guide_json() -> serde_json::Value {
    serde_json::json!({
        "topic": git_overlay_guide_topic(),
        "summary": git_overlay_guide_summary(),
        "steps": git_overlay_guide_steps(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_steps_nonempty() {
        assert!(git_overlay_guide_summary().contains("Heddle"));
        let steps = git_overlay_guide_steps();
        assert_eq!(steps.len(), 10);
        assert_eq!(steps[0], "heddle status");
        assert_eq!(steps[steps.len() - 1], "heddle verify");
        assert_eq!(git_overlay_guide_topic(), "git-overlay");
    }

    #[test]
    fn json_payload_shape() {
        let v = git_overlay_guide_json();
        assert_eq!(v["topic"], "git-overlay");
        assert_eq!(v["summary"], git_overlay_guide_summary());
        assert!(v["steps"].is_array());
        assert_eq!(v["steps"].as_array().unwrap().len(), 10);
    }
}
