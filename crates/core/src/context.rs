// SPDX-License-Identifier: Apache-2.0
//! Execution context shared by future facade operations.

use std::{path::PathBuf, sync::Arc};

use cli_shared::UserConfig;
use objects::{HeddleError, NoopProgress, NoopWarnings, ProgressSink, WarningSink};
use repo::Repository;

/// Semantic detail level for facade operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

/// Semantic execution state for embeddable Heddle operations.
pub struct ExecutionContext {
    repo: Option<Repository>,
    config: UserConfig,
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

    pub fn config(&self) -> &UserConfig {
        &self.config
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
    config: UserConfig,
    verbosity: Verbosity,
    progress: Arc<dyn ProgressSink>,
    warnings: Arc<dyn WarningSink>,
    op_id: Option<String>,
}

impl Default for ExecutionContextBuilder {
    fn default() -> Self {
        Self {
            repo: None,
            config: UserConfig::default(),
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

    pub fn config(mut self, config: UserConfig) -> Self {
        self.config = config;
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
            config: self.config,
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
            .config(UserConfig::default())
            .verbosity(Verbosity::Verbose)
            .op_id("op-123")
            .build();

        assert_eq!(ctx.verbosity(), Verbosity::Verbose);
        assert_eq!(ctx.op_id(), Some("op-123"));
    }
}
