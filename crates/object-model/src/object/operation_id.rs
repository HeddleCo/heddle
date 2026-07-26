// SPDX-License-Identifier: Apache-2.0
//! Client-supplied operation identifiers for idempotent state-changing calls.
//!
//! Every state-changing CLI verb and gRPC method accepts an [`OperationId`].
//! Repeated calls with the same id return the original outcome rather than
//! re-executing — the property the agent loop depends on for safe retry.
//! The newtype keeps the dedup intent visible at every callsite instead of
//! letting a bare `Uuid` blend in with other identifiers.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub Uuid);

impl OperationId {
    pub fn new() -> Self {
        // v7 (time-ordered): OperationId is an idempotency/dedup key, never a
        // secret, so a leaked creation-time is harmless and the ordering gives
        // better index locality wherever these keys are persisted/indexed.
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OperationIdParseError {
    #[error("invalid operation id: {0}")]
    InvalidUuid(#[from] uuid::Error),
}

impl FromStr for OperationId {
    type Err = OperationIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_generates_distinct_ids() {
        let a = OperationId::new();
        let b = OperationId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_round_trips_through_from_str() {
        let id = OperationId::new();
        let parsed: OperationId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn rejects_garbage() {
        assert!("not-a-uuid".parse::<OperationId>().is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let id = OperationId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: OperationId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
