// SPDX-License-Identifier: Apache-2.0
//! Local agent task assignment store.
//!
//! Stores one TOML file per task in `.heddle/agent-tasks/<task-id>.toml`.
//! These records are local operational provenance: they explain delegated
//! agent work without becoming source attribution or repository history.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    lock::RepoLock,
    store::{HeddleError, Result, atomic::write_file_atomic},
};

/// Current agent task TOML schema version.
pub const AGENT_TASK_SCHEMA_VERSION: u32 = 1;

/// Lifecycle status for a local agent task assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskStatus {
    /// The task is delegated but no active reservation is known.
    Open,
    /// An agent is actively working the task.
    InProgress,
    /// Work is blocked on external input.
    Blocked,
    /// Work completed successfully.
    Complete,
    /// Work was abandoned or superseded.
    Abandoned,
}

impl std::fmt::Display for AgentTaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Blocked => write!(f, "blocked"),
            Self::Complete => write!(f, "complete"),
            Self::Abandoned => write!(f, "abandoned"),
        }
    }
}

/// Local agent task assignment record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTaskRecord {
    /// Version of this TOML schema.
    pub schema_version: u32,
    /// Opaque stable task identifier.
    pub task_id: String,
    /// Human-readable task title.
    pub title: String,
    /// Detailed task body.
    pub body: String,
    /// Current task lifecycle status.
    pub status: AgentTaskStatus,
    /// Thread this task is delegated to.
    pub target_thread: String,
    /// Optional base state the task was delegated from.
    #[serde(default)]
    pub base_state: Option<String>,
    /// Optional base root the task was delegated from.
    #[serde(default)]
    pub base_root: Option<String>,
    /// Optional parent task.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    /// Optional coordination discussion id.
    #[serde(default)]
    pub coordination_discussion_id: Option<String>,
    /// Whether this task may continue without hosted connectivity.
    #[serde(default)]
    pub allow_offline: bool,
    /// Principal or agent that delegated the task.
    #[serde(default)]
    pub delegated_by: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
    /// Completion time for terminal statuses.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl AgentTaskRecord {
    /// Create a new local task assignment.
    pub fn new(task_id: String, title: String, target_thread: String) -> Self {
        let now = Utc::now();
        Self {
            schema_version: AGENT_TASK_SCHEMA_VERSION,
            task_id,
            title,
            body: String::new(),
            status: AgentTaskStatus::Open,
            target_thread,
            base_state: None,
            base_root: None,
            parent_task_id: None,
            coordination_discussion_id: None,
            allow_offline: false,
            delegated_by: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }
}

/// Manages local task assignment records stored in `.heddle/agent-tasks/`.
pub struct AgentTaskStore {
    tasks_dir: PathBuf,
}

