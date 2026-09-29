// SPDX-License-Identifier: Apache-2.0
//! Exclusive writers for checkouts; separate checkouts may share a Thread.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    object::ContentHash,
    store::{HeddleError, Liveness, Result, reservation_liveness_at},
};

/// The physical checkout lock shared by capture and lease handoff.
pub fn checkout_writer_lock(heddle_dir: &Path, root: &Path) -> Result<RepoLock> {
    Ok(checkout_writer_lock_for_path(
        heddle_dir,
        &root.canonicalize()?,
    ))
}

fn checkout_writer_lock_for_path(heddle_dir: &Path, path: &Path) -> RepoLock {
    let path_key = ContentHash::compute_typed(
        "checkout-writer-path-v2",
        path.as_os_str().as_encoded_bytes(),
    );
    RepoLock::at(
        heddle_dir
            .join("locks")
            .join(format!("checkout-{}.lock", path_key.to_hex())),
    )
}

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
    #[serde(default)]
    pub pid_birth: Option<String>,
    #[serde(default)]
    pub pid_namespace: Option<String>,
    #[serde(default)]
    pub harness_session_id: Option<String>,
    pub heartbeat_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub status: WriterLeaseStatus,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl WriterLease {
    fn conflicts_with(&self, thread: &str, path: Option<&Path>) -> bool {
        self.status == WriterLeaseStatus::Active
            && match (self.path.as_deref(), path) {
                (Some(owned), Some(requested)) => owned == requested,
                // An unmaterialized reservation has no narrower checkout scope.
                _ => self.thread == thread,
            }
    }

    pub fn lease_expires_at(&self) -> DateTime<Utc> {
        self.heartbeat_at + crate::store::AGENT_LEASE_DURATION
    }

    pub fn liveness_at(&self, now: DateTime<Utc>) -> Liveness {
        self.liveness_at_in_namespace(now, current_pid_namespace().as_deref())
    }

    fn liveness_at_in_namespace(
        &self,
        now: DateTime<Utc>,
        observed_namespace: Option<&str>,
    ) -> Liveness {
        if self.status != WriterLeaseStatus::Active {
            return Liveness::Dead;
        }
        // A PID in another namespace identifies a different process here.
        // Without comparable namespace identities, only the heartbeat can expire it.
        let same_namespace = if cfg!(target_os = "linux") {
            self.pid_namespace
                .as_deref()
                .is_some_and(|recorded| Some(recorded) == observed_namespace)
        } else {
            true
        };
        if !same_namespace {
            return reservation_liveness_at(None, None, Some(self.heartbeat_at), now);
        }
        if let (Some(pid), Some(birth)) = (self.pid, self.pid_birth.as_deref())
            && super::process_birth(pid).as_deref() != Some(birth)
        {
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

#[cfg(target_os = "linux")]
fn current_pid_namespace() -> Option<String> {
    std::fs::read_link("/proc/self/ns/pid")
        .ok()?
        .to_str()
        .map(str::to_owned)
}

#[cfg(not(target_os = "linux"))]
fn current_pid_namespace() -> Option<String> {
    None
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

    fn checkout_lock(&self, path: &Path) -> Result<RepoLock> {
        let heddle_dir = self
            .leases_dir
            .parent()
            .ok_or_else(|| HeddleError::Config("writer lease directory has no parent".into()))?;
        Ok(checkout_writer_lock_for_path(heddle_dir, path))
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

    fn reap_expired_locked(&self, now: DateTime<Utc>, held_checkout: Option<&Path>) -> Result<()> {
        for mut lease in self.list_locked()? {
            if lease.liveness_at(now) == Liveness::Dead && lease.status == WriterLeaseStatus::Active
            {
                // Never retire authority while a capture holds this checkout.
                // try_write avoids reversing the checkout -> lease-store lock
                // order used by capture and handoff.
                let checkout_guard = if let Some(path) = lease.path.as_deref() {
                    if held_checkout == Some(path) {
                        None
                    } else {
                        let lock = self.checkout_lock(path)?;
                        let Some(guard) = lock.try_write().map_err(|error| {
                            HeddleError::Config(format!(
                                "failed to acquire checkout writer lock: {error}"
                            ))
                        })?
                        else {
                            continue;
                        };
                        Some(guard)
                    }
                } else {
                    None
                };
                lease.status = WriterLeaseStatus::Abandoned;
                lease.completed_at = Some(now);
                self.write_lease(&lease)?;
                drop(checkout_guard);
            }
        }
        Ok(())
    }

    pub fn reserve(
        &self,
        draft: WriterLeaseDraft,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseReserveOutcome> {
        self.reserve_prepared(
            draft,
            generate_writer_lease_id(),
            generate_writer_lease_token(),
            now,
        )
    }

    /// The caller already holds the checkout mutation lock for `draft.path`.
    pub fn reserve_with_checkout_lock(
        &self,
        draft: WriterLeaseDraft,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseReserveOutcome> {
        self.reserve_prepared_inner(
            draft,
            generate_writer_lease_id(),
            generate_writer_lease_token(),
            now,
            true,
        )
    }

    /// A command journal persists these random credentials before reservation.
    /// Retrying the exact live reservation recovers its token; a different actor,
    /// path, token or an expired/released reservation can never revive it.
    pub fn reserve_prepared(
        &self,
        draft: WriterLeaseDraft,
        lease_id: String,
        token: String,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseReserveOutcome> {
        self.reserve_prepared_inner(draft, lease_id, token, now, false)
    }

    fn reserve_prepared_inner(
        &self,
        mut draft: WriterLeaseDraft,
        lease_id: String,
        token: String,
        now: DateTime<Utc>,
        checkout_lock_held: bool,
    ) -> Result<WriterLeaseReserveOutcome> {
        validate_lease_id(&lease_id)?;
        if token.len() < 32 || token.len() > 256 {
            return Err(HeddleError::Config("invalid prepared writer token".into()));
        }
        draft.path = draft.path.map(std::fs::canonicalize).transpose()?;
        let _checkout_guard = if !checkout_lock_held {
            draft
                .path
                .as_deref()
                .map(|path| {
                    self.checkout_lock(path)?.write().map_err(|error| {
                        HeddleError::Config(format!(
                            "failed to acquire checkout writer lock: {error}"
                        ))
                    })
                })
                .transpose()?
        } else {
            None
        };
        let _lock = self.write_lock()?;
        self.reap_expired_locked(now, draft.path.as_deref())?;
        if let Some(old) = self.load_path(&self.lease_path(&lease_id)?)? {
            if old.status == WriterLeaseStatus::Active
                && old.liveness_at(now) != Liveness::Dead
                && old.thread == draft.thread
                && old.path == draft.path
                && old.actor_session_id == draft.actor_session_id
                && old.token_hash == token_hash(&token)
            {
                return Ok(WriterLeaseReserveOutcome::Reserved(WriterLeaseGrant {
                    lease: old,
                    token,
                }));
            }
            return Err(HeddleError::Config(
                "prepared writer reservation changed or expired".into(),
            ));
        }
        if let Some(owner) = self
            .list_locked()?
            .into_iter()
            .find(|lease| lease.conflicts_with(&draft.thread, draft.path.as_deref()))
        {
            return Ok(WriterLeaseReserveOutcome::LiveOwner(owner));
        }
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
            pid_birth: None,
            pid_namespace: draft.pid.and_then(|_| current_pid_namespace()),
            harness_session_id: None,
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

    /// Advisory preflight. `reserve` repeats this check under the write lock.
    pub fn live_owner(&self, thread: &str, path: Option<&Path>) -> Result<Option<WriterLease>> {
        self.live_owner_inner(thread, path, None)
    }

    /// Advisory preflight when the caller holds this checkout's mutation lock.
    pub fn live_owner_with_checkout_lock(
        &self,
        thread: &str,
        path: &Path,
        _checkout_guard: &crate::lock::WriteLockGuard,
    ) -> Result<Option<WriterLease>> {
        self.live_owner_inner(thread, Some(path), Some(path))
    }

    fn live_owner_inner(
        &self,
        thread: &str,
        path: Option<&Path>,
        held_checkout: Option<&Path>,
    ) -> Result<Option<WriterLease>> {
        let path = path.map(std::fs::canonicalize).transpose()?;
        let held_checkout = held_checkout.map(std::fs::canonicalize).transpose()?;
        let _lock = self.write_lock()?;
        self.reap_expired_locked(Utc::now(), held_checkout.as_deref())?;
        Ok(self
            .list_locked()?
            .into_iter()
            .find(|lease| lease.conflicts_with(thread, path.as_deref())))
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
            return Ok(WriterLeaseAuthOutcome::Inactive(lease));
        }
        if token_hash(token) != lease.token_hash {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        lease.heartbeat_at = now;
        self.write_lease(&lease)?;
        Ok(WriterLeaseAuthOutcome::Authorized(lease))
    }

    /// A hook proves both possession of the lane credential and its process
    /// ancestry. Subsequent hook calls may only renew the same live session.
    pub fn bind_hook_session(
        &self,
        lease_id: &str,
        token: &str,
        session: &str,
        pid: u32,
        birth: &str,
        now: DateTime<Utc>,
    ) -> Result<WriterLeaseAuthOutcome> {
        let _lock = self.write_lock()?;
        let path = self.lease_path(lease_id)?;
        let Some(mut lease) = self.load_path(&path)? else {
            return Ok(WriterLeaseAuthOutcome::Missing);
        };
        if lease.status != WriterLeaseStatus::Active || lease.liveness_at(now) == Liveness::Dead {
            return Ok(WriterLeaseAuthOutcome::Inactive(lease));
        }
        if token_hash(token) != lease.token_hash {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        if let Some(existing) = lease.harness_session_id.as_deref()
            && existing != session
        {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        if let Some(existing) = lease.pid_birth.as_deref()
            && (lease.pid != Some(pid) || existing != birth)
        {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        lease.harness_session_id = Some(session.to_owned());
        lease.pid = Some(pid);
        lease.pid_birth = Some(birth.to_owned());
        lease.pid_namespace = current_pid_namespace();
        lease.boot_id = super::current_boot_id();
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
        let current = self.load(lease_id)?;
        let _checkout_guard = current
            .as_ref()
            .and_then(|lease| lease.path.as_deref())
            .map(|path| {
                self.checkout_lock(path)?.write().map_err(|error| {
                    HeddleError::Config(format!("failed to acquire checkout writer lock: {error}"))
                })
            })
            .transpose()?;
        self.release_with_checkout_lock(lease_id, token, status, now)
    }

    /// The caller holds the checkout mutation lock through credential cleanup.
    pub fn release_with_checkout_lock(
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
        if token_hash(token) != lease.token_hash {
            return Ok(WriterLeaseAuthOutcome::TokenMismatch);
        }
        if lease.status != WriterLeaseStatus::Active {
            return Ok(WriterLeaseAuthOutcome::Inactive(lease));
        }
        lease.status = status;
        lease.completed_at = Some(now);
        self.write_lease(&lease)?;
        Ok(WriterLeaseAuthOutcome::Authorized(lease))
    }

    pub fn list(&self) -> Result<Vec<WriterLease>> {
        let _lock = self.write_lock()?;
        self.reap_expired_locked(Utc::now(), None)?;
        self.list_locked()
    }

    /// List persisted leases without reaping expired active records.
    ///
    /// Cleanup previews use this read-only view so residue detection cannot
    /// mutate lease storage as a side effect.
    pub fn list_without_reaping(&self) -> Result<Vec<WriterLease>> {
        self.list_locked()
    }

    /// Remove reservations belonging to task assignments rolled back before launch.
    pub fn delete_for_tasks(&self, task_ids: &[String]) -> Result<()> {
        let _lock = self.write_lock()?;
        for lease in self.list_locked()? {
            if lease
                .task_assignment_id
                .as_ref()
                .is_some_and(|task_id| task_ids.contains(task_id))
            {
                let path = self.lease_path(&lease.lease_id)?;
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    pub fn abandon_thread(&self, thread: &str, now: DateTime<Utc>) -> Result<()> {
        let _lock = self.write_lock()?;
        let mut checkout_guards = Vec::new();
        for lease in self
            .list_locked()?
            .into_iter()
            .filter(|lease| lease.thread == thread && lease.status == WriterLeaseStatus::Active)
        {
            if let Some(path) = lease.path.as_deref() {
                let lock = self.checkout_lock(path)?;
                let guard = lock
                    .try_write()
                    .map_err(|error| {
                        HeddleError::Config(format!(
                            "failed to acquire checkout writer lock: {error}"
                        ))
                    })?
                    .ok_or_else(|| HeddleError::Config("checkout has an active mutation".into()))?;
                checkout_guards.push(guard);
            }
        }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn recycled_pid_does_not_keep_bound_lease_alive() {
        let now = Utc::now();
        let temp = TempDir::new().expect("lease store");
        let store = WriterLeaseStore::new(temp.path());
        let grant = match store.reserve(draft("lane"), now).expect("reserve") {
            WriterLeaseReserveOutcome::Reserved(grant) => grant,
            WriterLeaseReserveOutcome::LiveOwner(_) => panic!("new lane has an owner"),
        };
        let outcome = store
            .bind_hook_session(
                &grant.lease.lease_id,
                &grant.token,
                "codex:session-a",
                std::process::id(),
                "wrong-birth-tick",
                now,
            )
            .expect("bind");
        assert!(matches!(outcome, WriterLeaseAuthOutcome::Authorized(_)));
        assert_eq!(
            store
                .load(&grant.lease.lease_id)
                .expect("load")
                .expect("lease")
                .liveness_at(now),
            Liveness::Dead,
        );
        assert!(matches!(
            store.reserve(draft("lane"), now).expect("reacquire"),
            WriterLeaseReserveOutcome::Reserved(_)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreign_pid_namespace_cannot_reap_fresh_bound_lease() {
        let now = Utc::now();
        let temp = TempDir::new().expect("lease store");
        let store = WriterLeaseStore::new(temp.path());
        let grant = match store.reserve(draft("lane"), now).expect("reserve") {
            WriterLeaseReserveOutcome::Reserved(grant) => grant,
            WriterLeaseReserveOutcome::LiveOwner(_) => panic!("new lane has an owner"),
        };
        let outcome = store
            .bind_hook_session(
                &grant.lease.lease_id,
                &grant.token,
                "opencode:session-a",
                std::process::id(),
                "wrong-birth-tick",
                now,
            )
            .expect("bind");
        let WriterLeaseAuthOutcome::Authorized(mut lease) = outcome else {
            panic!("hook should bind");
        };
        assert_eq!(lease.pid_namespace, current_pid_namespace());
        assert_eq!(
            lease.liveness_at_in_namespace(now, lease.pid_namespace.as_deref()),
            Liveness::Dead,
            "a recycled PID in the same namespace must be rejected"
        );
        lease.pid_namespace = None;
        assert_eq!(lease.liveness_at(now), Liveness::Alive);
        lease.pid_namespace = Some("pid:[foreign]".to_string());
        store
            .write_lease(&lease)
            .expect("simulate foreign namespace");
        assert_eq!(lease.liveness_at(now), Liveness::Alive);
        assert!(matches!(
            store.reserve(draft("lane"), now).expect("competing writer"),
            WriterLeaseReserveOutcome::LiveOwner(_)
        ));
        assert!(matches!(
            store
                .reserve(draft("lane"), now + chrono::Duration::minutes(6))
                .expect("expired writer"),
            WriterLeaseReserveOutcome::Reserved(_)
        ));
    }

    #[test]
    fn separate_checkouts_of_one_thread_have_independent_writers() {
        let temp = TempDir::new().expect("lease directory");
        let store = WriterLeaseStore::new(temp.path());
        for name in ["agent-a", "agent-b"] {
            let path = temp.path().join(name);
            std::fs::create_dir(&path).expect("checkout directory");
            let mut request = draft("shared-thread");
            request.path = Some(path);
            assert!(
                matches!(
                    store
                        .reserve(request, Utc::now())
                        .expect("reserve checkout"),
                    WriterLeaseReserveOutcome::Reserved(_)
                ),
                "different checkouts must not contend on their Thread"
            );
        }
        assert_eq!(store.list().expect("leases").len(), 2);
    }

    #[test]
    fn one_checkout_cannot_have_two_writers_even_under_different_thread_names() {
        let temp = TempDir::new().expect("lease directory");
        let store = WriterLeaseStore::new(temp.path());
        let path = temp.path().join("checkout");
        std::fs::create_dir(&path).expect("checkout directory");
        let mut first = draft("thread-a");
        first.path = Some(path.clone());
        let mut second = draft("thread-b");
        second.path = Some(path.join("."));
        assert!(matches!(
            store.reserve(first, Utc::now()).expect("first writer"),
            WriterLeaseReserveOutcome::Reserved(_)
        ));
        assert!(matches!(
            store.reserve(second, Utc::now()).expect("competing writer"),
            WriterLeaseReserveOutcome::LiveOwner(_)
        ));
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
