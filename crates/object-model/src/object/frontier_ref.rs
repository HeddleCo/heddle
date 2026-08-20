// SPDX-License-Identifier: Apache-2.0
//! Synthetic frontier-root names.
//!
//! Sibling-line roots advertised for a merge-frontier antichain live in the
//! reserved `heddle/` namespace as `heddle/frontier/<thread>/<full-changeid>`.
//! The Git projection of the same root is
//! `refs/heddle/frontier/<thread>/<full-changeid>`.
//!
//! The ChangeId suffix is always [`ChangeId::to_string_full`] — never the
//! truncatable [`ChangeId::short`] / `Display` form — so two siblings that
//! share a prefix remain distinct refs.

use super::{
    ChangeId, ChangeIdParseError, RESERVED_REF_SEGMENT, ThreadName, is_reserved_heddle_namespace,
};

/// Wire and local-store prefix for synthetic frontier roots.
pub const SYNTHETIC_FRONTIER_PREFIX: &str = "heddle/frontier/";

/// Git-side prefix for the same roots. Disjoint from `refs/heads/`.
pub const GIT_SYNTHETIC_FRONTIER_PREFIX: &str = "refs/heddle/frontier/";

/// Type-distinct name of a synthetic frontier root.
///
/// This is not a [`ThreadName`] and not a [`MarkerName`]. Consume, store, and
/// mirror sites must persist it through the synthetic-ref path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntheticFrontierName {
    thread: String,
    change_id: ChangeId,
}

/// Why a synthetic frontier name could not be built or parsed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SyntheticFrontierNameError {
    #[error("synthetic frontier thread name is empty")]
    EmptyThread,
    #[error("synthetic frontier thread '{name}' occupies the reserved heddle/ namespace")]
    ReservedThread { name: String },
    #[error("synthetic frontier name '{name}' is not heddle/frontier/<thread>/<full-changeid>")]
    InvalidForm { name: String },
    #[error("synthetic frontier change id is not a full ChangeId: {0}")]
    ChangeId(#[from] ChangeIdParseError),
}

impl SyntheticFrontierName {
    /// Build a synthetic root for `thread` at `change_id`.
    ///
    /// `thread` is the user thread whose sibling line this root names. It must
    /// itself be a user name (not `heddle/`-rooted).
    pub fn new(
        thread: impl AsRef<str>,
        change_id: ChangeId,
    ) -> Result<Self, SyntheticFrontierNameError> {
        let thread = thread.as_ref();
        if thread.is_empty() {
            return Err(SyntheticFrontierNameError::EmptyThread);
        }
        if is_reserved_heddle_namespace(thread) {
            return Err(SyntheticFrontierNameError::ReservedThread {
                name: thread.to_string(),
            });
        }
        Ok(Self {
            thread: thread.to_string(),
            change_id,
        })
    }

    /// Parse a wire/store name of the form `heddle/frontier/<thread>/<full-changeid>`.
    ///
    /// The last `/`-segment is the ChangeId (`hc-` + full Crockford base32).
    /// Everything between `heddle/frontier/` and that segment is the thread
    /// name, which may itself contain `/`.
    pub fn parse(name: &str) -> Result<Self, SyntheticFrontierNameError> {
        let Some(rest) = strip_frontier_prefix(name) else {
            return Err(SyntheticFrontierNameError::InvalidForm {
                name: name.to_string(),
            });
        };
        let Some((thread, change_id)) = rest.rsplit_once('/') else {
            return Err(SyntheticFrontierNameError::InvalidForm {
                name: name.to_string(),
            });
        };
        let parsed = ChangeId::parse(change_id)?;
        if change_id != parsed.to_string_full() {
            return Err(SyntheticFrontierNameError::InvalidForm {
                name: name.to_string(),
            });
        }
        Self::new(thread, parsed)
    }

    /// True when `name` is a well-formed synthetic frontier root.
    pub fn looks_like(name: &str) -> bool {
        Self::parse(name).is_ok()
    }

    pub fn thread(&self) -> &str {
        &self.thread
    }

    pub fn change_id(&self) -> ChangeId {
        self.change_id
    }

