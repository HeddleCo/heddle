// SPDX-License-Identifier: Apache-2.0
//! Pure harness integration planning (no FS / env / current_exe I/O).
//!
//! Owns scope/path-mode parsing, harness name normalization, scope rules,
//! command path-mode classification, and status message assembly from
//! primitive facts. Manifest I/O, install writers, and RecoveryAdvice stay
//! CLI-owned.

/// Install / manifest scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationScopeKind {
    Repo,
    User,
}

/// Invalid `--scope` values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationScopeError {
    Invalid { value: String },
}

impl IntegrationScopeError {
    pub fn kind(&self) -> &'static str {
        "integration_scope_invalid"
    }
}

/// Parse `repo` / `user` scope tokens.
pub fn parse_scope(s: &str) -> Result<IntegrationScopeKind, IntegrationScopeError> {
    match s {
        "repo" => Ok(IntegrationScopeKind::Repo),
        "user" => Ok(IntegrationScopeKind::User),
        other => Err(IntegrationScopeError::Invalid {
            value: other.to_string(),
        }),
    }
}

impl IntegrationScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::User => "user",
        }
    }
}

/// Whether installed hooks invoke `heddle` via PATH or an absolute path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathModeKind {
    #[default]
    Relative,
    Absolute,
}

impl PathModeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relative => "relative",
            Self::Absolute => "absolute",
        }
    }
}

/// PATH-relative heddle invocation token.
pub fn relative_heddle_invocation() -> &'static str {
    "heddle"
}

/// Map the CLI `--absolute-path` flag to a path mode.
pub fn path_mode_from_absolute_flag(absolute: bool) -> PathModeKind {
    if absolute {
        PathModeKind::Absolute
    } else {
        PathModeKind::Relative
    }
}

/// A command line is PATH-relative iff its first whitespace-delimited token
/// (optional shell single-quotes stripped) is exactly `heddle`.
pub fn classify_command_path_mode(cmd: &str) -> PathModeKind {
    let first = cmd
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('\'');
    if first == relative_heddle_invocation() {
        PathModeKind::Relative
    } else {
        PathModeKind::Absolute
    }
}

/// Probe OpenCode plugin script text for relative vs absolute `Bun.spawnSync`.
pub fn classify_opencode_plugin_path_mode(contents: &str) -> Option<PathModeKind> {
    if contents.contains("Bun.spawnSync([\"heddle\"")
        || contents.contains("Bun.spawnSync(['heddle'")
    {
        Some(PathModeKind::Relative)
    } else if contents.contains("Bun.spawnSync([\"/") || contents.contains("Bun.spawnSync(['/") {
        Some(PathModeKind::Absolute)
    } else {
        None
    }
}

/// Canonical harness names accepted by install/uninstall/upgrade.
pub const SUPPORTED_HARNESSES: &[&str] = &["codex", "claude-code", "opencode"];

/// Unsupported harness name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationHarnessError {
    Unsupported { harness: String },
}

impl IntegrationHarnessError {
    pub fn kind(&self) -> &'static str {
        "integration_harness_unsupported"
    }
}

/// Normalize a single harness token (`claude` → `claude-code`).
pub fn normalize_harness_name(name: &str) -> Result<&'static str, IntegrationHarnessError> {
    match name.trim() {
        "" => Err(IntegrationHarnessError::Unsupported {
            harness: name.to_string(),
        }),
        "claude" | "claude-code" => Ok("claude-code"),
        "codex" => Ok("codex"),
        "opencode" => Ok("opencode"),
        other => Err(IntegrationHarnessError::Unsupported {
            harness: other.to_string(),
        }),
    }
}

/// Normalize a list of harness tokens, de-duplicating in sorted order.
pub fn normalize_harness_names(
    harnesses: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Vec<String>, IntegrationHarnessError> {
    let mut seen = std::collections::BTreeSet::new();
    for harness in harnesses {
        let raw = harness.as_ref();
        if raw.trim().is_empty() {
            continue;
        }
        seen.insert(normalize_harness_name(raw)?.to_string());
    }
    Ok(seen.into_iter().collect())
}

/// Harness rejected the chosen install scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationHarnessScopeError {
    /// Codex currently requires user scope.
    CodexRequiresUser,
}

impl IntegrationHarnessScopeError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CodexRequiresUser => "integration_codex_scope_invalid",
        }
    }
}

