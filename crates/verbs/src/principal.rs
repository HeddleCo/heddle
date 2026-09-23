// SPDX-License-Identifier: Apache-2.0
//! Capture-identity resolution policy.
//!
//! Domain policy, not configuration parsing: callers hand us whatever
//! user-config principal they loaded (as an optional name/email pair) and we
//! decide which [`Principal`] captures are attributed to.

use objects::object::Principal;
use repo::Repository;

use crate::ExecutionContext;

/// A principal together with the configuration surface that selected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrincipal {
    pub principal: Option<Principal>,
    pub source: Option<&'static str>,
}

impl ResolvedPrincipal {
    fn configured(principal: Principal, source: &'static str) -> Self {
        if principal.name_lossy().trim().is_empty() || principal.email_lossy().trim().is_empty() {
            return Self::unconfigured();
        }
        Self {
            principal: Some(principal),
            source: Some(source),
        }
    }

    fn unconfigured() -> Self {
        Self {
            principal: None,
            source: None,
        }
    }
}

/// Resolve capture attribution once for init, status, capture, and other
/// identity-bearing commands.
///
/// `user_principal` is the optional `(name, email)` pair from user config.
///
/// Precedence is environment, repository config, Git config (including a
/// shared parent checkout), then user config.
pub fn resolve_principal(
    repo: &Repository,
    user_principal: Option<(&str, &str)>,
) -> repo::Result<ResolvedPrincipal> {
    if let Some(resolved) = configured_from_env() {
        return Ok(resolved);
    }
    if let Some(config) = &repo.config().principal {
        return Ok(ResolvedPrincipal::configured(
            Principal::new(&config.name, &config.email),
            "repository",
        ));
    }
    if let Some(principal) = repo.configured_principal()? {
        return Ok(ResolvedPrincipal::configured(principal, "git_config"));
    }
    Ok(finish_principal_resolution(user_principal))
}

/// Resolve capture attribution when no repository is open.
///
/// Precedence is environment, then user config. Repository and Git-config
/// sources are unavailable without a repo.
pub fn resolve_principal_without_repo(user_principal: Option<(&str, &str)>) -> ResolvedPrincipal {
    if let Some(resolved) = configured_from_env() {
        return resolved;
    }
    finish_principal_resolution(user_principal)
}

fn configured_from_env() -> Option<ResolvedPrincipal> {
    Principal::from_env().map(|principal| ResolvedPrincipal::configured(principal, "environment"))
}

fn finish_principal_resolution(user_principal: Option<(&str, &str)>) -> ResolvedPrincipal {
    if let Some((name, email)) = user_principal {
        return ResolvedPrincipal::configured(Principal::new(name, email), "user_config");
    }
    ResolvedPrincipal::unconfigured()
}

/// Resolve capture attribution from an execution context, including a hosted
/// account fallback when no local principal is configured.
pub fn resolve_principal_from_context(
    repo: &Repository,
    ctx: &ExecutionContext,
) -> repo::Result<ResolvedPrincipal> {
    let resolved = resolve_principal(repo, ctx.principal_fallback())?;
    Ok(apply_hosted_principal_fallback(
        resolved,
        ctx.hosted_principal(),
    ))
}

/// Use a hosted-account identity only when no local principal resolved.
pub fn apply_hosted_principal_fallback(
    resolved: ResolvedPrincipal,
    hosted: Option<(&str, &str)>,
) -> ResolvedPrincipal {
    if resolved.principal.is_some() {
        return resolved;
    }
    let Some((name, email)) = hosted else {
        return resolved;
    };
    if name.trim().is_empty() {
        return resolved;
    }
    ResolvedPrincipal::configured(Principal::new(name, email), "hosted_account")
}

/// Human-facing source label. User config is called out as global because it
/// is shared across repositories unless `HEDDLE_HOME` or `HEDDLE_CONFIG`
/// isolates it.
pub fn principal_source_display(source: &str) -> &str {
    match source {
        "user_config" => "user_config (shared global config)",
        "hosted_account" => "hosted_account",
        _ => source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_repo_user_pair_beats_unknown_fallback() {
        let resolved = resolve_principal_without_repo(Some(("Luke", "luke@example.com")));
        assert_eq!(resolved.source, Some("user_config"));
        assert_eq!(
            resolved
                .principal
                .as_ref()
                .expect("configured")
                .name_lossy(),
            "Luke"
        );
    }

    #[test]
    fn without_repo_missing_pair_is_unconfigured() {
        let resolved = resolve_principal_without_repo(None);
        assert_eq!(resolved.source, None);
        assert!(resolved.principal.is_none());
    }

    #[test]
    fn display_labels_user_config_as_global() {
        assert_eq!(
            principal_source_display("user_config"),
            "user_config (shared global config)"
        );
        assert_eq!(principal_source_display("environment"), "environment");
        assert_eq!(principal_source_display("hosted_account"), "hosted_account");
    }

    #[test]
    fn hosted_fallback_fills_unaccountable_local_principal() {
        let unconfigured = resolve_principal_without_repo(None);
        let derived =
            apply_hosted_principal_fallback(unconfigured, Some(("luke", "luke@example.com")));
        assert_eq!(derived.source, Some("hosted_account"));
        assert_eq!(
            derived.principal.as_ref().expect("hosted").name_lossy(),
            "luke"
        );
        assert_eq!(
            derived.principal.as_ref().expect("hosted").email_lossy(),
            "luke@example.com"
        );
    }

    #[test]
    fn hosted_fallback_does_not_override_configured_principal() {
        let local = resolve_principal_without_repo(Some(("Ada", "ada@example.com")));
        let derived =
            apply_hosted_principal_fallback(local.clone(), Some(("luke", "luke@example.com")));
        assert_eq!(derived, local);
    }
}
