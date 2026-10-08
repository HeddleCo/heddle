// SPDX-License-Identifier: Apache-2.0
//! Ref-name validation rules.

use objects::{error::HeddleError, object::is_reserved_heddle_namespace};

/// Ref-name validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid ref name: {name}")]
pub struct RefNameError {
    pub name: String,
}

pub fn validate_ref_name(name: &str) -> Result<(), RefNameError> {
    // Native suffixes include the branch `@`; check them in a full namespace.
    let git_name = objects::name_encoding::git_name(name);
    let full = format!("refs/heads/{git_name}");
    if sley_refs::check_refname_format(&full, false).is_err()
        || is_reserved_heddle_namespace(name)
        || (name.starts_with("git%") && objects::name_encoding::native_git_name(&git_name) != name)
    {
        return Err(invalid(name));
    }
    Ok(())
}

/// Shared reservation chokepoint for every [`crate::refs::CoreRefBackend`].
///
/// Filesystem paths already call [`validate_ref_name`]. Hosted Postgres
/// mutations (and the in-memory test backend) must go through this helper
/// so a `ThreadName`/`MarkerName` minted via `new`/`From` cannot persist a
/// user ref in the reserved `heddle/` namespace.
///
/// Serve-side `ListRefs`/`Pull` gating of synthetic roots remains weft#1728.
pub fn require_user_ref_name(name: impl AsRef<str>) -> objects::error::Result<()> {
    validate_ref_name(name.as_ref()).map_err(|error| HeddleError::InvalidRefName(error.name))
}

fn invalid(name: &str) -> RefNameError {
    RefNameError {
        name: name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use objects::error::HeddleError;

    use super::{require_user_ref_name, validate_ref_name};

    #[test]
    fn rejects_ref_ending_in_lock() {
        assert!(validate_ref_name("refs/heads/foo.lock").is_err());
    }

    #[test]
    fn rejects_any_component_ending_in_lock() {
        // The git refname rule is per-component: a `.lock` directory
        // collides with the sibling ref's lockfile.
        assert!(validate_ref_name("refs/heads/foo.lock/bar").is_err());
        assert!(validate_ref_name("refs/foo.lock/heads/bar").is_err());
    }

    #[test]
    fn allows_lock_as_non_suffix() {
        // `.lock` must be a component *suffix* to be rejected.
        assert!(validate_ref_name("refs/heads/foo.locker").is_ok());
        assert!(validate_ref_name("refs/heads/lock").is_ok());
        assert!(validate_ref_name("refs/heads/foo.lock.bak").is_ok());
    }

    #[test]
    fn allows_plain_ref() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
    }

    #[test]
    fn native_git_prefix_requires_canonical_mapping() {
        assert!(validate_ref_name("git%foo").is_err());
        let imported = objects::name_encoding::native_git_name("git%foo");
        assert!(validate_ref_name(&imported).is_ok());
    }

    #[test]
    fn reserves_heddle_rooted_names() {
        assert!(validate_ref_name("heddle/frontier/main/hc-abc").is_err());
        assert!(validate_ref_name("Heddle/x").is_err());
        assert!(validate_ref_name("heddle").is_ok());
        assert!(validate_ref_name("heddlefoo").is_ok());
        assert!(validate_ref_name("my/heddle").is_ok());
        assert!(validate_ref_name("main@review").is_ok());
        assert!(validate_ref_name("main@hd-abc").is_ok());
    }

    #[test]
    fn require_user_ref_name_rejects_reserved_namespace() {
        let error = require_user_ref_name("heddle/frontier/main/hc-abc").unwrap_err();
        assert!(
            matches!(error, HeddleError::InvalidRefName(name) if name == "heddle/frontier/main/hc-abc")
        );
        assert!(require_user_ref_name("main").is_ok());
    }
}
