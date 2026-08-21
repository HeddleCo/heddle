// SPDX-License-Identifier: Apache-2.0
//! Newtype wrappers for string identifiers that were previously bare
//! `String` / `&str`. The compiler enforces that a `ThreadName` cannot
//! be passed where a `MarkerName` is expected, catching mix-ups at
//! build time with zero runtime cost.
//!
//! Each type is `#[serde(transparent)]` so the on-disk / wire format
//! is byte-identical to a bare `String`. Existing oplog entries,
//! packed refs, and rmp-serde payloads decode unchanged.

use std::{fmt, hash::Hash};

use serde::{Deserialize, Serialize};

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl From<$name> for String {
            fn from(n: $name) -> String {
                n.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}

string_newtype!(
    /// Name of a heddle thread (branch-like construct).
    ThreadName
);

string_newtype!(
    /// Name of a heddle marker (tag-like construct).
    MarkerName
);

/// First path segment reserved for Heddle-internal refs.
///
/// A user may still name a thread `heddle`, `heddlefoo`, or `my/heddle`.
/// Only a `heddle/`-rooted name is reserved (Invariant C).
pub const RESERVED_REF_SEGMENT: &str = "heddle";

/// True when `name` occupies the reserved `heddle/` namespace.
///
/// The check is case-insensitive on the first segment so a raw-Git branch
/// `Heddle/frontier/...` cannot slip past the reservation.
pub fn is_reserved_heddle_namespace(name: &str) -> bool {
    let mut parts = name.split('/');
    match (parts.next(), parts.next()) {
        (Some(first), Some(_)) => first.eq_ignore_ascii_case(RESERVED_REF_SEGMENT),
        _ => false,
    }
}

/// Rejection when a user thread or marker name occupies `heddle/`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "ref name '{name}' is reserved: the heddle/ namespace is internal and cannot be a user thread or marker"
)]
pub struct ReservedRefNameError {
    pub name: String,
}

impl ThreadName {
    /// Fallible constructor for user or imported names. Rejects the reserved
    /// `heddle/` namespace. Trusted reconstruction of already-stored names
    /// still uses [`ThreadName::new`].
    pub fn try_new(s: impl Into<String>) -> Result<Self, ReservedRefNameError> {
        let name = s.into();
        if is_reserved_heddle_namespace(&name) {
            return Err(ReservedRefNameError { name });
        }
        Ok(Self(name))
    }
}

impl MarkerName {
    /// Fallible constructor for user or imported names. Rejects the reserved
    /// `heddle/` namespace. Trusted reconstruction of already-stored names
    /// still uses [`MarkerName::new`].
    pub fn try_new(s: impl Into<String>) -> Result<Self, ReservedRefNameError> {
        let name = s.into();
        if is_reserved_heddle_namespace(&name) {
            return Err(ReservedRefNameError { name });
        }
        Ok(Self(name))
    }
}

string_newtype!(
    /// Checkout/lane scope identifier for scoped operations.
    Scope
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_name_display() {
        let t = ThreadName::new("main");
        assert_eq!(t.0, "main");
        assert_eq!(t.0, "main");
        assert_eq!(&*t, "main");
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let t = ThreadName::new("feature/foo");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"feature/foo\"");
        let back: ThreadName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn marker_name_distinct_from_thread_name() {
        let _t: ThreadName = "main".into();
        let _m: MarkerName = "v1.0".into();
        // These are different types — the compiler prevents mixing them.
    }

    #[test]
    #[allow(clippy::cmp_owned)] // exercising PartialEq<String> impl by design
    fn comparison_with_str() {
        let t = ThreadName::from("main");
        assert!(t == "main");
        assert!(t == *"main");
        assert!(t == String::from("main"));
    }

    #[test]
    fn borrow_for_hashmap_lookup() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(ThreadName::new("main"), 1);
        assert_eq!(map.get("main"), Some(&1));
    }

    #[test]
    fn reserved_namespace_is_heddle_rooted_only() {
        assert!(!is_reserved_heddle_namespace("heddle"));
        assert!(!is_reserved_heddle_namespace("heddlefoo"));
        assert!(!is_reserved_heddle_namespace("my/heddle"));
        assert!(!is_reserved_heddle_namespace("main@review"));
        assert!(is_reserved_heddle_namespace("heddle/frontier/main/hc-abc"));
        assert!(is_reserved_heddle_namespace("Heddle/x"));
    }

    #[test]
    fn try_new_rejects_reserved_thread_and_marker_names() {
        assert!(ThreadName::try_new("heddle/frontier/main/hc-1").is_err());
        assert!(MarkerName::try_new("heddle/notes").is_err());
        assert_eq!(ThreadName::try_new("heddle").unwrap().as_str(), "heddle");
        assert_eq!(
            ThreadName::try_new("main@hd-abc").unwrap().as_str(),
            "main@hd-abc"
        );
    }
}
