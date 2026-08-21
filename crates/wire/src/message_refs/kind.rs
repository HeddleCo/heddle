// SPDX-License-Identifier: Apache-2.0
//! Type-distinct advertised-ref discriminator and consume routing.

use objects::object::{
    MarkerName, ReservedRefNameError, StateId, SyntheticFrontierName, SyntheticFrontierNameError,
    ThreadName, is_reserved_heddle_namespace,
};
use serde::{Deserialize, Serialize};

use super::RefEntry;

/// Type-distinct discriminator for an advertised ref.
///
/// Synthetic frontier roots must never be coerced into a [`ThreadName`] or
/// [`MarkerName`]. Match on this enum at every consume/store/mirror site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Thread,
    Marker,
    SyntheticFrontierRoot,
}

impl RefKind {
    /// Classify an advertised name.
    ///
    /// A well-formed synthetic frontier name is always
    /// [`RefKind::SyntheticFrontierRoot`], even if a boolean `is_thread` flag
    /// still claims otherwise. A reserved `heddle/` name that is not a
    /// well-formed frontier root is also classified as synthetic so it cannot
    /// fall through to the thread or marker arms (fail closed).
    pub fn from_advertised_name(name: &str, advertised_as_thread: bool) -> Self {
        if SyntheticFrontierName::looks_like(name) || is_reserved_heddle_namespace(name) {
            return RefKind::SyntheticFrontierRoot;
        }
        if advertised_as_thread {
            RefKind::Thread
        } else {
            RefKind::Marker
        }
    }

    pub fn is_user_thread(self) -> bool {
        matches!(self, RefKind::Thread)
    }

    pub fn is_marker(self) -> bool {
        matches!(self, RefKind::Marker)
    }
}

/// Typed view of an advertised ref. The synthetic arm never holds a
/// [`ThreadName`] or [`MarkerName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvertisedRef {
    Thread(ThreadName),
    Marker(MarkerName),
    SyntheticFrontier(SyntheticFrontierName),
}

/// Why an advertised ref could not be consumed as its declared kind.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AdvertisedRefError {
    #[error("{0}")]
    Reserved(#[from] ReservedRefNameError),
    #[error("{0}")]
    Synthetic(#[from] SyntheticFrontierNameError),
    #[error("ref '{name}' is reserved and cannot be consumed as a {kind:?}")]
    KindMismatch { name: String, kind: RefKind },
}

impl RefEntry {
    pub fn thread(name: impl Into<String>, state_id: StateId) -> Self {
        Self {
            name: name.into(),
            state_id,
            kind: RefKind::Thread,
        }
    }

    pub fn marker(name: impl Into<String>, state_id: StateId) -> Self {
        Self {
            name: name.into(),
            state_id,
            kind: RefKind::Marker,
        }
    }

    pub fn synthetic_frontier(name: SyntheticFrontierName, state_id: StateId) -> Self {
        Self {
            name: name.as_name(),
            state_id,
            kind: RefKind::SyntheticFrontierRoot,
        }
    }

    pub fn from_advertised(
        name: impl Into<String>,
        state_id: StateId,
        advertised_as_thread: bool,
    ) -> Self {
        let name = name.into();
        let kind = RefKind::from_advertised_name(&name, advertised_as_thread);
        Self {
            name,
            state_id,
            kind,
        }
    }

    pub fn is_user_thread(&self) -> bool {
        self.kind.is_user_thread()
    }

    pub fn is_marker(&self) -> bool {
        self.kind.is_marker()
    }

    /// Route this entry to its type-distinct consume form.
    ///
    /// [`RefKind::SyntheticFrontierRoot`] yields only a
    /// [`SyntheticFrontierName`]. It never constructs a [`ThreadName`] or
    /// [`MarkerName`].
    pub fn advertised(&self) -> Result<AdvertisedRef, AdvertisedRefError> {
        match self.kind {
            RefKind::Thread => {
                if is_reserved_heddle_namespace(&self.name) {
                    return Err(AdvertisedRefError::KindMismatch {
                        name: self.name.clone(),
                        kind: self.kind,
                    });
                }
                Ok(AdvertisedRef::Thread(ThreadName::try_new(
                    self.name.clone(),
                )?))
            }
            RefKind::Marker => {
                if is_reserved_heddle_namespace(&self.name) {
                    return Err(AdvertisedRefError::KindMismatch {
                        name: self.name.clone(),
                        kind: self.kind,
                    });
                }
                Ok(AdvertisedRef::Marker(MarkerName::try_new(
                    self.name.clone(),
                )?))
            }
            RefKind::SyntheticFrontierRoot => Ok(AdvertisedRef::SyntheticFrontier(
                SyntheticFrontierName::parse(&self.name)?,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{ChangeId, StateId, SyntheticFrontierName};

    use super::*;

    fn cid(last: u8) -> ChangeId {
        let mut bytes = [0u8; 16];
        bytes[15] = last;
        ChangeId::from_bytes(bytes)
    }

    #[test]
    fn advertised_synthetic_root_never_becomes_thread_or_marker_name() {
        let change = cid(11);
        let name = SyntheticFrontierName::new("main", change).unwrap();
        let entry = RefEntry::from_advertised(name.as_name(), StateId::from_bytes([1; 32]), true);
        assert_eq!(entry.kind, RefKind::SyntheticFrontierRoot);
        match entry.advertised().expect("synthetic consume") {
            AdvertisedRef::SyntheticFrontier(parsed) => assert_eq!(parsed, name),
            AdvertisedRef::Thread(_) | AdvertisedRef::Marker(_) => {
                panic!("synthetic root must not construct ThreadName or MarkerName")
            }
        }
    }

    #[test]
    fn advertised_as_thread_flag_cannot_reclassify_a_frontier_root() {
        let change = cid(12);
        let name = SyntheticFrontierName::new("main", change).unwrap();
        assert_eq!(
            RefKind::from_advertised_name(&name.as_name(), true),
            RefKind::SyntheticFrontierRoot
        );
        assert_eq!(
            RefKind::from_advertised_name(&name.as_name(), false),
            RefKind::SyntheticFrontierRoot
        );
        assert_eq!(
            RefKind::from_advertised_name("heddle/not-a-frontier", true),
            RefKind::SyntheticFrontierRoot
        );
    }

    #[test]
    fn user_thread_at_hd_suffix_stays_a_thread() {
        let entry = RefEntry::from_advertised("main@hd-abcdef", StateId::from_bytes([2; 32]), true);
        assert_eq!(entry.kind, RefKind::Thread);
        match entry.advertised().expect("user thread") {
            AdvertisedRef::Thread(name) => assert_eq!(name.as_str(), "main@hd-abcdef"),
            other => panic!("expected thread, got {other:?}"),
        }
    }
}
