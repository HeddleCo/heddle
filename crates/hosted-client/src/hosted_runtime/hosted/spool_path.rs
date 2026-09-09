// SPDX-License-Identifier: Apache-2.0
//! Personal-first hosted spool path resolution for reads.
//!
//! A bare first path segment on clone/pull resolves against the caller's
//! personal root (`spool/<personal-slug>/<seg>`) first, then falls back to
//! the top-level spool (`spool/<seg>`). Writes are not resolved here.
//! Authorization is always the caller's identity via GetCurrentUserSpool;
//! this module never constructs another account's personal path.

use wire::ProtocolError;

use super::HostedClient;

const SPOOL_PREFIX: &str = "spool/";

/// How a typed hosted path should be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedReadPath {
    /// Use this canonical path as-is (explicit `spool/…` or multi-segment).
    Explicit(String),
    /// Probe `personal` first; only if it is absent for this caller, use `root`.
    PersonalFirst { personal: String, root: String },
}

/// Strip a leading `spool/` if present.
pub fn strip_spool_prefix(path: &str) -> &str {
    path.strip_prefix(SPOOL_PREFIX).unwrap_or(path)
}

/// Canonicalize a typed hosted path to `spool/…`.
///
/// Rejects empty components, `.`, and `..`. Does not apply personal-first
/// resolution — callers that need that use [`plan_personal_first_read`].
pub fn canonicalize_spool_path(typed: &str) -> Result<String, ProtocolError> {
    let normalized = normalize_typed_path(typed)?;
    Ok(ensure_spool_prefix(&normalized))
}

/// True when `path` is a root-level spool (`spool/<one-segment>`).
pub fn is_root_level_spool_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(SPOOL_PREFIX) else {
        return false;
    };
    !rest.is_empty() && !rest.contains('/')
}

/// Plan personal-first read resolution for a typed hosted path.
///
/// `personal_root` is the caller's GetCurrentUserSpool `full_path` (for
/// example `spool/willow-ibis-8e7264`). An empty root means the caller has
/// no personal spool (anonymous or unprovisioned): a bare name then
/// resolves only to the top-level path.
pub fn plan_personal_first_read(
    personal_root: &str,
    typed_path: &str,
) -> Result<HostedReadPath, ProtocolError> {
    let typed = normalize_typed_path(typed_path)?;
    if typed.contains('/') {
        return Ok(HostedReadPath::Explicit(ensure_spool_prefix(&typed)));
    }
    let root = format!("{SPOOL_PREFIX}{typed}");
    let personal_root = personal_root.trim().trim_matches('/');
    if personal_root.is_empty() {
        return Ok(HostedReadPath::Explicit(root));
    }
    let personal = join_child(personal_root, &typed);
    if personal == root {
        return Ok(HostedReadPath::Explicit(root));
    }
    Ok(HostedReadPath::PersonalFirst { personal, root })
}

/// Resolve a typed hosted path for a read (clone/pull).
///
/// Identity comes from GetCurrentUserSpool. A miss or existence-hiding
/// denial on the personal child falls back to the root path. Any other
/// error fails closed so a lookup blip cannot clone a different spool.
pub async fn resolve_personal_first_read(
    client: &mut HostedClient,
    typed_path: &str,
) -> Result<String, ProtocolError> {
    let typed = normalize_typed_path(typed_path)?;
    if typed.contains('/') {
        return Ok(ensure_spool_prefix(&typed));
    }
    let personal_root = match client.get_current_user_spool().await {
        Ok(spool) => spool.full_path,
        Err(
            ProtocolError::AuthorizationFailed(_)
            | ProtocolError::AuthenticationFailed(_)
            | ProtocolError::ObjectNotFound(_),
        ) => String::new(),
        Err(ProtocolError::RemoteFailure {
            code:
                wire::RemoteFailureCode::PermissionDenied
                | wire::RemoteFailureCode::Unauthenticated
                | wire::RemoteFailureCode::NotFound,
            ..
        }) => String::new(),
        Err(err) => return Err(err),
    };
    match plan_personal_first_read(&personal_root, &typed)? {
        HostedReadPath::Explicit(path) => Ok(path),
        HostedReadPath::PersonalFirst { personal, root } => {
            if personal_child_exists(client, &personal).await? {
                Ok(personal)
            } else {
                Ok(root)
            }
        }
    }
}

async fn personal_child_exists(
    client: &mut HostedClient,
    personal_path: &str,
) -> Result<bool, ProtocolError> {
    match client.get_spool(personal_path).await {
        Ok(spool) if spool.full_path == personal_path => Ok(true),
        Ok(spool) => Err(ProtocolError::InvalidState(format!(
            "GetSpool({personal_path}) returned a different spool ({})",
            spool.full_path
        ))),
        Err(ProtocolError::ObjectNotFound(_) | ProtocolError::AuthorizationFailed(_)) => Ok(false),
        Err(ProtocolError::RemoteFailure {
            code: wire::RemoteFailureCode::NotFound | wire::RemoteFailureCode::PermissionDenied,
            ..
        }) => Ok(false),
        Err(err) => Err(err),
    }
}