/// Scope rule shared by preflight and install paths.
pub fn validate_harness_scope(
    harness: &str,
    scope: IntegrationScopeKind,
) -> Result<(), IntegrationHarnessScopeError> {
    match harness {
        "codex" if scope != IntegrationScopeKind::User => {
            Err(IntegrationHarnessScopeError::CodexRequiresUser)
        }
        _ => Ok(()),
    }
}

/// Validate every harness against a parsed scope.
pub fn validate_install_plan(
    harnesses: &[String],
    scope: IntegrationScopeKind,
) -> Result<(), IntegrationHarnessScopeError> {
    for harness in harnesses {
        validate_harness_scope(harness, scope)?;
    }
    Ok(())
}

/// Pure selection parse before auto-detection / FS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSelectionPlan {
    /// Explicit empty install (`none`).
    None,
    /// Detect from environment / tree (`auto`).
    Auto,
    /// Explicit harness list.
    Explicit(Vec<String>),
}

/// Parse `--install-harnesses` selection without PATH/directory probes.
pub fn plan_harness_selection(
    selection: &str,
) -> Result<HarnessSelectionPlan, IntegrationHarnessError> {
    match selection {
        "none" => Ok(HarnessSelectionPlan::None),
        "auto" => Ok(HarnessSelectionPlan::Auto),
        value => Ok(HarnessSelectionPlan::Explicit(normalize_harness_names(
            value.split(',').map(|item| item.to_string()),
        )?)),
    }
}

/// Whether a claude settings body still contains the Heddle relay marker.
pub fn claude_settings_has_relay(contents: &str) -> bool {
    contents.contains("heddle integration relay claude-code")
}

/// Whether a codex config body still contains the Heddle notify marker.
pub fn codex_config_has_relay(contents: &str) -> bool {
    contents.contains("integration relay codex notify")
}

/// Timeline capability path filter for opencode installs.
pub fn is_timeline_capability_path(path: &str) -> bool {
    path.ends_with("heddle.timeline.json")
}

/// Capability list for an installed integration from pure facts.
pub fn integration_capabilities(harness: &str, has_timeline_paths: bool) -> Vec<String> {
    if harness == "opencode" && has_timeline_paths {
        vec!["timeline".to_string()]
    } else {
        Vec::new()
    }
}

/// Human list/doctor empty state.
pub fn empty_integrations_message() -> &'static str {
    "No Heddle-managed harness integrations."
}

/// Install success message.
pub fn installed_message(harnesses: &[String]) -> String {
    format!(
        "Installed Heddle harness integrations for: {}",
        harnesses.join(", ")
    )
}

/// Uninstall success message.
pub fn uninstalled_message(harnesses: &[String]) -> String {
    format!(
        "Uninstalled Heddle harness integrations for: {}",
        harnesses.join(", ")
    )
}

/// Upgrade success message.
pub fn upgraded_message(harnesses: &[String]) -> String {
    format!(
        "Upgraded Heddle harness integrations for: {}",
        harnesses.join(", ")
    )
}

/// One list-mode status line body.
pub fn list_status_line(harness: &str, scope: &str, status: &str, method: &str) -> String {
    format!("{harness} [{scope}] {status} ({method})")
}

/// One doctor-mode status line body.
pub fn doctor_status_line(
    harness: &str,
    scope: &str,
    path_mode: &str,
    healthy: bool,
    status: &str,
) -> String {
    let health = if healthy { "healthy" } else { status };
    format!("{harness} [{scope}] (path: {path_mode}): {health}")
}

/// Health status token when a managed path is missing.
pub fn missing_status_token() -> &'static str {
    "missing"
}

/// Health status token when config drifted from Heddle markers.
pub fn drifted_status_token() -> &'static str {
    "drifted"
}

