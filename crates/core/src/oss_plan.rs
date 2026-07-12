// SPDX-License-Identifier: Apache-2.0
//! Git-overlay guide content (machine path).
//!
//! Single typed value — not four micro-functions. Human ANSI formatting stays CLI.

/// Machine-facing git-overlay guide (JSON / structured output).
#[derive(Debug, Clone, PartialEq, Eq)]
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
        "heddle commit -m <message>",
        "heddle start <name> --path ../<name>",
        "heddle ready",
        "heddle land --thread <name> --no-push",
        "heddle push",
        "heddle undo",
        "heddle verify",
    ],
};

impl GitOverlayGuide {
    /// Machine JSON object for the guide.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "topic": self.topic,
            "summary": self.summary,
            "steps": self.steps,
        })
    }
}

/// Backward-compatible alias for callers that want a function form.
pub fn git_overlay_guide_json() -> serde_json::Value {
    GIT_OVERLAY_GUIDE.to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_shape() {
        assert_eq!(GIT_OVERLAY_GUIDE.topic, "git-overlay");
        assert_eq!(GIT_OVERLAY_GUIDE.steps.len(), 10);
        let v = GIT_OVERLAY_GUIDE.to_json();
        assert_eq!(v["steps"].as_array().unwrap().len(), 10);
    }
}
