// SPDX-License-Identifier: Apache-2.0
//! Heddle's in-repo hosted client.
//!
//! Transport, credentials, identity, and the hosted sync glue that verbs use
//! against a weft server. `heddle-api` protos remain the only shared seam with
//! weft/tapestry; this crate owns the client side of that contract.

pub mod attachments;
pub mod attribution;
pub mod client;
#[cfg(feature = "client")]
pub mod hosted_runtime;
#[cfg(feature = "client")]
pub mod network;

/// Register factories needed to reopen CLI-owned lazy hosted repositories.
#[cfg(feature = "client")]
pub fn register_hosted_factory() {
    hosted_runtime::hosted::register_hosted_factory();
}

#[cfg(test)]
mod test_process_env {
    //! Shared test gate for process-global credential environment.
    //!
    //! Tests that mutate environment variables take the write side; every
    //! other crate test takes the read side. This keeps unrelated repository
    //! fixtures parallel without letting them observe a temporary home or
    //! credential path.

    use std::sync::OnceLock;

    use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    static LOCK: OnceLock<RwLock<()>> = OnceLock::new();

    fn lock() -> &'static RwLock<()> {
        LOCK.get_or_init(|| RwLock::new(()))
    }

    pub async fn shared() -> RwLockReadGuard<'static, ()> {
        lock().read().await
    }

    pub async fn exclusive() -> RwLockWriteGuard<'static, ()> {
        lock().write().await
    }

    pub fn shared_blocking() -> RwLockReadGuard<'static, ()> {
        lock().blocking_read()
    }

    pub fn exclusive_blocking() -> RwLockWriteGuard<'static, ()> {
        lock().blocking_write()
    }
}
