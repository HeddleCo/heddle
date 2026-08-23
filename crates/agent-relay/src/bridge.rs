// SPDX-License-Identifier: Apache-2.0
//! The port the relay needs into CLI-owned capture and checkout machinery.
//!
//! The relay records agent work (hook captures, timeline tool calls) by
//! snapshotting the worktree, and materializes thread checkouts when a
//! harness session needs one. Both go through CLI verbs' implementations
//! (`create_snapshot`, the `thread start` worktree helpers) that this crate
//! must not depend on directly — that is the cli ↔ harness cycle being cut.
//! The CLI installs an implementation once per invocation and hands it to
//! [`crate::relay_harness_event`] / [`crate::HarnessBridgeRuntime::new`].

use std::path::{Path, PathBuf};

use anyhow::Result;
use config::UserConfig;
use objects::object::StateId;
use repo::Repository;

/// One agent-attributed capture request, in the shape the CLI's
/// `create_snapshot` consumes via its agent overrides.
#[derive(Debug, Clone)]
pub struct RelayCapture {
    pub intent: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
}

pub trait HarnessCliBridge: Send + Sync {
    /// Capture the current worktree as a state attributed to the agent
    /// described by `capture`. Returns the captured state id.
    fn capture_snapshot(
        &self,
        repo: &Repository,
        user_config: &UserConfig,
        capture: RelayCapture,
    ) -> Result<String>;

    /// Validate + create the target directory for a harness-managed thread
    /// checkout; returns the resolved absolute path.
    fn prepare_worktree_target(
        &self,
        repo: &Repository,
        path: &Path,
        self_thread: Option<&str>,
    ) -> Result<PathBuf>;

    /// Materialize an isolated checkout of `base_state` at `path`.
    fn write_isolated_checkout(
        &self,
        repo: &Repository,
        path: &Path,
        base_state: &StateId,
        thread: Option<&str>,
    ) -> Result<()>;
}
