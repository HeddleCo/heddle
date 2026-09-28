//! A physical checkout has one mutation lock and one shared writer lease.
use std::path::Path;

use chrono::Utc;
use objects::{
    HeddleError, RecoveryDetails,
    fs_atomic::write_file_atomic_secret,
    lock::{RepoLock, WriteLockGuard},
    object::ContentHash,
    store::{
        WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseGrant, WriterLeaseReserveOutcome,
        WriterLeaseStatus, WriterLeaseStore,
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
            match self.store.release(
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

    /// Verify a command that may mutate thread state without saving a tree.
    /// Save itself takes the checkout mutation lock and authenticates again.
    pub fn authorize_checkout_writer(&self) -> Result<()> {
        let store = WriterLeaseStore::new(self.heddle_dir());
        let thread = self.current_lane()?.unwrap_or_default();
        let owner = store.live_owner(&thread, Some(self.root()))?;
        match (owner, self.read_checkout_writer_credential()?) {
            (None, None) => Ok(()),
            (Some(owner), None) => Err(foreign_writer_error(&owner.lease_id)),
            (None, Some(_)) => Err(credential_error("lane writer lease is no longer active")),
            (Some(owner), Some(credential)) if owner.lease_id == credential.lease => {
                let root = self.root().canonicalize()?;
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
        let root = self.root().canonicalize()?;
        let store = WriterLeaseStore::new(self.heddle_dir());
        match store.authenticate_and_renew(lease, token, Utc::now())? {
            WriterLeaseAuthOutcome::Authorized(owner)
                if (owner.path.as_deref() == Some(root.as_path()) || owner.path.is_none())
                    && self.current_lane()?.as_deref() == Some(owner.thread.as_str()) =>
            {
                write_checkout_writer_credential(self.root(), lease, token)
            }
            _ => Err(credential_error(
                "writer token does not own the current checkout and lane",
            )),
        }
    }

    fn checkout_mutation_lock(&self) -> Result<WriteLockGuard> {
        // Shared worktrees have distinct roots but one object/lease store.
        let root = self.root().canonicalize()?;
        let path_key = ContentHash::compute_typed(
            "checkout-writer-path-v2",
            root.as_os_str().as_encoded_bytes(),
        );
        RepoLock::at(
            self.heddle_dir()
                .join("locks")
                .join(format!("checkout-{}.lock", path_key.to_hex())),
        )
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
    ) -> Result<CheckoutWriterGuard> {
        if actor.is_empty() {
            return Err(Error::Invalid("checkout writer actor is required".into()));
        }
        let mutation_lock = self.checkout_mutation_lock()?;
        let store = WriterLeaseStore::new(self.heddle_dir());
        if let Some(credential) = self.read_checkout_writer_credential()? {
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
        let grant = match store.reserve(
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
