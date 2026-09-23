// SPDX-License-Identifier: Apache-2.0
//! Execution context shared by future facade operations.

use std::path::PathBuf;

use objects::HeddleError;
use repo::{FsMonitorMode, Repository, WorktreeStatusOptions};

/// Semantic execution state for embeddable Heddle operations.
///
/// Carries only resolved values: the caller (usually the CLI) resolves user
/// config before construction, so this crate never reads config files.
pub struct ExecutionContext {
    repo: Option<Repository>,
    start_path: Option<PathBuf>,
    principal_fallback: Option<(String, String)>,
    hosted_principal: Option<(String, String)>,
    hosted_account_unclaimed: bool,
    fsmonitor_mode: FsMonitorMode,
}

impl ExecutionContext {
    pub fn builder() -> ExecutionContextBuilder {
        ExecutionContextBuilder::default()
    }

    pub fn require_repo(&self) -> Result<&Repository, HeddleError> {
        self.repo
            .as_ref()
            .ok_or_else(|| HeddleError::RepositoryNotFound(PathBuf::from(".")))
    }

    pub fn repo(&self) -> Option<&Repository> {
        self.repo.as_ref()
    }

    pub fn start_path(&self) -> Option<&std::path::Path> {
        self.start_path.as_deref()
    }

    /// User-config principal fallback as an optional `(name, email)` pair.
    pub fn principal_fallback(&self) -> Option<(&str, &str)> {
        self.principal_fallback
            .as_ref()
            .map(|(name, email)| (name.as_str(), email.as_str()))
    }

    /// Hosted-account principal derived from a local login, used only when no
    /// local principal is configured.
    pub fn hosted_principal(&self) -> Option<(&str, &str)> {
        self.hosted_principal
            .as_ref()
            .map(|(name, email)| (name.as_str(), email.as_str()))
    }

    /// Whether local hosted state identifies an account that still needs its
    /// human claim ceremony before it can supply accountable attribution.
    pub fn hosted_account_unclaimed(&self) -> bool {
        self.hosted_account_unclaimed
    }

    /// Resolved fsmonitor mode for worktree-status hot paths.
    pub fn fsmonitor_mode(&self) -> FsMonitorMode {
        self.fsmonitor_mode
    }

    pub fn worktree_status_options(&self) -> WorktreeStatusOptions {
        WorktreeStatusOptions {
            fsmonitor: repo::FsMonitorSettings {
                mode: self.fsmonitor_mode,
            },
        }
    }
}

/// Builder for [`ExecutionContext`].
#[derive(Default)]
pub struct ExecutionContextBuilder {
    repo: Option<Repository>,
    start_path: Option<PathBuf>,
    principal_fallback: Option<(String, String)>,
    hosted_principal: Option<(String, String)>,
    hosted_account_unclaimed: bool,
    fsmonitor_mode: FsMonitorMode,
}

impl ExecutionContextBuilder {
    pub fn repo(mut self, repo: Repository) -> Self {
        self.repo = Some(repo);
        self
    }

    pub fn start_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.start_path = Some(path.into());
        self
    }

    pub fn principal_fallback(mut self, principal: Option<(String, String)>) -> Self {
        self.principal_fallback = principal;
        self
    }

    pub fn hosted_principal(mut self, principal: Option<(String, String)>) -> Self {
        self.hosted_principal = principal;
        self
    }

    pub fn hosted_account_unclaimed(mut self, unclaimed: bool) -> Self {
        self.hosted_account_unclaimed = unclaimed;
        self
    }

    pub fn fsmonitor_mode(mut self, mode: FsMonitorMode) -> Self {
        self.fsmonitor_mode = mode;
        self
    }

    pub fn build(self) -> ExecutionContext {
        ExecutionContext {
            repo: self.repo,
            start_path: self.start_path,
            principal_fallback: self.principal_fallback,
            hosted_principal: self.hosted_principal,
            hosted_account_unclaimed: self.hosted_account_unclaimed,
            fsmonitor_mode: self.fsmonitor_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_context_has_no_repo_and_noop_sinks() {
        let ctx = ExecutionContext::builder().build();

        assert!(matches!(
            ctx.require_repo(),
            Err(HeddleError::RepositoryNotFound(_))
        ));
        assert_eq!(ctx.fsmonitor_mode(), FsMonitorMode::Off);
        assert!(ctx.principal_fallback().is_none());
        assert!(!ctx.hosted_account_unclaimed());
    }

    #[test]
    fn builder_sets_non_repo_fields() {
        let ctx = ExecutionContext::builder()
            .start_path("/tmp/heddle-verbs-context-test")
            .principal_fallback(Some(("Luke".into(), "luke@example.com".into())))
            .hosted_account_unclaimed(true)
            .fsmonitor_mode(FsMonitorMode::Watchman)
            .build();

        assert_eq!(
            ctx.start_path(),
            Some(std::path::Path::new("/tmp/heddle-verbs-context-test"))
        );
        assert_eq!(ctx.principal_fallback(), Some(("Luke", "luke@example.com")));
        assert!(ctx.hosted_account_unclaimed());
        assert_eq!(
            ctx.worktree_status_options().fsmonitor.mode,
            FsMonitorMode::Watchman
        );
    }
}
