// SPDX-License-Identifier: Apache-2.0
//! In-progress operation detection: is a merge/rebase/cherry-pick/revert/
//! bisect mid-flight, in either the Heddle or the Git overlay state model,
//! and what should the user run next.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use objects::error::Result;

use super::{Repository, RepositoryCapability};
use super::overlay::resolve_git_dir;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OperationScope {
    Git,
    Heddle,
}

impl std::fmt::Display for OperationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git => write!(f, "git"),
            Self::Heddle => write!(f, "heddle"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl std::fmt::Display for OperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Merge => write!(f, "merge"),
            Self::Rebase => write!(f, "rebase"),
            Self::CherryPick => write!(f, "cherry-pick"),
            Self::Revert => write!(f, "revert"),
            Self::Bisect => write!(f, "bisect"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepositoryOperationStatus {
    pub scope: OperationScope,
    pub kind: OperationKind,
    pub in_progress: bool,
    pub state: String,
    pub message: String,
    #[schemars(with = "Option<String>")]
    pub next_action: String,
}

impl Repository {
    pub fn operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if let Some(status) = self.heddle_operation_status()? {
            return Ok(Some(status));
        }
        self.git_operation_status()
    }

    fn heddle_operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if self.merge_state_manager().is_merge_in_progress() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Merge,
                in_progress: true,
                state: "in-progress".to_string(),
                message: "Heddle merge is in progress".to_string(),
                next_action: "heddle continue".to_string(),
            }));
        }

        let rebase_state = self.heddle_dir.join("REBASE_STATE");
        if rebase_state.exists() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Rebase,
                in_progress: true,
                state: "in-progress".to_string(),
                message: "Heddle rebase is in progress".to_string(),
                next_action: "heddle continue".to_string(),
            }));
        }

        let bisect_state = self.heddle_dir.join("BISECT_STATE");
        if bisect_state.exists() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Bisect,
                in_progress: true,
                state: "in-progress".to_string(),
                // The `bisect` verb was removed in the whole-CLI consolidation
                // (heddle#473); a lingering BISECT_STATE can only come from an
                // older binary, and the only valid recovery now is to abort.
                message: "Heddle bisect is in progress".to_string(),
                next_action: "heddle abort".to_string(),
            }));
        }

        Ok(None)
    }

    fn git_operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let git_dir = resolve_git_dir(&self.root)?;
        let raw_git_next_action = "heddle verify";
        let candidates = [
            (
                git_dir.join("rebase-merge"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("rebase-apply"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("MERGE_HEAD"),
                OperationKind::Merge,
                "Git merge is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("CHERRY_PICK_HEAD"),
                OperationKind::CherryPick,
                "Git cherry-pick is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("REVERT_HEAD"),
                OperationKind::Revert,
                "Git revert is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("BISECT_LOG"),
                OperationKind::Bisect,
                "Git bisect is in progress",
                raw_git_next_action,
            ),
        ];

        for (path, kind, message, next_action) in candidates {
            if path.exists() {
                return Ok(Some(RepositoryOperationStatus {
                    scope: OperationScope::Git,
                    kind,
                    in_progress: true,
                    state: "in-progress".to_string(),
                    message: message.to_string(),
                    next_action: next_action.to_string(),
                }));
            }
        }

        Ok(None)
    }
}
