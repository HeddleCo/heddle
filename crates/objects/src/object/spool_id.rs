// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SpoolId(String);

impl SpoolId {
    pub fn parse(value: impl Into<String>) -> Result<Self, SpoolIdParseError> {
        let value = value.into();
        let mut segments = value.split('/');
        let namespace = segments.next().unwrap_or_default();
        let name = segments.next().unwrap_or_default();
        if segments.next().is_some() || !valid_segment(namespace) || !valid_segment(name) {
            return Err(SpoolIdParseError(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && segment
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && segment
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

impl std::fmt::Display for SpoolId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for SpoolId {
    type Error = SpoolIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SpoolId> for String {
    fn from(value: SpoolId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid spool id '{0}'; expected canonical namespace/name")]
pub struct SpoolIdParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_namespace_and_name() {
        let id = SpoolId::parse("acme/api-v2").unwrap();
        assert_eq!(id.as_str(), "acme/api-v2");
    }

    #[test]
    fn rejects_noncanonical_values() {
        for value in ["repo", "Acme/repo", "acme/repo/child", "acme/-repo"] {
            assert!(SpoolId::parse(value).is_err(), "accepted {value}");
        }
    }
}
