// SPDX-License-Identifier: Apache-2.0
//! Command-boundary auto-capture.

use anyhow::Result;
use repo::Repository;

use super::{
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
};
use crate::{
    cli::{Cli, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoCaptureTrigger {
    Push,
    Sync,
}

impl AutoCaptureTrigger {
    fn command(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Sync => "sync",
        }
    }

    fn intent(self) -> String {
        format!("Auto capture before {}", self.command())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoCaptureOutcome {
    pub trigger: AutoCaptureTrigger,
    pub change_id: String,
}

pub(crate) fn auto_capture_command_boundary(
    cli: &Cli,
    repo: &Repository,
    user_config: &UserConfig,
    trigger: AutoCaptureTrigger,
) -> Result<Option<AutoCaptureOutcome>> {
    if !user_config.command_auto_capture_enabled()? {
        return Ok(None);
    }

    // Avoid manufacturing states while the operator is in the middle of
    // resolving a merge or higher-level transaction. Those surfaces already
    // have explicit capture/continue flows.
    if repo.merge_state_manager().is_merge_in_progress() || repo.operation_status()?.is_some() {
        return Ok(None);
    }

    let status_options = worktree_status_options(Some(repo.config()));
    if !worktree_dirty(repo, &status_options)? {
        return Ok(None);
    }

    let snapshot = create_snapshot(
        repo,
        user_config,
        Some(trigger.intent()),
        None,
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    let outcome = AutoCaptureOutcome {
        trigger,
        change_id: snapshot.change_id,
    };
    emit_auto_capture(cli, repo, &outcome);
    Ok(Some(outcome))
}

fn emit_auto_capture(cli: &Cli, repo: &Repository, outcome: &AutoCaptureOutcome) {
    if cli.quiet {
        return;
    }
    if should_output_json(cli, Some(repo.config())) {
        let record = serde_json::json!({
            "output_kind": "auto_capture",
            "status": "captured",
            "trigger": outcome.trigger.command(),
            "change_id": outcome.change_id,
        });
        eprintln!("{}", record);
    } else {
        println!(
            "{} auto-captured dirty worktree before {} as {}",
            style::ok_marker(),
            outcome.trigger.command(),
            style::change_id(&outcome.change_id),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::MutexGuard;

    use clap::Parser;

    use super::*;
    use crate::config::{UserAutoCaptureMode, UserCaptureConfig, UserPrincipalConfig};

    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct AutoCaptureEnvGuard {
        _guard: MutexGuard<'static, ()>,
        saved: Option<std::ffi::OsString>,
    }

    impl AutoCaptureEnvGuard {
        fn clean() -> Self {
            let guard = TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = std::env::var_os("HEDDLE_AUTO_CAPTURE");
            unsafe { std::env::remove_var("HEDDLE_AUTO_CAPTURE") };
            Self {
                _guard: guard,
                saved,
            }
        }
    }

    impl Drop for AutoCaptureEnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(saved) = &self.saved {
                    std::env::set_var("HEDDLE_AUTO_CAPTURE", saved);
                } else {
                    std::env::remove_var("HEDDLE_AUTO_CAPTURE");
                }
            }
        }
    }

    fn quiet_cli() -> Cli {
        Cli::parse_from(["heddle", "--quiet", "status"])
    }

    fn user_config(auto: UserAutoCaptureMode) -> UserConfig {
        UserConfig {
            principal: Some(UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            capture: UserCaptureConfig { auto },
            ..UserConfig::default()
        }
    }

    #[test]
    fn command_boundary_auto_capture_snapshots_dirty_worktree() {
        let _env = AutoCaptureEnvGuard::clean();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = repo.root().join("note.txt");
        std::fs::write(&path, b"one\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        std::fs::write(&path, b"two\n").unwrap();

        let outcome = auto_capture_command_boundary(
            &quiet_cli(),
            &repo,
            &user_config(UserAutoCaptureMode::Command),
            AutoCaptureTrigger::Push,
        )
        .unwrap()
        .expect("dirty worktree should auto-capture");

        let head = repo.head().unwrap().expect("head after auto-capture");
        assert_eq!(outcome.change_id, head.short());
        assert_ne!(head, seed.change_id);
        let status_options = crate::cli::worktree_status_options(Some(repo.config()));
        assert!(!worktree_dirty(&repo, &status_options).unwrap());
    }

    #[test]
    fn command_boundary_auto_capture_is_noop_when_disabled() {
        let _env = AutoCaptureEnvGuard::clean();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = repo.root().join("note.txt");
        std::fs::write(&path, b"one\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        std::fs::write(&path, b"two\n").unwrap();

        let outcome = auto_capture_command_boundary(
            &quiet_cli(),
            &repo,
            &user_config(UserAutoCaptureMode::Off),
            AutoCaptureTrigger::Push,
        )
        .unwrap();

        assert!(outcome.is_none());
        assert_eq!(repo.head().unwrap(), Some(seed.change_id));
        let status_options = crate::cli::worktree_status_options(Some(repo.config()));
        assert!(worktree_dirty(&repo, &status_options).unwrap());
    }

    #[test]
    fn command_boundary_auto_capture_is_noop_when_worktree_clean() {
        let _env = AutoCaptureEnvGuard::clean();
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = repo.root().join("note.txt");
        std::fs::write(&path, b"one\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();

        let outcome = auto_capture_command_boundary(
            &quiet_cli(),
            &repo,
            &user_config(UserAutoCaptureMode::Command),
            AutoCaptureTrigger::Push,
        )
        .unwrap();

        assert!(outcome.is_none());
        assert_eq!(repo.head().unwrap(), Some(seed.change_id));
        let status_options = crate::cli::worktree_status_options(Some(repo.config()));
        assert!(!worktree_dirty(&repo, &status_options).unwrap());
    }
}