    /// Wire / local-store name: `heddle/frontier/<thread>/<full-changeid>`.
    pub fn as_name(&self) -> String {
        format!(
            "{SYNTHETIC_FRONTIER_PREFIX}{}/{}",
            self.thread,
            self.change_id.to_string_full()
        )
    }

    /// Git-side name: `refs/heddle/frontier/<thread>/<full-changeid>`.
    pub fn git_ref(&self) -> String {
        format!(
            "{GIT_SYNTHETIC_FRONTIER_PREFIX}{}/{}",
            self.thread,
            self.change_id.to_string_full()
        )
    }

    /// The user thread this synthetic root belongs to, as a [`ThreadName`].
    ///
    /// Only the *owning* user thread is a ThreadName. The synthetic root
    /// itself must never be coerced into one.
    pub fn owning_thread(&self) -> ThreadName {
        ThreadName::new(&self.thread)
    }
}

impl std::fmt::Display for SyntheticFrontierName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_name())
    }
}

fn strip_frontier_prefix(name: &str) -> Option<&str> {
    if let Some(rest) = name.strip_prefix(SYNTHETIC_FRONTIER_PREFIX) {
        return nonempty(rest);
    }
    if let Some(rest) = name.strip_prefix(GIT_SYNTHETIC_FRONTIER_PREFIX) {
        return nonempty(rest);
    }
    let mut parts = name.splitn(3, '/');
    let first = parts.next()?;
    let second = parts.next()?;
    let rest = parts.next()?;
    if first.eq_ignore_ascii_case(RESERVED_REF_SEGMENT) && second.eq_ignore_ascii_case("frontier") {
        nonempty(rest)
    } else {
        None
    }
}

fn nonempty(rest: &str) -> Option<&str> {
    if rest.is_empty() { None } else { Some(rest) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(last: u8) -> ChangeId {
        let mut bytes = [0u8; 16];
        bytes[15] = last;
        ChangeId::from_bytes(bytes)
    }

    #[test]
    fn full_change_id_keeps_prefix_sharing_siblings_distinct() {
        let a = cid(0);
        let mut shared = [0u8; 16];
        shared[0] = 0xaa;
        shared[1] = 0xbb;
        shared[15] = 1;
        let b = ChangeId::from_bytes(shared);
        let mut c_bytes = shared;
        c_bytes[15] = 2;
        let c = ChangeId::from_bytes(c_bytes);

        let left = SyntheticFrontierName::new("main", b).unwrap();
        let right = SyntheticFrontierName::new("main", c).unwrap();
        assert_ne!(left.as_name(), right.as_name());
        assert_ne!(left.git_ref(), right.git_ref());
        assert!(left.as_name().contains(&b.to_string_full()));
        assert!(right.as_name().contains(&c.to_string_full()));
        assert!(!left.as_name().ends_with(&b.short()));
        let _ = a;
    }

    #[test]
    fn parse_round_trips_slashed_thread_and_full_change_id() {
        let change = cid(9);
        let name = SyntheticFrontierName::new("feature/auth", change).unwrap();
        let parsed = SyntheticFrontierName::parse(&name.as_name()).unwrap();
        assert_eq!(parsed.thread(), "feature/auth");
        assert_eq!(parsed.change_id(), change);
        assert_eq!(
            parsed.git_ref(),
            format!(
                "refs/heddle/frontier/feature/auth/{}",
                change.to_string_full()
            )
        );
    }

    #[test]
    fn rejects_short_change_id_suffix() {
        let change = cid(3);
        let short = format!("heddle/frontier/main/{}", change.short());
        assert!(SyntheticFrontierName::parse(&short).is_err());
    }

    #[test]
    fn user_thread_at_change_id_is_not_a_synthetic_root() {
        let change = cid(4);
        let user = format!("main@{}", change.to_string_full());
        assert!(!is_reserved_heddle_namespace(&user));
        assert!(SyntheticFrontierName::parse(&user).is_err());
        assert_ne!(
            user,
            SyntheticFrontierName::new("main", change)
                .unwrap()
                .as_name()
        );
    }

    #[test]
    fn refuses_a_reserved_thread_component() {
        let change = cid(5);
        assert!(SyntheticFrontierName::new("heddle/nested", change).is_err());
    }
}
