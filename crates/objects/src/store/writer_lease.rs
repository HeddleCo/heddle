// SPDX-License-Identifier: Apache-2.0
//! Exclusive writer leases for agent-controlled thread mutations.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    store::{HeddleError, Liveness, Result, reservation_liveness_at},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterLeaseStatus {
    Active,
    Complete,
    Abandoned,
}

impl std::fmt::Display for WriterLeaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Complete => write!(f, "complete"),
            Self::Abandoned => write!(f, "abandoned"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterLease {
    pub lease_id: String,
    pub thread: String,
    #[serde(default)]
    pub actor_session_id: Option<String>,
    #[serde(default)]
    pub task_assignment_id: Option<String>,
    #[serde(default)]
    pub anchor_state: Option<String>,
    #[serde(default)]
    pub anchor_root: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub token_hash: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub boot_id: Option<String>,
    pub heartbeat_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub status: WriterLeaseStatus,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl WriterLease {
    pub fn lease_expires_at(&self) -> DateTime<Utc> {
        self.heartbeat_at + crate::store::AGENT_LEASE_DURATION
    }

    pub fn liveness_at(&self, now: DateTime<Utc>) -> Liveness {
        if self.status != WriterLeaseStatus::Active {
            return Liveness::Dead;
        }
        reservation_liveness_at(
            self.pid,
            self.boot_id.as_deref(),
            Some(self.heartbeat_at),
            now,
        )
    }
}

#[derive(Debug, Clone)]
pub struct WriterLeaseDraft {
    pub thread: String,
    pub actor_session_id: Option<String>,
    pub task_assignment_id: Option<String>,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub path: Option<PathBuf>,
    pub pid: Option<u32>,
    pub boot_id: Option<String>,
}

#[derive(Debug)]
pub struct WriterLeaseGrant {
    pub lease: WriterLease,
    pub token: String,
}

#[derive(Debug)]
pub enum WriterLeaseReserveOutcome {
    Reserved(WriterLeaseGrant),
    LiveOwner(WriterLease),
}

#[derive(Debug)]
pub enum WriterLeaseAuthOutcome {
    Authorized(WriterLease),
    Missing,
    TokenMismatch,
    Inactive(WriterLease),
}

pub struct WriterLeaseStore {
    leases_dir: PathBuf,
}

impl WriterLeaseStore {
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            leases_dir: heddle_dir.join("writer-leases"),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.leases_dir.join(".lock")
    }

    fn write_lock(&self) -> Result<crate::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path()).write().map_err(|err| {
            HeddleError::Config(format!("failed to acquire writer lease lock: {err}"))
        })
    }

    fn lease_path(&self, lease_id: &str) -> Result<PathBuf> {
        validate_lease_id(lease_id)?;
        Ok(self.leases_dir.join(format!("{lease_id}.toml")))
    }

    fn load_path(&self, path: &Path) -> Result<Option<WriterLease>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content)
            .map(Some)
            .map_err(|err| HeddleError::Config(err.to_string()))
    }

    fn write_lease(&self, lease: &WriterLease) -> Result<()> {
        crate::fs_atomic::create_dir_all_durable(&self.leases_dir)?;
        let content =
            toml::to_string_pretty(lease).map_err(|err| HeddleError::Config(err.to_string()))?;
        Ok(write_file_atomic(
            &self.lease_path(&lease.lease_id)?,
            content.as_bytes(),
        )?)
    }

    fn list_locked(&self) -> Result<Vec<WriterLease>> {
        if !self.leases_dir.exists() {
            return Ok(Vec::new());
        }
        let mut leases = Vec::new();
        for entry in std::fs::read_dir(&self.leases_dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
                && let Some(lease) = self.load_path(&path)?
            {
                leases.push(lease);
            }
        }
        leases.sort_by_key(|lease| std::cmp::Reverse(lease.started_at));
        Ok(leases)
    }

    fn reap_expired_locked(&self, now: DateTime<Utc>) -> Result<()> {
        for mut lease in self.list_locked()? {
            if lease.liveness_at(now) == Liveness::Dead && lease.status == WriterLeaseStatus::Active
            {
                lease.status = WriterLeaseStatus::Abandoned;
                lease.completed_at = Some(now);
                self.write_lease(&lease)?;
            }
        }
        Ok(())
    }

    pub fn reserve(
        &self,
        draft: WriterLeaseDraft,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseReserveOutcome> {
        let _lock = self.write_lock()?;
        self.reap_expired_locked(now)?;
        if let Some(owner) = self
            .list_locked()?
            .into_iter()
            .find(|lease| lease.thread == draft.thread && lease.status == WriterLeaseStatus::Active)
        {
            return Ok(WriterLeaseReserveOutcome::LiveOwner(owner));
        }

        let lease_id = generate_writer_lease_id();
        let token = generate_writer_lease_token();
        let lease = WriterLease {
            lease_id,
            thread: draft.thread,
            actor_session_id: draft.actor_session_id,
            task_assignment_id: draft.task_assignment_id,
            anchor_state: draft.anchor_state,
            anchor_root: draft.anchor_root,
            path: draft.path,
            token_hash: token_hash(&token),
            pid: draft.pid,
            boot_id: draft.boot_id,
            heartbeat_at: now,
            started_at: now,
            status: WriterLeaseStatus::Active,
            completed_at: None,
        };
        self.write_lease(&lease)?;
        Ok(WriterLeaseReserveOutcome::Reserved(WriterLeaseGrant {
            lease,
            token,
        }))
    }

    pub fn authenticate_and_renew(
        &self,
        lease_id: &str,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseAuthOutcome> {
        let _lock = self.write_lock()?;
        let path = self.lease_path(lease_id)?;
        let Some(mut lease) = self.load_path(&path)? else {
            return Ok(WriterLeaseAuthOutcome::Missing);
        };
        if lease.status != WriterLeaseStatus::Active || lease.liveness_at(now) == Liveness::Dead {
            if lease.status == WriterLeaseStatus::Active {
                lease.status = WriterLeaseStatus::Abandoned;
                lease.completed_at = Some(now);
                self.write_lease(&lease)?;
            }
            return Ok(WriterLeaseAuthOutcome::Inactive(lease));
        }
        if token_hash(token) != lease.token_hash {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        lease.heartbeat_at = now;
        self.write_lease(&lease)?;
        Ok(WriterLeaseAuthOutcome::Authorized(lease))
    }

    pub fn release(
        &self,
        lease_id: &str,
        token: &str,
        status: WriterLeaseStatus,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseAuthOutcome> {
        let _lock = self.write_lock()?;
        let path = self.lease_path(lease_id)?;
        let Some(mut lease) = self.load_path(&path)? else {
            return Ok(WriterLeaseAuthOutcome::Missing);
        };
        if lease.status != WriterLeaseStatus::Active {
            return Ok(WriterLeaseAuthOutcome::Inactive(lease));
        }
        if token_hash(token) != lease.token_hash {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        lease.status = status;
        lease.completed_at = Some(now);
        self.write_lease(&lease)?;
        Ok(WriterLeaseAuthOutcome::Authorized(lease))
    }

    pub fn list(&self) -> Result<Vec<WriterLease>> {
        let _lock = self.write_lock()?;
        self.reap_expired_locked(Utc::now())?;
        self.list_locked()
    }

    pub fn abandon_thread(&self, thread: &str, now: DateTime<Utc>) -> Result<()> {
        let _lock = self.write_lock()?;
        for mut lease in self.list_locked()? {
            if lease.thread == thread && lease.status == WriterLeaseStatus::Active {
                lease.status = WriterLeaseStatus::Abandoned;
                lease.completed_at = Some(now);
                self.write_lease(&lease)?;
            }
        }
        Ok(())
    }

    pub fn load(&self, lease_id: &str) -> Result<Option<WriterLease>> {
        self.load_path(&self.lease_path(lease_id)?)
    }
}

pub fn generate_writer_lease_id() -> String {
    format!("lease-{}", random_base32())
}

pub fn generate_writer_lease_token() -> String {
    format!("hwl_{}", random_base32())
}

fn random_base32() -> String {
    let random_bytes: [u8; 24] = rand::random();
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &random_bytes).to_lowercase()
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn validate_lease_id(lease_id: &str) -> Result<()> {
    if lease_id.starts_with("lease-")
        && lease_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Ok(());
    }
    Err(HeddleError::Config(format!(
        "invalid writer lease id '{lease_id}'"
    )))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn draft(thread: &str) -> WriterLeaseDraft {
        WriterLeaseDraft {
            thread: thread.to_string(),
            actor_session_id: Some("agent-one".to_string()),
            task_assignment_id: None,
            anchor_state: Some("hd-state".to_string()),
            anchor_root: Some("root".to_string()),
            path: None,
            pid: None,
            boot_id: None,
        }
    }

    #[test]
    fn token_is_required_to_renew_or_release() {
        let temp = TempDir::new().unwrap();
        let store = WriterLeaseStore::new(temp.path());
        let now = Utc::now();
        let WriterLeaseReserveOutcome::Reserved(grant) =
            store.reserve(draft("feature/a"), now).unwrap()
        else {
            panic!("first lease should reserve");
        };
        assert!(matches!(
            store
                .authenticate_and_renew(&grant.lease.lease_id, "wrong", now)
                .unwrap(),
            WriterLeaseAuthOutcome::TokenMismatch
        ));
        assert!(matches!(
            store
                .authenticate_and_renew(&grant.lease.lease_id, &grant.token, now)
                .unwrap(),
            WriterLeaseAuthOutcome::Authorized(_)
        ));
    }

    #[test]
    fn expired_lease_does_not_block_a_new_owner() {
        let temp = TempDir::new().unwrap();
        let store = WriterLeaseStore::new(temp.path());
        let now = Utc::now();
        let WriterLeaseReserveOutcome::Reserved(first) =
            store.reserve(draft("feature/a"), now).unwrap()
        else {
            panic!("first lease should reserve");
        };
        let later = now + crate::store::AGENT_LEASE_DURATION + chrono::Duration::seconds(1);
        let WriterLeaseReserveOutcome::Reserved(second) =
            store.reserve(draft("feature/a"), later).unwrap()
        else {
            panic!("expired lease should not block");
        };
        assert_ne!(first.lease.lease_id, second.lease.lease_id);
    }

    #[test]
    fn stored_lease_does_not_contain_bearer_token() {
        let temp = TempDir::new().unwrap();
        let store = WriterLeaseStore::new(temp.path());
        let WriterLeaseReserveOutcome::Reserved(grant) =
            store.reserve(draft("feature/a"), Utc::now()).unwrap()
        else {
            panic!("first lease should reserve");
        };
        let persisted = std::fs::read_to_string(
            temp.path()
                .join("writer-leases")
                .join(format!("{}.toml", grant.lease.lease_id)),
        )
        .unwrap();
        assert!(!persisted.contains(&grant.token));
        assert!(persisted.contains("token_hash"));
    }
}
