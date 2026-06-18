// SPDX-License-Identifier: Apache-2.0
//! Segment-aware scope containment for namespace-tree access checks.
//!
//! This is the single authorization primitive for "is `candidate` inside the
//! namespace tree rooted at `scope`?". Callers MUST NOT reimplement this with
//! string prefix matching: `candidate.starts_with(&format!("{}/", scope))`
//! treats `..` segments literally, so `a/b/../c` passes a check against scope
//! `a/b` and then normalizes to the *sibling* `a/c` — a check-then-normalize
//! bypass (see heddle#631).

/// Whether `candidate` is the scope namespace itself or a descendant of it,
/// compared whole-segment by whole-segment.
///
/// Hardening rules (deny-by-default):
/// - Any `.` or `..` segment in either string is rejected outright. We never
///   normalize traversal segments — a path that needs normalizing is denied.
/// - Empty segments (`a//b`, leading/trailing `/`, or the empty string) are
///   rejected outright, so `/`-boundary tricks cannot smuggle segments past
///   the comparison.
/// - Comparison is per-segment, so scope `a/b` does NOT match `a/bc` (the
///   classic non-boundary prefix bug) and never grants upward access (scope
///   `a/b` does not match `a`).
pub fn scope_contains(scope: &str, candidate: &str) -> bool {
    let Some(scope_segments) = well_formed_segments(scope) else {
        return false;
    };
    let Some(candidate_segments) = well_formed_segments(candidate) else {
        return false;
    };

    candidate_segments.len() >= scope_segments.len()
        && scope_segments == candidate_segments[..scope_segments.len()]
}

/// Splits `path` on `/`, returning `None` if any segment is empty, `.`,
/// or `..` (including the empty string itself).
fn well_formed_segments(path: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return None;
    }
    Some(segments)
}

#[cfg(test)]
mod tests {
    use super::scope_contains;

    #[test]
    fn exact_match_allowed() {
        assert!(scope_contains("a/b", "a/b"));
    }

    #[test]
    fn descendant_allowed() {
        assert!(scope_contains("a/b", "a/b/c"));
        assert!(scope_contains("a/b", "a/b/c/d"));
    }

    #[test]
    fn dotdot_traversal_denied() {
        // The heddle#631 bypass: matches the old prefix check, normalizes to
        // the sibling a/c after it.
        assert!(!scope_contains("a/b", "a/b/../c"));
        assert!(!scope_contains("a/b", "a/b/.."));
        assert!(!scope_contains("a/b", "a/b/c/../../b/c"));
    }

    #[test]
    fn single_dot_denied() {
        // We deny rather than normalize: no traversal-ish input is trusted.
        assert!(!scope_contains("a/b", "a/b/./c"));
        assert!(!scope_contains("a/b", "a/b/."));
    }

    #[test]
    fn empty_segments_denied() {
        assert!(!scope_contains("a/b", "a//b"));
        assert!(!scope_contains("a/b", "a/b//c"));
        assert!(!scope_contains("a/b", "a/b/"));
        assert!(!scope_contains("a/b", "/a/b"));
        assert!(!scope_contains("a/b", ""));
    }

    #[test]
    fn sibling_denied() {
        assert!(!scope_contains("a/b", "a/c"));
    }

    #[test]
    fn upward_access_denied() {
        assert!(!scope_contains("a/b", "a"));
        assert!(!scope_contains("a/b/c", "a/b"));
    }

    #[test]
    fn non_boundary_prefix_denied() {
        // scope "a/b" must not match "a/bc" in either direction.
        assert!(!scope_contains("a/b", "a/bc"));
        assert!(!scope_contains("a/bc", "a/b"));
    }

    #[test]
    fn malformed_scope_denies_everything() {
        // A scope containing traversal/empty segments is misconfigured;
        // deny rather than guess.
        assert!(!scope_contains("a/..", "a"));
        assert!(!scope_contains("a/../b", "a/../b"));
        assert!(!scope_contains("a//b", "a/b/c"));
        assert!(!scope_contains("", "a"));
        assert!(!scope_contains("", ""));
    }
}
