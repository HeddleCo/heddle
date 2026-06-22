// SPDX-License-Identifier: Apache-2.0
//! Synchronization helpers shared across Heddle crates.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Extension methods for mutexes that should panic when poisoned.
pub trait LockExt {
    /// Guard returned by [`LockExt::lock_or_poisoned`].
    type Guard<'a>
    where
        Self: 'a;

    /// Lock the mutex, preserving the existing panic-on-poison behavior.
    fn lock_or_poisoned(&self) -> Self::Guard<'_>;
}

impl<T: ?Sized> LockExt for Mutex<T> {
    type Guard<'a>
        = MutexGuard<'a, T>
    where
        Self: 'a;

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    fn lock_or_poisoned(&self) -> Self::Guard<'_> {
        self.lock()
            .expect("invariant: lock not poisoned (a holder panicked)")
    }
}

/// Extension methods for read-write locks that should panic when poisoned.
pub trait RwLockExt {
    /// Guard returned by [`RwLockExt::read_or_poisoned`].
    type ReadGuard<'a>
    where
        Self: 'a;

    /// Guard returned by [`RwLockExt::write_or_poisoned`].
    type WriteGuard<'a>
    where
        Self: 'a;

    /// Acquire a read guard, preserving the existing panic-on-poison behavior.
    fn read_or_poisoned(&self) -> Self::ReadGuard<'_>;

    /// Acquire a write guard, preserving the existing panic-on-poison behavior.
    fn write_or_poisoned(&self) -> Self::WriteGuard<'_>;
}

impl<T: ?Sized> RwLockExt for RwLock<T> {
    type ReadGuard<'a>
        = RwLockReadGuard<'a, T>
    where
        Self: 'a;

    type WriteGuard<'a>
        = RwLockWriteGuard<'a, T>
    where
        Self: 'a;

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    fn read_or_poisoned(&self) -> Self::ReadGuard<'_> {
        self.read()
            .expect("invariant: lock not poisoned (a holder panicked)")
    }

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    fn write_or_poisoned(&self) -> Self::WriteGuard<'_> {
        self.write()
            .expect("invariant: lock not poisoned (a holder panicked)")
    }
}
