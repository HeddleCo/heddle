//! A physical checkout has one mutation lock and one shared writer lease.
use std::path::Path;

use chrono::Utc;
use objects::{
    HeddleError, RecoveryDetails,
    fs_atomic::write_file_atomic_secret,
    lock::WriteLockGuard,
    object::ContentHash,
    store::{
        WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseGrant, WriterLeaseReserveOutcome,
        WriterLeaseStatus, WriterLeaseStore, checkout_writer_lock, current_boot_id, process_birth,
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    Repository,
    thread_replication::{Error, Result},
};

const WRITER_CREDENTIAL_FILE: &str = "writer-credential.json";

#[derive(Deserialize, Serialize)]
struct WriterCredential {
    lease: String,
    token: String,
}

/// Store the checkout's existing writer authority without exposing the bearer
/// in process output. The checkout metadata directory is already local to it.
pub fn write_checkout_writer_credential(root: &Path, lease: &str, token: &str) -> Result<()> {
    let credential = WriterCredential {
        lease: lease.to_owned(),
        token: token.to_owned(),
    };
    let bytes =
        serde_json::to_vec(&credential).map_err(|error| Error::Invalid(error.to_string()))?;
    write_file_atomic_secret(&root.join(".heddle").join(WRITER_CREDENTIAL_FILE), &bytes)?;
    Ok(())
}

pub fn remove_checkout_writer_credential(root: &Path, lease: &str) -> Result<()> {
    let path = root.join(".heddle").join(WRITER_CREDENTIAL_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let credential: WriterCredential = serde_json::from_slice(&bytes)
        .map_err(|error| credential_error(format!("invalid lane credential: {error}")))?;
    if credential.lease == lease {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn credential_error(error: impl Into<String>) -> Error {
    Error::Object(HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            "writer_lease_credential_invalid",
            error,
            "Use the active lane credential, or ask its holder to release the lease.",
            "the checkout's writer credential is missing, invalid, or outside its lease scope",
            "mutation without the lane's active writer authority would bypass checkout ownership",
            "the mutation was refused before saving state",
        )
        .with_recovery_commands(vec!["heddle agent list".to_string()]),
    ))
}

fn foreign_writer_error(lease: &str) -> Error {
    Error::Object(HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            "writer_lease_owned_by_other",
            format!("checkout is reserved by writer {lease}"),
            "Use the active lane credential, or ask its holder to release the lease.",
            format!("writer lease {lease} owns this checkout"),
            "a second writer would mutate the checkout without its owner's authority",
            "the mutation was refused before saving state",
        )
        .with_recovery_commands(vec!["heddle agent list".to_string()]),
    ))
}