impl AgentTaskStore {
    /// Create a task store backed by `<heddle_dir>/agent-tasks/`.
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            tasks_dir: heddle_dir.join("agent-tasks"),
        }
    }

    fn task_path(&self, task_id: &str) -> Result<PathBuf> {
        validate_task_id(task_id)?;
        Ok(self.tasks_dir.join(format!("{task_id}.toml")))
    }

    fn lock_path(&self) -> PathBuf {
        self.tasks_dir.join(".lock")
    }

    fn write_lock(&self) -> Result<crate::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path())
            .write()
            .map_err(|err| HeddleError::Config(format!("failed to acquire agent task lock: {err}")))
    }

    fn write_record_file(&self, record: &AgentTaskRecord) -> Result<()> {
        std::fs::create_dir_all(&self.tasks_dir)?;
        let path = self.task_path(&record.task_id)?;
        let content =
            toml::to_string_pretty(record).map_err(|err| HeddleError::Config(err.to_string()))?;
        Ok(write_file_atomic(&path, content.as_bytes())?)
    }

    fn load_record_from_path(
        &self,
        path: &Path,
        expected_task_id: &str,
    ) -> Result<Option<AgentTaskRecord>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let record: AgentTaskRecord =
            toml::from_str(&content).map_err(|err| HeddleError::Config(err.to_string()))?;
        if record.task_id != expected_task_id {
            return Err(HeddleError::Config(format!(
                "agent task file '{}' contains mismatched task_id '{}'",
                path.display(),
                record.task_id
            )));
        }
        Ok(Some(record))
    }

    /// Create and persist a task, generating a task id when absent.
    pub fn create(&self, mut record: AgentTaskRecord) -> Result<AgentTaskRecord> {
        let _lock = self.write_lock()?;
        std::fs::create_dir_all(&self.tasks_dir)?;
        if record.task_id.is_empty() {
            record.task_id = generate_agent_task_id();
        }
        record.schema_version = AGENT_TASK_SCHEMA_VERSION;
        validate_task_id(&record.task_id)?;
        let path = self.task_path(&record.task_id)?;
        if path.exists() {
            return Err(HeddleError::Config(format!(
                "agent task '{}' already exists",
                record.task_id
            )));
        }
        self.write_record_file(&record)?;
        Ok(record)
    }

    /// Load a task by id.
    pub fn load(&self, task_id: &str) -> Result<Option<AgentTaskRecord>> {
        let path = self.task_path(task_id)?;
        self.load_record_from_path(&path, task_id)
    }

    /// List all task records, most-recently-updated first.
    pub fn list(&self) -> Result<Vec<AgentTaskRecord>> {
        if !self.tasks_dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for dir_entry in std::fs::read_dir(&self.tasks_dir)? {
            let path = dir_entry?.path();
            if path.extension().map(|ext| ext == "toml").unwrap_or(false) {
                let Some(task_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                validate_task_id(task_id)?;
                if let Some(record) = self.load_record_from_path(&path, task_id)? {
                    records.push(record);
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
        Ok(records)
    }

    /// Mutate an existing task under the task-store write lock.
    pub fn update<F>(&self, task_id: &str, mut update: F) -> Result<Option<AgentTaskRecord>>
    where
        F: FnMut(&mut AgentTaskRecord),
    {
        let _lock = self.write_lock()?;
        let path = self.task_path(task_id)?;
        let Some(mut record) = self.load_record_from_path(&path, task_id)? else {
            return Ok(None);
        };
        update(&mut record);
        if record.task_id != task_id {
            return Err(HeddleError::Config(format!(
                "agent task update attempted to change task_id from '{}' to '{}'",
                task_id, record.task_id
            )));
        }
        record.schema_version = AGENT_TASK_SCHEMA_VERSION;
        record.updated_at = Utc::now();
        record.completed_at = match record.status {
            AgentTaskStatus::Complete | AgentTaskStatus::Abandoned => {
                record.completed_at.or(Some(record.updated_at))
            }
            AgentTaskStatus::Open | AgentTaskStatus::InProgress | AgentTaskStatus::Blocked => None,
        };
        self.write_record_file(&record)?;
        Ok(Some(record))
    }
}

/// Generate a local task assignment id.
pub fn generate_agent_task_id() -> String {
    format!("task-{}", uuid::Uuid::now_v7())
}

/// Validate a task id for direct use as a single TOML filename stem.
pub fn validate_task_id(task_id: &str) -> Result<()> {
    if task_id.is_empty()
        || !task_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(HeddleError::Config(format!(
            "invalid task ID '{task_id}': only lowercase alphanumeric and hyphens allowed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, AgentTaskStore) {
        let temp = TempDir::new().unwrap();
        let store = AgentTaskStore::new(&temp.path().join(".heddle"));
        (temp, store)
    }

    #[test]
    fn agent_task_create_loads_toml_record() {
        let (_temp, store) = store();
        let mut task = AgentTaskRecord::new(
            "task-demo".to_string(),
            "Demo task".to_string(),
            "main".into(),
        );
        task.body = "Do the thing".into();
        task.base_state = Some("hd-base".into());
        task.base_root = Some("root123".into());

        let created = store.create(task).unwrap();
        let loaded = store.load("task-demo").unwrap().unwrap();

        assert_eq!(created.schema_version, AGENT_TASK_SCHEMA_VERSION);
        assert_eq!(loaded.title, "Demo task");
        assert_eq!(loaded.body, "Do the thing");
        assert_eq!(loaded.target_thread, "main");
        assert_eq!(loaded.base_state.as_deref(), Some("hd-base"));
        assert_eq!(loaded.base_root.as_deref(), Some("root123"));
    }

    #[test]
    fn agent_task_update_sets_completion_time_for_terminal_status() {
        let (_temp, store) = store();
        store
            .create(AgentTaskRecord::new(
                "task-update".to_string(),
                "Update".to_string(),
                "main".into(),
            ))
            .unwrap();

        let updated = store
            .update("task-update", |task| {
                task.status = AgentTaskStatus::Complete;
            })
            .unwrap()
            .unwrap();

        assert_eq!(updated.status, AgentTaskStatus::Complete);
        assert!(updated.completed_at.is_some());
    }

    #[test]
    fn agent_task_rejects_path_traversal_ids() {
        let (_temp, store) = store();
        let err = store.load("../nope").unwrap_err();
        assert!(err.to_string().contains("invalid task ID"));
    }

    #[test]
    fn agent_task_rejects_mismatched_filename_and_record_id() {
        let (_temp, store) = store();
        std::fs::create_dir_all(&store.tasks_dir).unwrap();
        let record = AgentTaskRecord::new(
            "task-other".to_string(),
            "Tampered".to_string(),
            "main".into(),
        );
        let content = toml::to_string_pretty(&record).unwrap();
        std::fs::write(store.tasks_dir.join("task-requested.toml"), content).unwrap();

        let err = store.load("task-requested").unwrap_err();
        assert!(err.to_string().contains("mismatched task_id"));
    }

    #[test]
    fn agent_task_update_rejects_identity_mutation() {
        let (_temp, store) = store();
        store
            .create(AgentTaskRecord::new(
                "task-stable".to_string(),
                "Stable".to_string(),
                "main".into(),
            ))
            .unwrap();

        let err = store
            .update("task-stable", |task| {
                task.task_id = "task-other".to_string();
            })
            .unwrap_err();

        assert!(err.to_string().contains("attempted to change task_id"));
        assert!(store.load("task-stable").unwrap().is_some());
        assert!(store.load("task-other").unwrap().is_none());
    }
}
