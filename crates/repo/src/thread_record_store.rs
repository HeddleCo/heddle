// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

use objects::{
    fs_atomic::write_file_atomic,
    lock::{RepoLock, WriteLockGuard},
    store::{HeddleError, Result},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::thread_model::ThreadRecord;

pub trait ThreadRecordStore {
    fn load_record(&self, thread_id: &str) -> Result<Option<ThreadRecord>>;
    fn save_record(&self, record: &ThreadRecord) -> Result<()>;
    fn list_records(&self) -> Result<Vec<ThreadRecord>>;
    fn delete_record(&self, thread_id: &str) -> Result<()>;

    fn find_record_by_thread(&self, thread: &str) -> Result<Option<ThreadRecord>> {
        let mut records = self
            .list_records()?
            .into_iter()
            .filter(|record| record.thread == thread)
            .collect::<Vec<_>>();
        records.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(records.pop())
    }
}

#[derive(Debug, Clone)]
pub struct FilesystemThreadRecordStore {
    root: PathBuf,
    lock_name: String,
}

impl FilesystemThreadRecordStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock_name: ".lock".to_string(),
        }
    }

    pub fn with_lock_name(root: impl Into<PathBuf>, lock_name: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            lock_name: lock_name.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn record_path(&self, thread_id: &str) -> Result<PathBuf> {
        if thread_id.is_empty() {
            return Err(HeddleError::Config("thread id cannot be empty".to_string()));
        }
        Ok(self
            .root
            .join(format!("{}.toml", encode_thread_id(thread_id))))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join(&self.lock_name)
    }

    pub fn write_lock(&self) -> Result<WriteLockGuard> {
        RepoLock::at(self.lock_path())
            .write()
            .map_err(|err| HeddleError::Config(format!("failed to acquire thread lock: {err}")))
    }

    pub fn save_value<T: Serialize>(&self, thread_id: &str, value: &T) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.record_path(thread_id)?;
        let content =
            toml::to_string_pretty(value).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(write_file_atomic(&path, content.as_bytes())?)
    }

    pub fn load_value<T: DeserializeOwned>(&self, thread_id: &str) -> Result<Option<T>> {
        let path = self.record_path(thread_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let value = toml::from_str(&content).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(Some(value))
    }

    pub fn list_values<T: DeserializeOwned>(&self) -> Result<Vec<T>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut values = Vec::new();
        for dir_entry in std::fs::read_dir(&self.root)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if path.extension().map(|ext| ext == "toml").unwrap_or(false) {
                let content = std::fs::read_to_string(path)?;
                let value: T =
                    toml::from_str(&content).map_err(|e| HeddleError::Config(e.to_string()))?;
                values.push(value);
            }
        }
        Ok(values)
    }

    pub fn delete_value(&self, thread_id: &str) -> Result<()> {
        let path = self.record_path(thread_id)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

impl ThreadRecordStore for FilesystemThreadRecordStore {
    fn load_record(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        self.load_value(thread_id)
    }

    fn save_record(&self, record: &ThreadRecord) -> Result<()> {
        let _lock = self.write_lock()?;
        self.save_value(&record.id, record)
    }

    fn list_records(&self) -> Result<Vec<ThreadRecord>> {
        let mut records: Vec<ThreadRecord> = self.list_values()?;
        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
    }

    fn delete_record(&self, thread_id: &str) -> Result<()> {
        let _lock = self.write_lock()?;
        self.delete_value(thread_id)
    }
}

fn encode_thread_id(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len() * 2);
    for byte in thread_id.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        ThreadConfidenceSummary, ThreadFreshness, ThreadIntegrationPolicy, ThreadMode,
        ThreadRecord, ThreadState, ThreadVerificationSummary,
    };

    fn sample_record() -> ThreadRecord {
        ThreadRecord {
            id: "thread-1".to_string(),
            thread: "feature/thread-1".to_string(),
            target_thread: Some("main".to_string()),
            parent_thread: None,
            mode: ThreadMode::Materialized,
            state: ThreadState::Active,
            base_state: "abc123".to_string(),
            base_root: "def456".to_string(),
            current_state: Some("abc123".to_string()),
            merged_state: None,
            task: Some("implement thing".to_string()),
            changed_paths: vec!["src/lib.rs".to_string()],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: ThreadFreshness::Current,
            verification_summary: ThreadVerificationSummary::default(),
            confidence_summary: ThreadConfidenceSummary::default(),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    #[test]
    fn filesystem_thread_record_store_round_trips_record() {
        let temp = TempDir::new().unwrap();
        let store = FilesystemThreadRecordStore::new(temp.path());
        let record = sample_record();

        store.save_record(&record).unwrap();
        let loaded = store.load_record(&record.id).unwrap().unwrap();

        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.thread, record.thread);
        assert_eq!(loaded.base_state, record.base_state);
        assert_eq!(loaded.task, record.task);
    }
}