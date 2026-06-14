// SPDX-License-Identifier: Apache-2.0
//! Ref-name validation rules.

/// Ref-name validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid ref name: {name}")]
pub struct RefNameError {
    pub name: String,
}

pub fn validate_ref_name(name: &str) -> Result<(), RefNameError> {
    if name.is_empty() {
        return Err(invalid(name));
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(invalid(name));
    }
    if name.contains("..") || name.contains("//") {
        return Err(invalid(name));
    }
    if name.contains('\\') || name.starts_with('/') || name.ends_with('/') {
        return Err(invalid(name));
    }
    if name.starts_with('.') {
        return Err(invalid(name));
    }
    // Git refname rule: no path *component* may end in `.lock` (not just
    // the whole ref) — `refs/heads/foo.lock/bar` would collide with the
    // on-disk lockfile of `refs/heads/foo`.
    if name
        .split('/')
        .any(|component| component.ends_with(".lock"))
    {
        return Err(invalid(name));
    }
    Ok(())
}

fn invalid(name: &str) -> RefNameError {
    RefNameError {
        name: name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_ref_name;

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
}