fn normalize_typed_path(typed: &str) -> Result<String, ProtocolError> {
    let trimmed = typed.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(ProtocolError::InvalidState(
            "hosted spool path must not be empty".to_string(),
        ));
    }
    if trimmed
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(ProtocolError::InvalidState(
            "hosted spool path must contain nonempty names, without '.' or '..'".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn ensure_spool_prefix(path: &str) -> String {
    if path == "spool" || path.starts_with(SPOOL_PREFIX) {
        path.to_string()
    } else {
        format!("{SPOOL_PREFIX}{path}")
    }
}

fn join_child(parent: &str, slug: &str) -> String {
    let parent = ensure_spool_prefix(parent.trim().trim_matches('/'));
    format!("{parent}/{slug}")
}

#[cfg(test)]
mod tests {
    use super::{HostedReadPath, is_root_level_spool_path, plan_personal_first_read};

    #[test]
    fn bare_name_prefers_the_caller_personal_child_over_the_root() {
        let plan = plan_personal_first_read("spool/alice", "foo").expect("plan");
        assert_eq!(
            plan,
            HostedReadPath::PersonalFirst {
                personal: "spool/alice/foo".into(),
                root: "spool/foo".into(),
            }
        );
    }

    #[test]
    fn caller_with_no_personal_root_gets_the_root_spool() {
        let plan = plan_personal_first_read("", "foo").expect("plan");
        assert_eq!(plan, HostedReadPath::Explicit("spool/foo".into()));
    }

    #[test]
    fn never_constructs_another_owners_personal_path() {
        let plan = plan_personal_first_read("spool/alice", "foo").expect("plan");
        match plan {
            HostedReadPath::PersonalFirst { personal, root } => {
                assert_eq!(personal, "spool/alice/foo");
                assert_eq!(root, "spool/foo");
                assert!(
                    !personal.contains("spool/bob/"),
                    "must not shadow a root spool with a different owner's child"
                );
            }
            other => panic!("expected personal-first, got {other:?}"),
        }
    }

    #[test]
    fn explicit_spool_root_is_not_rewritten_to_personal() {
        let plan = plan_personal_first_read("spool/alice", "spool/foo").expect("plan");
        assert_eq!(plan, HostedReadPath::Explicit("spool/foo".into()));
    }

    #[test]
    fn explicit_personal_path_stays_explicit() {
        let plan = plan_personal_first_read("spool/alice", "spool/alice/foo").expect("plan");
        assert_eq!(plan, HostedReadPath::Explicit("spool/alice/foo".into()));
    }

    #[test]
    fn multi_segment_without_prefix_is_prefixed_not_personal_first() {
        let plan = plan_personal_first_read("spool/alice", "alice/foo").expect("plan");
        assert_eq!(plan, HostedReadPath::Explicit("spool/alice/foo".into()));
    }

    #[test]
    fn personal_root_without_spool_prefix_still_scopes_to_the_caller() {
        let plan = plan_personal_first_read("alice", "foo").expect("plan");
        assert_eq!(
            plan,
            HostedReadPath::PersonalFirst {
                personal: "spool/alice/foo".into(),
                root: "spool/foo".into(),
            }
        );
    }

    #[test]
    fn rejects_dot_and_empty_components() {
        for path in ["", ".", "..", "foo/../bar", "foo//bar", "foo/."] {
            assert!(
                plan_personal_first_read("spool/alice", path).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn root_level_detection() {
        assert!(is_root_level_spool_path("spool/foo"));
        assert!(!is_root_level_spool_path("spool/alice/foo"));
        assert!(!is_root_level_spool_path("foo"));
        assert!(!is_root_level_spool_path("spool/"));
    }

    #[test]
    fn picking_the_root_when_a_personal_child_exists_is_the_wrong_spool() {
        // This is the security assertion the issue requires: given both
        // spool/<me>/foo and spool/foo, a bare `foo` must select personal.
        match plan_personal_first_read("spool/me", "foo").expect("plan") {
            HostedReadPath::PersonalFirst { personal, root } => {
                assert_eq!(personal, "spool/me/foo");
                assert_eq!(root, "spool/foo");
                assert_ne!(
                    personal, root,
                    "personal and root must be distinct so the probe can pick the caller's copy"
                );
            }
            HostedReadPath::Explicit(path) => {
                panic!("bare foo must not skip personal-first; would clone {path}")
            }
        }
    }
}
