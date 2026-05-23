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
    if name.ends_with(".lock") {
        return Err(invalid(name));
    }
    Ok(())
}

fn invalid(name: &str) -> RefNameError {
    RefNameError {
        name: name.to_string(),
    }
}
