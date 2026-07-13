// SPDX-License-Identifier: Apache-2.0
//! Pure spool CLI display helpers (no gRPC / repo I/O).
//!
//! Owns mount-name defaults, short ids, and child-edge status labels for
//! `heddle spool …`. Hosted RPC and RecoveryAdvice stay CLI-owned.
//!
//! Child-edge status uses a pure enum so `heddle-core` does not depend on
//! `prost` / wire types. CLI maps `ChildEdgeStatus` (or wire `i32`) via
//! [`edge_status_from_i32`] / [`ChildEdgeStatusKind`].

/// Default mount name = the child path's last `/`-segment (trailing slashes ignored).
pub fn default_mount_name(child_path: &str) -> &str {
    child_path
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(child_path)
}

/// First 12 characters of a change-id / state string for human display.
///
/// Same truncation as [`crate::approval_plan::short_state_id`].
pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Pure child-edge status (mirrors hosted `ChildEdgeStatus` wire enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildEdgeStatusKind {
    /// Wire `UNSPECIFIED` (0).
    Unspecified,
    /// Wire `UP_TO_DATE` (1).
    UpToDate,
    /// Wire `FAST_FORWARDABLE` (2).
    FastForwardable,
    /// Wire `DIVERGED` (3).
    Diverged,
    /// Wire `NO_CHILD_HEAD` (4).
    NoChildHead,
}

/// Map stable protobuf discriminant values to [`ChildEdgeStatusKind`].
///
/// Unknown values map to [`ChildEdgeStatusKind::Unspecified`].
pub fn edge_status_from_i32(status: i32) -> ChildEdgeStatusKind {
    match status {
        1 => ChildEdgeStatusKind::UpToDate,
        2 => ChildEdgeStatusKind::FastForwardable,
        3 => ChildEdgeStatusKind::Diverged,
        4 => ChildEdgeStatusKind::NoChildHead,
        _ => ChildEdgeStatusKind::Unspecified,
    }
}

/// Human label for a child-edge status kind.
pub fn edge_status_label(kind: ChildEdgeStatusKind) -> &'static str {
    match kind {
        ChildEdgeStatusKind::UpToDate => "up-to-date",
        ChildEdgeStatusKind::FastForwardable => "fast-forwardable",
        ChildEdgeStatusKind::Diverged => "diverged",
        ChildEdgeStatusKind::NoChildHead => "no-child-head",
        ChildEdgeStatusKind::Unspecified => "unspecified",
    }
}

/// Label from a wire `i32` status (convenience for CLI after RPC).
pub fn edge_status_label_from_i32(status: i32) -> &'static str {
    edge_status_label(edge_status_from_i32(status))
}

/// Attach success header line.
pub fn spool_attach_message(child: &str, parent: &str, mount: &str) -> String {
    format!("Attached {child} under {parent} at '{mount}'")
}

/// Detach human line from whether a mount was removed.
pub fn spool_detach_message(mount_name: &str, parent: &str, removed: bool) -> String {
    if removed {
        format!("Detached '{mount_name}' from {parent}")
    } else {
        format!("No child mounted at '{mount_name}' under {parent} (nothing to detach).")
    }
}

/// Empty children list line.
pub fn spool_children_empty_message(parent: &str) -> String {
    format!("{parent} has no child spools.")
}

/// Non-empty children header.
pub fn spool_children_header(count: usize, parent: &str) -> String {
    format!("{count} child spool(s) of {parent}:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mount_name_uses_last_path_segment() {
        assert_eq!(default_mount_name("acme/lib"), "lib");
        assert_eq!(default_mount_name("acme/team/lib"), "lib");
        assert_eq!(default_mount_name("acme/lib/"), "lib");
        assert_eq!(default_mount_name("solo"), "solo");
        assert_eq!(default_mount_name("/"), "/");
    }

    #[test]
    fn short_id_truncates_to_twelve() {
        assert_eq!(short_id("abcdefghijklmnop"), "abcdefghijkl");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn edge_status_label_covers_every_variant() {
        assert_eq!(
            edge_status_label(ChildEdgeStatusKind::UpToDate),
            "up-to-date"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatusKind::FastForwardable),
            "fast-forwardable"
        );
        assert_eq!(edge_status_label(ChildEdgeStatusKind::Diverged), "diverged");
        assert_eq!(
            edge_status_label(ChildEdgeStatusKind::NoChildHead),
            "no-child-head"
        );
        assert_eq!(
            edge_status_label(ChildEdgeStatusKind::Unspecified),
            "unspecified"
        );
        // Wire discriminants from hosted.proto
        assert_eq!(edge_status_from_i32(0), ChildEdgeStatusKind::Unspecified);
        assert_eq!(edge_status_from_i32(1), ChildEdgeStatusKind::UpToDate);
        assert_eq!(
            edge_status_from_i32(2),
            ChildEdgeStatusKind::FastForwardable
        );
        assert_eq!(edge_status_from_i32(3), ChildEdgeStatusKind::Diverged);
        assert_eq!(edge_status_from_i32(4), ChildEdgeStatusKind::NoChildHead);
        assert_eq!(edge_status_from_i32(99), ChildEdgeStatusKind::Unspecified);
        assert_eq!(edge_status_label_from_i32(1), "up-to-date");
    }

    #[test]
    fn attach_detach_messages() {
        assert_eq!(
            spool_attach_message("acme/lib", "acme/app", "lib"),
            "Attached acme/lib under acme/app at 'lib'"
        );
        assert!(spool_detach_message("lib", "acme/app", true).starts_with("Detached"));
        assert!(spool_detach_message("lib", "acme/app", false).contains("nothing to detach"));
    }
}