#[cfg(target_os = "linux")]
fn process_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn mac_process_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = i32::try_from(pid).ok()?;
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (read == size).then(|| unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
fn process_parent(pid: u32) -> Option<u32> {
    Some(mac_process_info(pid)?.pbi_ppid)
}

#[cfg(target_os = "linux")]
fn shell_process(pid: u32) -> bool {
    let shell = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .is_some_and(|name| matches!(name.trim(), "sh" | "bash" | "dash" | "zsh" | "fish"));
    shell
        && std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .is_some_and(|args| {
                args.windows(b"integration ".len())
                    .any(|part| part == b"integration ")
            })
}

#[cfg(target_os = "macos")]
fn shell_process(pid: u32) -> bool {
    let Some(info) = mac_process_info(pid) else {
        return false;
    };
    let name: Vec<u8> = info
        .pbi_comm
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    matches!(
        name.as_slice(),
        b"sh" | b"bash" | b"dash" | b"zsh" | b"fish"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hook_owner() -> Result<(u32, String)> {
    let parent = process_parent(std::process::id())
        .ok_or_else(|| credential_error("cannot identify the harness process"))?;
    let pid = if shell_process(parent) {
        process_parent(parent).unwrap_or(parent)
    } else {
        parent
    };
    let birth = process_birth(pid)
        .ok_or_else(|| credential_error("cannot identify the harness process birth"))?;
    Ok((pid, birth))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn hook_owner() -> Result<(u32, String)> {
    Err(credential_error(
        "session-bound lane writers require process ancestry support",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn belongs_to_harness(pid: u32, birth: &str) -> bool {
    belongs_to_harness_with(
        pid,
        birth,
        std::process::id(),
        process_parent,
        process_birth,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn belongs_to_harness_with(
    pid: u32,
    birth: &str,
    mut current: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    birth_of: impl Fn(u32) -> Option<String>,
) -> bool {
    for _ in 0..64 {
        if current == pid {
            return birth_of(current).as_deref() == Some(birth);
        }
        let Some(parent) = parent_of(current) else {
            return false;
        };
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn belongs_to_harness(_pid: u32, _birth: &str) -> bool {
    false
}

fn require_hook_owner(lease: &objects::store::WriterLease) -> Result<()> {
    match (lease.pid, lease.pid_birth.as_deref()) {
        (Some(pid), Some(birth)) if belongs_to_harness(pid, birth) => Ok(()),
        _ => Err(credential_error(
            "lane credential belongs to another harness session",
        )),
    }
}

fn lease_needs_harness(lease: &objects::store::WriterLease) -> bool {
    lease.task_assignment_id.is_some() || lease.harness_session_id.is_some()
}

/// Keep lease handoff and credential cleanup outside an active checkout mutation.
pub fn lock_checkout_writer_handoff(heddle_dir: &Path, root: &Path) -> Result<WriteLockGuard> {
    checkout_writer_lock(heddle_dir, root)?
        .write()
        .map_err(|error| Error::Invalid(error.to_string()))
}

/// Keep this guard on the acquiring thread until all checkout mutations finish.
/// A temporary CLI lease is released on drop; an authenticated persistent agent
/// lease remains owned by that agent between commands.
pub struct CheckoutWriterGuard {
    store: WriterLeaseStore,
    temporary: Option<WriterLeaseGrant>,
    _mutation_lock: WriteLockGuard,
}
impl CheckoutWriterGuard {
    /// Release a temporary lease explicitly so persistence failures reach callers.
    pub fn finish(mut self) -> Result<()> {
        self.release()
    }
    fn release(&mut self) -> Result<()> {
        if let Some(grant) = self.temporary.as_ref() {
            match self.store.release_with_checkout_lock(
                &grant.lease.lease_id,
                &grant.token,
                WriterLeaseStatus::Complete,
                Utc::now(),
            )? {
                WriterLeaseAuthOutcome::Authorized(_) => {
                    self.temporary = None;
                }
                _ => {
                    return Err(Error::Invalid(
                        "checkout writer lease changed during mutation".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
impl Drop for CheckoutWriterGuard {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::warn!(%error, "failed to release checkout writer lease");
        }
    }
}
impl Repository {
    fn is_managed_lane(&self, thread: &str) -> bool {
        matches!(
            (
                self.managed_checkout_path(thread).canonicalize(),
                self.root().canonicalize(),
            ),
            (Ok(managed), Ok(root)) if managed == root
        )
    }

    fn requires_hook_session(&self, store: &WriterLeaseStore, thread: &str) -> Result<bool> {
        if !self.is_managed_lane(thread) {
            return Ok(false);
        }
        let root = self.root().canonicalize()?;
        Ok(store.list_without_reaping()?.iter().any(|lease| {
            lease.thread == thread
                && lease.path.as_deref() == Some(root.as_path())
                && lease_needs_harness(lease)
        }))
    }

    /// Called by installed hooks. A hook receives the session ID on stdin;
    /// the kernel ancestry supplies the proof that commands run in its process
    /// tree. A new session can claim a lane only after the old lease is dead.
    pub fn hook_checkout_writer(
        &self,
        harness: &str,
        session: Option<&str>,
        release: bool,
    ) -> Result<()> {
        let Some(thread) = self.current_lane()? else {
            return Ok(());
        };
        let store = WriterLeaseStore::new(self.heddle_dir());
        if !self.requires_hook_session(&store, &thread)? {
            return Ok(());
        }
        let (pid, birth) = hook_owner()?;
        let session = session
            .filter(|id| !id.is_empty())
            .map(|id| format!("{harness}:{id}"))
            .unwrap_or_else(|| format!("{harness}:pid:{pid}:{birth}"));
        let _guard = lock_checkout_writer_handoff(self.heddle_dir(), self.root())?;
        let owner = store.live_owner_with_checkout_lock(&thread, self.root(), &_guard)?;
        let credential = self.read_checkout_writer_credential()?;
        if let Some(owner) = owner {
            let Some(credential) = credential else {
                return Err(credential_error("live lane credential is missing"));
            };
            if owner.lease_id != credential.lease {
                return Err(credential_error(
                    "lane credential does not match its live owner",
                ));
            }
            let bound = store.bind_hook_session(
                &credential.lease,
                &credential.token,
                &session,
                pid,
                &birth,
                Utc::now(),
            )?;
            if !matches!(bound, WriterLeaseAuthOutcome::Authorized(_)) {
                return Err(credential_error("lane belongs to another harness session"));
            }
            if release {
                let released = store.release_with_checkout_lock(
                    &credential.lease,
                    &credential.token,
                    WriterLeaseStatus::Complete,
                    Utc::now(),
                )?;
                if !matches!(released, WriterLeaseAuthOutcome::Authorized(_)) {
                    return Err(credential_error("lane release lost its writer lease"));
                }
                remove_checkout_writer_credential(self.root(), &credential.lease)?;
            }
            return Ok(());
        }
        if release {
            return Ok(());
        }
        let root = self.root().canonicalize()?;
        let previous = store.list_without_reaping()?.into_iter().find(|lease| {
            lease.thread == thread
                && lease.path.as_deref() == Some(root.as_path())
                && lease.task_assignment_id.is_some()
        });
        let grant = match store.reserve_with_checkout_lock(
            WriterLeaseDraft {
                thread,
                actor_session_id: previous
                    .as_ref()
                    .and_then(|lease| lease.actor_session_id.clone()),
                task_assignment_id: previous
                    .as_ref()
                    .and_then(|lease| lease.task_assignment_id.clone()),
                anchor_state: self.head()?.map(|state| state.to_string_full()),
                anchor_root: previous.and_then(|lease| lease.anchor_root),
                path: Some(self.root().to_owned()),
                pid: Some(pid),
                boot_id: current_boot_id(),
            },
            Utc::now(),
        )? {
            WriterLeaseReserveOutcome::Reserved(grant) => grant,
            WriterLeaseReserveOutcome::LiveOwner(_) => {
                return Err(credential_error("lane has another live writer"));
            }
        };
        let bound = store.bind_hook_session(
            &grant.lease.lease_id,
            &grant.token,
            &session,
            pid,
            &birth,
            Utc::now(),
        )?;
        if !matches!(bound, WriterLeaseAuthOutcome::Authorized(_)) {
            return Err(credential_error("could not bind new lane lease"));
        }
        write_checkout_writer_credential(self.root(), &grant.lease.lease_id, &grant.token)
    }
    fn read_checkout_writer_credential(&self) -> Result<Option<WriterCredential>> {
        let path = self.root().join(".heddle").join(WRITER_CREDENTIAL_FILE);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(credential_error("lane credential is not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o777 != 0o600 {
                return Err(credential_error(
                    "lane credential must have 0600 permissions",
                ));
            }
        }
        let bytes = std::fs::read(path)?;
        let credential = serde_json::from_slice(&bytes)
            .map_err(|error| credential_error(format!("invalid lane credential: {error}")))?;
        Ok(Some(credential))
    }

    /// Remember which lane credential a capture started with before running hooks.
    pub fn checkout_writer_lease_id(&self) -> Result<Option<String>> {
        Ok(self
            .read_checkout_writer_credential()?
            .map(|credential| credential.lease))
    }

    /// Verify a command that may mutate thread state without saving a tree.
    /// Save itself takes the checkout mutation lock and authenticates again.
    pub fn authorize_checkout_writer_for(
        &self,
        thread: &str,
        selected_path: Option<&Path>,
    ) -> Result<()> {
        let store = WriterLeaseStore::new(self.heddle_dir());
        let owner = store.live_owner(thread, selected_path)?;
        let root = self.root().canonicalize()?;
        if let Some(owner) = &owner
            && (owner.path.as_deref().is_some_and(|path| path != root)
                || (owner.path.is_none() && self.current_lane()?.as_deref() != Some(thread)))
        {
            return Err(foreign_writer_error(&owner.lease_id));
        }
        if owner.is_none()
            && selected_path
                .map(|path| path.canonicalize())
                .transpose()?
                .as_deref()
                != Some(root.as_path())
            && self.current_lane()?.as_deref() != Some(thread)
        {
            return Ok(());
        }
        match (owner, self.read_checkout_writer_credential()?) {
            (None, None) if self.requires_hook_session(&store, thread)? => Err(credential_error(
                "lane requires a live harness session before mutation",
            )),
            (None, None) => Ok(()),
            (Some(owner), None) => Err(foreign_writer_error(&owner.lease_id)),
            (None, Some(_)) => Err(credential_error("lane writer lease is no longer active")),
            (Some(owner), Some(credential)) if owner.lease_id == credential.lease => {
                if lease_needs_harness(&owner) {
                    require_hook_owner(&owner)?;
                }
                match store.authenticate_and_renew(
                    &credential.lease,
                    &credential.token,
                    Utc::now(),
                )? {
                    WriterLeaseAuthOutcome::Authorized(authenticated)
                        if authenticated.path.as_deref() == Some(root.as_path())
                            || (authenticated.path.is_none() && authenticated.thread == thread) =>
                    {
                        Ok(())
                    }
                    _ => Err(credential_error("lane writer credential is not active")),
                }
            }
            (Some(_), Some(_)) => Err(credential_error(
                "lane writer credential does not own this checkout",
            )),
        }
    }

    /// Install authority supplied to an explicit agent command only when it
    /// owns this physical checkout and its attached lane.
    pub fn install_checkout_writer_credential(&self, lease: &str, token: &str) -> Result<()> {
        let _mutation_lock = lock_checkout_writer_handoff(self.heddle_dir(), self.root())?;
        let root = self.root().canonicalize()?;
        let store = WriterLeaseStore::new(self.heddle_dir());
        match store.authenticate_and_renew(lease, token, Utc::now())? {
            WriterLeaseAuthOutcome::Authorized(owner)
                if (owner.path.as_deref() == Some(root.as_path()) || owner.path.is_none())
                    && self.current_lane()?.as_deref() == Some(owner.thread.as_str()) =>
            {
                if lease_needs_harness(&owner) {
                    require_hook_owner(&owner)?;
                }
                write_checkout_writer_credential(self.root(), lease, token)
            }
            _ => Err(credential_error(
                "writer token does not own the current checkout and lane",
            )),
        }
    }

    fn checkout_mutation_lock(&self) -> Result<WriteLockGuard> {
        // Shared worktrees have distinct roots but one object/lease store.
        checkout_writer_lock(self.heddle_dir(), self.root())?
            .try_write()
            .map_err(|error| Error::Invalid(error.to_string()))?
            .ok_or_else(|| Error::Invalid("checkout already has an active mutation".into()))
    }
    /// Claim a temporary writer for a normal CLI mutation. An existing agent's
    /// live reservation must instead be authenticated with its actual token.
    pub fn acquire_checkout_writer(
        &self,
        thread: ContentHash,
        actor: &str,
        expected_lease: Option<&str>,
    ) -> Result<CheckoutWriterGuard> {
        if actor.is_empty() {
            return Err(Error::Invalid("checkout writer actor is required".into()));
        }
        let mutation_lock = self.checkout_mutation_lock()?;
        let store = WriterLeaseStore::new(self.heddle_dir());
        let credential = self.read_checkout_writer_credential()?;
        if credential
            .as_ref()
            .map(|credential| credential.lease.as_str())
            != expected_lease
        {
            return Err(credential_error(
                "lane writer authority changed during capture",
            ));
        }
        if let Some(credential) = credential {
            self.authenticate_writer_with_lock(
                &store,
                thread,
                &credential.lease,
                &credential.token,
            )?;
            return Ok(CheckoutWriterGuard {
                store,
                temporary: None,
                _mutation_lock: mutation_lock,
            });
        }
        if let Some(lane) = self.current_lane()?
            && self.requires_hook_session(&store, &lane)?
        {
            return Err(credential_error(
                "lane requires a live harness session before mutation",
            ));
        }
        let grant = match store.reserve_with_checkout_lock(
            WriterLeaseDraft {
                thread: thread.to_hex(),
                actor_session_id: Some(actor.to_owned()),
                task_assignment_id: None,
                anchor_state: self.head()?.map(|state| state.to_string_full()),
                anchor_root: None,
                path: Some(self.root().to_owned()),
                pid: Some(std::process::id()),
                boot_id: objects::store::current_boot_id(),
            },
            Utc::now(),
        )? {
            WriterLeaseReserveOutcome::Reserved(grant) => grant,
            WriterLeaseReserveOutcome::LiveOwner(owner) => {
                return Err(foreign_writer_error(&owner.lease_id));
            }
        };
        Ok(CheckoutWriterGuard {
            store,
            temporary: Some(grant),
            _mutation_lock: mutation_lock,
        })
    }
    /// Serialize a mutation under an existing agent's checkout lease.
    pub fn authenticate_checkout_writer(
        &self,
        thread: ContentHash,
        lease: &str,
        token: &str,
    ) -> Result<CheckoutWriterGuard> {
        let mutation_lock = self.checkout_mutation_lock()?;
        let store = WriterLeaseStore::new(self.heddle_dir());
        self.authenticate_writer_with_lock(&store, thread, lease, token)?;
        Ok(CheckoutWriterGuard {
            store,
            temporary: None,
            _mutation_lock: mutation_lock,
        })
    }

    fn authenticate_writer_with_lock(
        &self,
        store: &WriterLeaseStore,
        thread: ContentHash,
        lease: &str,
        token: &str,
    ) -> Result<()> {
        let root = self.root().canonicalize()?;
        if let Some(owner) = store.load(lease)?
            && lease_needs_harness(&owner)
        {
            require_hook_owner(&owner)?;
        }
        match store.authenticate_and_renew(lease, token, Utc::now())? {
            WriterLeaseAuthOutcome::Authorized(owner)
                if (owner.path.as_deref() == Some(root.as_path()) || owner.path.is_none())
                    && (owner.thread == thread.to_hex()
                        || self.current_lane()?.as_deref() == Some(owner.thread.as_str())) => {}
            _ => {
                return Err(credential_error(
                    "mutation requires this checkout's active writer token",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::belongs_to_harness_with;

    #[test]
    fn pid_one_harness_authorizes_its_descendants() {
        let parent = |pid| match pid {
            42 => Some(7),
            7 => Some(1),
            1 => Some(0),
            _ => None,
        };
        let birth = |pid| (pid == 1).then(|| "owner-birth".to_string());
        assert!(belongs_to_harness_with(1, "owner-birth", 42, parent, birth));
        assert!(!belongs_to_harness_with(
            1,
            "different-birth",
            42,
            parent,
            birth
        ));
        assert!(!belongs_to_harness_with(
            1,
            "owner-birth",
            99,
            parent,
            birth
        ));
    }
}
