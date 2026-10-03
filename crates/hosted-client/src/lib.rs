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

#[cfg(test)]
pub(crate) fn with_test_principal(repo: repo::Repository) -> repo::Result<repo::Repository> {
    let mut config = repo.config().clone();
    config.set_principal("Heddle Test", "test@heddle.dev");
    config.save(&repo.heddle_dir().join("config.toml"))?;
    repo::Repository::open(repo.root())
}

/// Register factories needed to reopen CLI-owned lazy hosted repositories.
#[cfg(feature = "client")]
pub fn register_hosted_factory() {
    hosted_runtime::hosted::register_hosted_factory();
}

/// Polls one very large test future on a thread with an explicit stack.
///
/// The full device and hosted pull round trips nest many awaits over the Sync
/// messages, which carry heddle-api 0.31.0-alpha.17's inline 2.3 KB
/// import-authority bundle. Unoptimized builds give each nested future its own
/// stack slots, which outgrew the default 2 MiB test thread. Release builds and
/// the CLI's 8 MiB main thread are unaffected.
#[cfg(test)]
fn on_large_stack<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let outcome = std::thread::Builder::new()
        .name("large-stack-test".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(test())
        })
        .expect("large-stack test thread")
        .join();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
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