/// Healthy status token.
pub fn healthy_status_token() -> &'static str {
    "healthy"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parse_and_labels() {
        assert_eq!(parse_scope("repo").unwrap(), IntegrationScopeKind::Repo);
        assert_eq!(parse_scope("user").unwrap(), IntegrationScopeKind::User);
        assert!(matches!(
            parse_scope("workspace"),
            Err(IntegrationScopeError::Invalid { value }) if value == "workspace"
        ));
        assert_eq!(IntegrationScopeKind::Repo.as_str(), "repo");
        assert_eq!(IntegrationScopeKind::User.as_str(), "user");
    }

    #[test]
    fn path_mode_classification() {
        assert_eq!(path_mode_from_absolute_flag(false), PathModeKind::Relative);
        assert_eq!(path_mode_from_absolute_flag(true), PathModeKind::Absolute);
        assert_eq!(relative_heddle_invocation(), "heddle");
        assert_eq!(
            classify_command_path_mode(
                "heddle --repo /some/path integration relay claude-code Stop"
            ),
            PathModeKind::Relative
        );
        assert_eq!(
            classify_command_path_mode(
                "/Users/dev/.cargo/bin/heddle --repo /repo integration relay claude-code Stop"
            ),
            PathModeKind::Absolute
        );
        assert_eq!(
            classify_command_path_mode(
                "'/Users/dev/.cargo/bin/heddle' --repo /repo integration relay claude-code Stop"
            ),
            PathModeKind::Absolute
        );
        assert_eq!(
            classify_opencode_plugin_path_mode("Bun.spawnSync([\"heddle\", '--repo']"),
            Some(PathModeKind::Relative)
        );
        assert_eq!(
            classify_opencode_plugin_path_mode("Bun.spawnSync([\"/usr/bin/heddle\", '--repo']"),
            Some(PathModeKind::Absolute)
        );
        assert_eq!(classify_opencode_plugin_path_mode("no spawn here"), None);
    }

    #[test]
    fn harness_normalize_and_scope_rules() {
        assert_eq!(normalize_harness_name("claude").unwrap(), "claude-code");
        assert_eq!(normalize_harness_name("codex").unwrap(), "codex");
        assert!(matches!(
            normalize_harness_name("windsurf"),
            Err(IntegrationHarnessError::Unsupported { harness }) if harness == "windsurf"
        ));
        let names = normalize_harness_names(["claude", "codex", "claude-code"]).unwrap();
        assert_eq!(names, vec!["claude-code".to_string(), "codex".to_string()]);

        assert!(validate_harness_scope("codex", IntegrationScopeKind::User).is_ok());
        assert_eq!(
            validate_harness_scope("codex", IntegrationScopeKind::Repo),
            Err(IntegrationHarnessScopeError::CodexRequiresUser)
        );
        assert!(validate_harness_scope("claude-code", IntegrationScopeKind::Repo).is_ok());
        assert!(validate_install_plan(&["codex".into()], IntegrationScopeKind::Repo).is_err());
    }

    #[test]
    fn selection_and_messages() {
        assert_eq!(
            plan_harness_selection("none").unwrap(),
            HarnessSelectionPlan::None
        );
        assert_eq!(
            plan_harness_selection("auto").unwrap(),
            HarnessSelectionPlan::Auto
        );
        assert_eq!(
            plan_harness_selection("codex,claude").unwrap(),
            HarnessSelectionPlan::Explicit(vec!["claude-code".into(), "codex".into()])
        );
        assert_eq!(
            installed_message(&["codex".into()]),
            "Installed Heddle harness integrations for: codex"
        );
        assert!(uninstalled_message(&["a".into()]).contains("Uninstalled"));
        assert!(upgraded_message(&["a".into()]).contains("Upgraded"));
        assert_eq!(
            empty_integrations_message(),
            "No Heddle-managed harness integrations."
        );
        assert_eq!(
            list_status_line("codex", "user", "healthy", "notify"),
            "codex [user] healthy (notify)"
        );
        assert_eq!(
            doctor_status_line("codex", "user", "relative", true, "healthy"),
            "codex [user] (path: relative): healthy"
        );
        assert_eq!(
            doctor_status_line("codex", "user", "relative", false, "missing"),
            "codex [user] (path: relative): missing"
        );
    }

    #[test]
    fn health_and_capability_helpers() {
        assert!(claude_settings_has_relay(
            "heddle integration relay claude-code SessionStart"
        ));
        assert!(!claude_settings_has_relay("{}"));
        assert!(codex_config_has_relay("integration relay codex notify"));
        assert!(is_timeline_capability_path(
            "/repo/.opencode/plugins/heddle.timeline.json"
        ));
        assert!(!is_timeline_capability_path("heddle.js"));
        assert_eq!(
            integration_capabilities("opencode", true),
            vec!["timeline".to_string()]
        );
        assert!(integration_capabilities("opencode", false).is_empty());
        assert!(integration_capabilities("codex", true).is_empty());
    }
}
