// SPDX-License-Identifier: Apache-2.0
//! CLI enforcement for typed source-authority decisions.

use heddle_core::source_authority::{SourceAction, SourceAuthorityActions};
use repo::{Repository, RepositorySourceAuthority};

use super::{advice::RecoveryAdvice, command_catalog::checked_action_from_argv};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceOperation {
    Push,
    Pull,
}

impl SourceOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pull => "pull",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceAuthorityDispatch {
    actions: SourceAuthorityActions,
}

impl SourceAuthorityDispatch {
    pub(crate) fn for_repo(repo: &Repository) -> Self {
        Self {
            actions: SourceAuthorityActions::new(repo.source_authority()),
        }
    }

    pub(crate) fn require_push(
        self,
        remote: Option<&str>,
        thread: Option<&str>,
        force: bool,
        all_threads: bool,
    ) -> Result<(), RecoveryAdvice> {
        let mut argv = self.actions.argv(SourceAction::Push);
        if force {
            argv.push("--force-with-lease".to_string());
        }
        if all_threads {
            argv.push("--all".to_string());
        }
        if let Some(remote) = remote {
            argv.push(remote.to_string());
        }
        if let Some(thread) = thread {
            argv.push(thread.to_string());
        }
        self.require_native(SourceOperation::Push, argv, None)
    }

    pub(crate) fn require_pull(
        self,
        remote: Option<&str>,
        remote_thread: Option<&str>,
        local_thread: Option<&str>,
    ) -> Result<(), RecoveryAdvice> {
        let mut argv = self.actions.argv(SourceAction::Pull);
        if let Some(remote) = remote {
            argv.push(remote.to_string());
        }
        if let Some(thread) = remote_thread {
            argv.push(thread.to_string());
        }
        let preceding =
            local_thread.map(|thread| checked_action_from_argv(["git", "switch", thread]));
        self.require_native(SourceOperation::Pull, argv, preceding)
    }

    fn require_native(
        self,
        operation: SourceOperation,
        recovery_argv: Vec<String>,
        preceding_action: Option<String>,
    ) -> Result<(), RecoveryAdvice> {
        if self.actions.authority() == RepositorySourceAuthority::Native {
            return Ok(());
        }
        let direct_git = checked_action_from_argv(recovery_argv);
        let mut recovery_commands = preceding_action.into_iter().collect::<Vec<_>>();
        recovery_commands.push(direct_git.clone());
        recovery_commands.push("heddle adopt".to_string());
        Err(RecoveryAdvice::safety_refusal(
            "source_authority_direct_git",
            format!(
                "`heddle {}` is unavailable while Git owns source history",
                operation.label()
            ),
            format!("Run `{direct_git}` directly, or run `heddle adopt` first."),
            "repository source authority is git-overlay",
            format!(
                "`heddle {}` would mutate Git-owned source state",
                operation.label()
            ),
            "Git source state and Heddle metadata were left unchanged",
            direct_git,
            recovery_commands,
        ))
    }
}
