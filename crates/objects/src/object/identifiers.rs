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
}
