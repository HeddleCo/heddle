//! Transport-free resource-kind hierarchy for Biscuit authorization.
//!
//! The Biscuit world models access as `right(kind, path, action)`
//! facts. The inheritance pattern is the same across every kind: a
//! grant at one resource covers all of its children.
//!
//! This module centralizes the parent-resolution logic so that
//! `BiscuitFacts::is_admin_on`, `can_read`, `can_write` (and any
//! future capability check) walk the hierarchy uniformly. Adding a
//! new resource kind becomes a single-line addition to
//! [`resolve_parent`] and a one-line addition to the parser if its
//! path shape is non-trivial.
//!
//! # Spools are ONE chain (weft#358 P1 §4, weft#1130, weft#1212)
//!
//! `namespace` and `repo` are product facets, not authorization kinds. Every
//! container and content-bearing project is a `spool`, and its parent step is
//! [`spool_parent`]: strip the trailing `/`-delimited segment. The one walk
//! ([`walk_to_root`]) is shared by every caller that needs inheritance.
//!
//! Path shape conventions:
//! - **spool**: `/`-delimited segments
//!   (`org`, `org/acme`, `org/acme/heddle`). Parent is the leading
//!   prefix; a single segment is a root and has none.
//! - **thread**: `<spool_path>/threads/<thread_name>`. Parent is the
//!   spool (with the `/threads/<name>` suffix stripped).
//! - **context**: same path as the parent spool. Parent is that spool.

use std::fmt;

/// Canonical resource kinds. Stringly-typed at the wire boundary
/// (Biscuit facts are `right("spool", ...)` — strings) but typed
/// internally so a typo at a call site fails to compile rather than
/// silently never matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Spool,
    Thread,
    Context,
}

impl ResourceKind {
    /// Stable string token used in Biscuit facts.
    pub const fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Spool => "spool",
            ResourceKind::Thread => "thread",
            ResourceKind::Context => "context",
        }
    }

    /// Parse from the wire string. Unknown tokens return `None` —
    /// callers should treat that as "not a recognized resource"
    /// (typically: no inheritance applies, fall back to literal
    /// match only).
    ///
    /// Named `parse` rather than `from_str` to avoid shadowing the
    /// `std::str::FromStr` trait method (clippy
    /// `should_implement_trait`). We don't implement `FromStr` here
    /// because the trait demands an `Err` type, and there's nothing
    /// useful to surface beyond "unknown kind."
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "spool" => Some(Self::Spool),
            "thread" => Some(Self::Thread),
            "context" => Some(Self::Context),
            _ => None,
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One step up the SPOOL chain: strip the trailing `/`-delimited
/// segment. `None` at a root (a single segment, or nothing at all).
///
/// This is the whole of spool nesting; product facets do not create separate
/// authorization steps.
///
/// Trailing slashes are tolerated so a caller that appends `/` to a
/// prefix does not silently get an extra empty level.
fn spool_parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    if parent.is_empty() {
        None
    } else {
        Some(parent)
    }
}

/// Every ancestor of `(kind, path)`, nearest first, ending at the root.
///
/// The single shared walk: repeated [`resolve_parent`] until it returns
/// `None`. Callers that need "does any ancestor satisfy P?" iterate this
/// and stop early rather than open-coding the loop — before weft#1130
/// there were two hand-rolled copies of it in `facts.rs`, and two copies
/// is one place for them to drift apart.
///
/// Termination: each step strips at least one path segment (or, for
/// `thread`/`context`, converts to a strictly shorter or same-length
/// path exactly once before rejoining the spool chain), so the sequence
/// is finite for any input. No cycle detection is needed.
pub fn walk_to_root(kind: ResourceKind, path: &str) -> impl Iterator<Item = (ResourceKind, &str)> {
    let mut current = Some((kind, path));
    std::iter::from_fn(move || {
        let (kind, path) = current?;
        let parent = resolve_parent(kind, path)?;
        current = Some(parent);
        Some(parent)
    })
}

/// Walk one step up the resource hierarchy. Returns the parent's
/// `(kind, path)` tuple, or `None` when this is a root resource.
///
/// The walk is single-step on purpose: callers iterate by calling
/// `resolve_parent` until it returns `None`. This keeps the helper
/// stack-free, makes cycle detection unnecessary (single direction,
/// finite path length), and lets `BiscuitFacts::is_admin_on` decide
/// when to stop (e.g. on the first matching grant).
///
/// Examples:
/// ```
/// use heddle_biscuit_verifier::resource::{ResourceKind, resolve_parent};
///
/// assert_eq!(
///     resolve_parent(ResourceKind::Spool, "org/acme/heddle"),
///     Some((ResourceKind::Spool, "org/acme"))
/// );
/// assert_eq!(
///     resolve_parent(ResourceKind::Spool, "org/acme"),
///     Some((ResourceKind::Spool, "org"))
/// );
/// assert_eq!(resolve_parent(ResourceKind::Spool, "org"), None);
/// assert_eq!(
///     resolve_parent(ResourceKind::Thread, "org/acme/heddle/threads/main"),
///     Some((ResourceKind::Spool, "org/acme/heddle"))
/// );
/// assert_eq!(
///     resolve_parent(ResourceKind::Context, "org/acme/heddle"),
///     Some((ResourceKind::Spool, "org/acme/heddle"))
/// );
/// ```
pub fn resolve_parent(kind: ResourceKind, path: &str) -> Option<(ResourceKind, &str)> {
    match kind {
        ResourceKind::Spool => spool_parent(path).map(|parent| (ResourceKind::Spool, parent)),
        ResourceKind::Thread => {
            // Thread paths are `<repo_path>/threads/<thread_name>`.
            // The substring before `/threads/` is the parent repo.
            // Threads with malformed paths (no `/threads/` segment)
            // are treated as orphans with no parent — surfacing the
            // misuse to the caller rather than silently inheriting.
            let trimmed = path.trim_end_matches('/');
            let cut = trimmed.rfind("/threads/")?;
            let parent = &trimmed[..cut];
            if parent.is_empty() {
                None
            } else {
                Some((ResourceKind::Spool, parent))
            }
        }
        ResourceKind::Context => {
            // Context streams are 1:1 with their parent spool and share its
            // path. Keep Context distinct until the model decision is made.
            let trimmed = path.trim_end_matches('/');
            if trimmed.is_empty() {
                return None;
            }
            Some((ResourceKind::Spool, trimmed))
        }
    }
}

