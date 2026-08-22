// SPDX-License-Identifier: Apache-2.0
//! Execution context shared by future facade operations.

use std::{path::PathBuf, sync::Arc};

use objects::{HeddleError, NoopProgress, NoopWarnings, ProgressSink, WarningSink};
use repo::{FsMonitorMode, Repository, WorktreeStatusOptions};

/// Semantic detail level for facade operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

/// Semantic execution state for embeddable Heddle operations.
///
/// Carries only resolved values: the caller (usually the CLI) resolves user
/// config before construction, so this crate never reads config files.
pub struct ExecutionContext {
    repo: Option<Repository>,
    start_path: Option<PathBuf>,
    principal_fallback: Option<(String, String)>,
    fsmonitor_mode: FsMonitorMode,
    verbosity: Verbosity,
    progress: Arc<dyn ProgressSink>,
    warnings: Arc<dyn WarningSink>,
    op_id: Option<String>,
    // TODO(F3): faults + semantic_cache once de-singletoned.
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

    pub fn progress(&self) -> &dyn ProgressSink {
        &*self.progress
    }

    pub fn warnings(&self) -> &dyn WarningSink {
        &*self.warnings
    }

    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    pub fn op_id(&self) -> Option<&str> {
        self.op_id.as_deref()
    }
}

/// Builder for [`ExecutionContext`].
pub struct ExecutionContextBuilder {
    repo: Option<Repository>,
    start_path: Option<PathBuf>,
    principal_fallback: Option<(String, String)>,
    fsmonitor_mode: FsMonitorMode,
    verbosity: Verbosity,
    progress: Arc<dyn ProgressSink>,
    warnings: Arc<dyn WarningSink>,
    op_id: Option<String>,
}

impl Default for ExecutionContextBuilder {
    fn default() -> Self {
        Self {
            repo: None,
            start_path: None,
            principal_fallback: None,
            fsmonitor_mode: FsMonitorMode::default(),
            verbosity: Verbosity::Normal,
            progress: Arc::new(NoopProgress),
            warnings: Arc::new(NoopWarnings),
            op_id: None,
        }
    }
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

    pub fn fsmonitor_mode(mut self, mode: FsMonitorMode) -> Self {
        self.fsmonitor_mode = mode;
        self
    }

    pub fn verbosity(mut self, verbosity: Verbosity) -> Self {
        self.verbosity = verbosity;
        self
    }

    pub fn progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    pub fn warnings(mut self, warnings: Arc<dyn WarningSink>) -> Self {
        self.warnings = warnings;
        self
    }

    pub fn op_id(mut self, op_id: impl Into<String>) -> Self {
        self.op_id = Some(op_id.into());
        self
    }

    pub fn build(self) -> ExecutionContext {
        ExecutionContext {
            repo: self.repo,
            start_path: self.start_path,
            principal_fallback: self.principal_fallback,
            fsmonitor_mode: self.fsmonitor_mode,
            verbosity: self.verbosity,
            progress: self.progress,
            warnings: self.warnings,
            op_id: self.op_id,
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
        assert_eq!(ctx.verbosity(), Verbosity::Normal);
        assert!(ctx.op_id().is_none());
        assert_eq!(ctx.fsmonitor_mode(), FsMonitorMode::Off);
        assert!(ctx.principal_fallback().is_none());
        ctx.progress().event(objects::ProgressEvent::Finish {
            id: objects::TaskId(1),
        });
        ctx.warnings().warn(objects::Warning {
            kind: "test".into(),
            message: "ignored".to_string(),
        });
    }

    #[test]
    fn builder_sets_non_repo_fields() {
        let ctx = ExecutionContext::builder()
            .start_path("/tmp/heddle-verbs-context-test")
            .principal_fallback(Some(("Luke".into(), "luke@example.com".into())))
            .fsmonitor_mode(FsMonitorMode::Watchman)
            .verbosity(Verbosity::Verbose)
            .op_id("op-123")
            .build();

        assert_eq!(ctx.verbosity(), Verbosity::Verbose);
        assert_eq!(ctx.op_id(), Some("op-123"));
        assert_eq!(
            ctx.start_path(),
            Some(std::path::Path::new("/tmp/heddle-verbs-context-test"))
        );
        assert_eq!(
            ctx.principal_fallback(),
            Some(("Luke", "luke@example.com"))
        );
        assert_eq!(
            ctx.worktree_status_options().fsmonitor.mode,
            FsMonitorMode::Watchman
        );
    }
}
