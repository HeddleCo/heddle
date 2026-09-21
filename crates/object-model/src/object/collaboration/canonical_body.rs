// SPDX-License-Identifier: Apache-2.0

use std::sync::Mutex;

/// Cached canonical MessagePack body for a content-addressed value.
///
/// Not part of the serialized form. Equality ignores it. [`Clone`] drops it so a
/// clone cannot keep bytes that no longer match mutated fields. `encode` and
/// `decode` store the body; `id` / `hash` reuse it instead of serializing again.
/// Struct literals set this to [`Default::default`].
#[derive(Debug, Default)]
pub struct CanonicalBody {
    bytes: Mutex<Option<Vec<u8>>>,
}

impl Clone for CanonicalBody {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for CanonicalBody {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for CanonicalBody {}

impl CanonicalBody {
    pub(crate) fn cloned(&self) -> Option<Vec<u8>> {
        self.lock().clone()
    }

    pub(crate) fn store(&self, bytes: Vec<u8>) {
        *self.lock() = Some(bytes);
    }

    pub(crate) fn clear(&self) {
        *self.lock() = None;
    }

    /// Debug builds require `fresh` to reproduce `cached`. Release builds trust
    /// the body stored by the last successful `encode` or `decode`.
    pub(crate) fn debug_matches<E: std::fmt::Display>(
        cached: &[u8],
        fresh: impl FnOnce() -> Result<Vec<u8>, E>,
    ) {
        #[cfg(debug_assertions)]
        match fresh() {
            Ok(bytes) => debug_assert_eq!(
                cached,
                bytes.as_slice(),
                "cached canonical body does not match fields"
            ),
            Err(error) => {
                panic!("cached canonical body outlives a value that no longer encodes: {error}")
            }
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (cached, fresh);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.bytes.lock().unwrap_or_else(|err| err.into_inner())
    }
}
