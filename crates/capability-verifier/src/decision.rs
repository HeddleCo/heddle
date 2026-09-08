// SPDX-License-Identifier: MIT OR Apache-2.0

/// The single authoritative purge decision returned by the crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decision {
    /// Authorize the direct-owner purge operation.
    Purge,
    /// Deny the operation.
    Deny(Denial),
}

impl Decision {
    /// Whether this is any accepting decision.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        !matches!(self, Self::Deny(_))
    }
}

/// Stable denial categories for conformance and caller telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Denial {
    /// The input exceeded a byte or count ceiling.
    OverLimit,
    /// The public evidence was malformed or non-canonical.
    Malformed,
    /// A signature, signer id, or cryptographic chain failed.
    InvalidProof,
    /// The spool/genesis/root anchor did not match trusted context.
    GenesisBinding,
    /// Owner state or ownership history did not match current context.
    StaleOwner,
    /// The capability scope or action did not cover the operation.
    Capability,
    /// A direct-only action appeared in an attenuated capability.
    DirectOnly,
    /// Payload, spool, identity, or leaf-capability binding failed.
    OperationBinding,
    /// Evidence was outside its validity interval.
    Time,
}
