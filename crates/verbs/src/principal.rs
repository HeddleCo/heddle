// SPDX-License-Identifier: Apache-2.0
//! Capture-identity resolution policy.
//!
//! Domain policy, not configuration parsing: callers hand us whatever
//! user-config principal they loaded (as an optional name/email pair) and we
//! decide which [`Principal`] captures are attributed to.

use objects::object::Principal;
use repo::Repository;

/// A principal together with the configuration surface that selected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrincipal {
    pub principal: Principal,
    pub source: Option<&'static str>,
}

impl ResolvedPrincipal {
    fn configured(principal: Principal, source: &'static str) -> Self {
        Self {
            principal,
            source: Some(source),
        }
    }

    fn unknown(principal: Principal) -> Self {
        Self {
            principal,
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
/// shared parent checkout), user config, then the built-in Unknown principal.
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
    let principal = repo.get_principal()?;
    if principal_is_accountable(&principal) {
        return Ok(ResolvedPrincipal::configured(principal, "git_config"));
    }
    Ok(finish_principal_resolution(user_principal, principal))
}

/// Resolve capture attribution when no repository is open.
///
/// Precedence is environment, then user config, then the built-in Unknown
/// principal. Repository and Git-config sources are unavailable without a repo.
pub fn resolve_principal_without_repo(user_principal: Option<(&str, &str)>) -> ResolvedPrincipal {
    if let Some(resolved) = configured_from_env() {
        return resolved;
    }
    finish_principal_resolution(
        user_principal,
        Principal::new("Unknown", "unknown@example.com"),
    )
}

fn configured_from_env() -> Option<ResolvedPrincipal> {
    Principal::from_env().map(|principal| ResolvedPrincipal::configured(principal, "environment"))
}

fn finish_principal_resolution(
    user_principal: Option<(&str, &str)>,
    fallback: Principal,
) -> ResolvedPrincipal {
    if let Some((name, email)) = user_principal {
        return ResolvedPrincipal::configured(Principal::new(name, email), "user_config");
    }
    ResolvedPrincipal::unknown(fallback)
}

/// Human-facing source label. User config is called out as global because it
/// is shared across repositories unless `HEDDLE_HOME` or `HEDDLE_CONFIG`
/// isolates it.
pub fn principal_source_display(source: &str) -> &str {
    match source {
        "user_config" => "user_config (shared global config)",
        _ => source,
    }
}

fn principal_is_accountable(principal: &Principal) -> bool {
    let name = principal.name_lossy();
    let email = principal.email_lossy();
    let name = name.trim();
    let email = email.trim();
    !name.is_empty() && !email.is_empty() && !(name == "Unknown" && email == "unknown@example.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_repo_user_pair_beats_unknown_fallback() {
        let resolved = resolve_principal_without_repo(Some(("Luke", "luke@example.com")));
        assert_eq!(resolved.source, Some("user_config"));
        assert_eq!(resolved.principal.name_lossy(), "Luke");
    }

    #[test]
    fn without_repo_missing_pair_falls_back_to_unknown() {
        let resolved = resolve_principal_without_repo(None);
        assert_eq!(resolved.source, None);
        assert_eq!(resolved.principal.email_lossy(), "unknown@example.com");
    }

    #[test]
    fn display_labels_user_config_as_global() {
        assert_eq!(
            principal_source_display("user_config"),
            "user_config (shared global config)"
        );
        assert_eq!(principal_source_display("environment"), "environment");
    }
}
