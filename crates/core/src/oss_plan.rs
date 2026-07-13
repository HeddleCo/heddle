// SPDX-License-Identifier: Apache-2.0
//! Git-overlay guide content (machine path).
//!
//! Single typed value — not micro-functions. Human ANSI formatting stays CLI.

use serde::Serialize;

/// Machine-facing git-overlay guide (JSON / structured output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitOverlayGuide {
    pub topic: &'static str,
    pub summary: &'static str,
    pub steps: &'static [&'static str],
}

/// Canonical git-overlay guide payload.
pub const GIT_OVERLAY_GUIDE: GitOverlayGuide = GitOverlayGuide {
    topic: "git-overlay",
    summary: "Use Heddle as the daily loop with explicit Git projection compatibility: status, diff, commit, start --path, ready, land, push, undo, verify.",
    steps: &[
        "heddle status",
        "heddle init",
        "heddle diff",
        "heddle capture -m <message>",
        "heddle start <name> --path ../<name>",
        "heddle ready",
        "heddle land --thread <name>",
        "heddle push",
        "heddle undo",
        "heddle verify",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_shape() {
        assert_eq!(GIT_OVERLAY_GUIDE.topic, "git-overlay");
        assert_eq!(GIT_OVERLAY_GUIDE.steps.len(), 10);
        let v = serde_json::to_value(&GIT_OVERLAY_GUIDE).unwrap();
        assert_eq!(v["steps"].as_array().unwrap().len(), 10);
    }
}
