// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle init` planning: principal status, side-effects list, paths.

use std::path::{Path, PathBuf};

use objects::object::Principal;

use crate::principal_lacks_accountable_identity;

/// Recommended command when no principal is configured.
pub const SET_PRINCIPAL_COMMAND: &str =
    "heddle init --principal-name <name> --principal-email <email>";

/// Pure principal configuration status for init output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPrincipalPlan {
    pub status: &'static str,
    pub source: Option<&'static str>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub recommended_action: Option<&'static str>,
}

impl InitPrincipalPlan {
    pub fn configured(source: &'static str, principal: &Principal) -> Self {
        Self {
            status: "configured",
            source: Some(source),
            name: Some(principal.name.clone()),
            email: Some(principal.email.clone()),
            recommended_action: None,
        }
    }

    pub fn not_configured() -> Self {
        Self {
            status: "not_configured",
            source: None,
            name: None,
            email: None,
            recommended_action: Some(SET_PRINCIPAL_COMMAND),
        }
    }
}

/// Whether a principal lacks accountable identity (same policy as capture).
pub fn principal_is_unconfigured(principal: &Principal) -> bool {
    principal_lacks_accountable_identity(&principal.name, &principal.email)
}

/// Prefer the first configured principal among ordered candidates.
///
/// Each entry is `(source_label, principal)`. Callers gather env/repo/git/user
/// facts and pass them in precedence order.
pub fn select_init_principal(candidates: &[(&'static str, Principal)]) -> InitPrincipalPlan {
    for (source, principal) in candidates {
        if !principal_is_unconfigured(principal) {
            return InitPrincipalPlan::configured(source, principal);
        }
    }
    InitPrincipalPlan::not_configured()
}

/// Side-effect lines for init human/JSON output.
pub fn init_side_effects(has_git: bool, principal_configured: bool) -> Vec<String> {
    let mut side_effects = Vec::new();
    if has_git {
        side_effects.push("created Heddle sidecar for the existing Git repository".to_string());
        side_effects.push("updated .git/info/exclude for Heddle metadata".to_string());
        side_effects.push("left Git-tracked files untouched".to_string());
    } else {
        side_effects.push("created Heddle repository metadata".to_string());
    }
    if principal_configured {
        side_effects.push("updated default principal attribution".to_string());
    }
    side_effects
}

/// Resolve a path against a known current directory (no ambient cwd read).
pub fn resolve_absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Default next action after a successful init.
pub fn init_recommended_action() -> &'static str {
    "heddle capture -m \"...\""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_selection_and_side_effects() {
        let unknown = Principal::new("Unknown", "unknown@example.com");
        let ada = Principal::new("Ada", "ada@example.com");
        let plan = select_init_principal(&[("environment", unknown), ("user_config", ada)]);
        assert_eq!(plan.status, "configured");
        assert_eq!(plan.source, Some("user_config"));
        assert_eq!(plan.name.as_deref(), Some("Ada"));

        let empty = select_init_principal(&[]);
        assert_eq!(empty.status, "not_configured");
        assert_eq!(empty.recommended_action, Some(SET_PRINCIPAL_COMMAND));

        let se = init_side_effects(true, true);
        assert!(se.iter().any(|s| s.contains("sidecar")));
        assert!(se.iter().any(|s| s.contains("principal")));

        assert_eq!(
            resolve_absolute_path(Path::new("/cwd"), Path::new("rel")),
            PathBuf::from("/cwd/rel")
        );
        assert_eq!(
            resolve_absolute_path(Path::new("/cwd"), Path::new("/abs")),
            PathBuf::from("/abs")
        );
    }
}