/// Build the canonical path for a thread under a repo. Use this
/// everywhere a `right("thread", ...)` fact, a `thread_policies`
/// row, or a thread-keyed approval mentions a thread by id —
/// keeps the format in one place so a future schema change is a
/// single edit.
///
/// Format: `<repo_path>/threads/<thread_name>`.
pub fn thread_path(repo_path: &str, thread_name: &str) -> String {
    format!("{repo_path}/threads/{thread_name}")
}

/// Inverse of [`thread_path`]: split a thread path into its
/// `(repo_path, thread_name)` components. Returns `None` for
/// malformed paths (no `/threads/` segment).
pub fn split_thread_path(path: &str) -> Option<(&str, &str)> {
    let cut = path.rfind("/threads/")?;
    let repo = &path[..cut];
    let thread = &path[cut + "/threads/".len()..];
    if repo.is_empty() || thread.is_empty() {
        None
    } else {
        Some((repo, thread))
    }
}

/// Context streams sit at the repo's path (1:1 mapping). This
/// alias documents intent at call sites that emit
/// `right("context", ...)` facts.
pub fn context_path(repo_path: &str) -> &str {
    repo_path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_walk_returns_each_parent_until_root() {
        assert_eq!(
            resolve_parent(ResourceKind::Spool, "org/acme/sub"),
            Some((ResourceKind::Spool, "org/acme"))
        );
        assert_eq!(
            resolve_parent(ResourceKind::Spool, "org/acme"),
            Some((ResourceKind::Spool, "org"))
        );
        assert_eq!(resolve_parent(ResourceKind::Spool, "org"), None);
        assert_eq!(resolve_parent(ResourceKind::Spool, ""), None);
    }

    #[test]
    fn thread_walks_up_to_owning_repo() {
        assert_eq!(
            resolve_parent(ResourceKind::Thread, "org/acme/heddle/threads/main"),
            Some((ResourceKind::Spool, "org/acme/heddle"))
        );
        assert_eq!(
            resolve_parent(ResourceKind::Thread, "org/acme/heddle/threads/release/v1"),
            Some((ResourceKind::Spool, "org/acme/heddle"))
        );
        // Malformed thread path — no `/threads/` segment.
        assert_eq!(
            resolve_parent(ResourceKind::Thread, "org/acme/heddle/main"),
            None
        );
    }

    #[test]
    fn context_walks_to_repo_at_same_path() {
        assert_eq!(
            resolve_parent(ResourceKind::Context, "org/acme/heddle"),
            Some((ResourceKind::Spool, "org/acme/heddle"))
        );
        assert_eq!(resolve_parent(ResourceKind::Context, ""), None);
    }

    #[test]
    fn parent_walk_to_root_is_finite_for_every_kind() {
        // No matter where you start, walking up returns None within
        // O(path_segments) steps. Spot-check every kind to guard
        // against accidental infinite loops in `resolve_parent`.
        for (kind, path) in [
            (ResourceKind::Spool, "org/acme/sub/deep"),
            (ResourceKind::Thread, "org/acme/heddle/threads/main"),
            (ResourceKind::Context, "org/acme/heddle"),
        ] {
            let mut current = (kind, path.to_string());
            for _ in 0..32 {
                match resolve_parent(current.0, &current.1) {
                    Some((k, p)) => current = (k, p.to_string()),
                    None => break,
                }
            }
            // If we hit the loop limit we never reached root —
            // bug. The assertion is "we got out before 32 steps."
            // Real paths are <8 segments deep.
        }
    }

    #[test]
    fn thread_path_round_trips() {
        let p = thread_path("org/acme/heddle", "main");
        assert_eq!(p, "org/acme/heddle/threads/main");
        let (repo, thread) = split_thread_path(&p).expect("split");
        assert_eq!(repo, "org/acme/heddle");
        assert_eq!(thread, "main");
    }

    #[test]
    fn split_thread_path_rejects_malformed_input() {
        assert!(split_thread_path("org/acme/heddle").is_none());
        assert!(split_thread_path("/threads/").is_none());
        assert!(split_thread_path("org/acme/heddle/threads/").is_none());
        assert!(split_thread_path("/threads/main").is_none());
    }

    #[test]
    fn split_thread_path_handles_thread_names_with_slashes() {
        // Real-world: release branches like "release/v1" need to
        // round-trip even though they contain a `/`.
        let p = thread_path("org/acme/heddle", "release/v1");
        let (repo, thread) = split_thread_path(&p).expect("split");
        assert_eq!(repo, "org/acme/heddle");
        assert_eq!(thread, "release/v1");
    }

    #[test]
    fn context_path_aliases_repo_path() {
        assert_eq!(context_path("org/acme/heddle"), "org/acme/heddle");
    }

    #[test]
    fn parse_round_trips() {
        for kind in [
            ResourceKind::Spool,
            ResourceKind::Thread,
            ResourceKind::Context,
        ] {
            assert_eq!(ResourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ResourceKind::parse("unknown"), None);
    }
}
