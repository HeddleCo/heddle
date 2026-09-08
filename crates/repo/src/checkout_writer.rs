//! A physical checkout has one mutation lock and one shared writer lease.
use chrono::Utc;
use objects::{
    lock::{RepoLock, WriteLockGuard},
    object::ContentHash,
    store::{
        WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseGrant, WriterLeaseReserveOutcome,
        WriterLeaseStatus, WriterLeaseStore,
    },
};

use crate::{
    Repository,
    thread_replication::{Error, Result},
};

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
                return Err(Error::Invalid(format!(
                    "checkout is reserved by writer {}",
                    owner.lease_id
                )));
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
        let root = self.root().canonicalize()?;
        match store.authenticate_and_renew(lease, token, Utc::now())? {
            WriterLeaseAuthOutcome::Authorized(owner)
                if owner.path.as_deref() == Some(root.as_path())
                    && owner.thread == thread.to_hex() => {}
            _ => {
                return Err(Error::Invalid(
                    "mutation requires this checkout's active writer token".into(),
                ));
            }
        }
        Ok(CheckoutWriterGuard {
            store,
            temporary: None,
            _mutation_lock: mutation_lock,
        })
    }
}
