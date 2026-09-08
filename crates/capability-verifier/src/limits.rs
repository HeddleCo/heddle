// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{Error, Result};

/// Strict owner-authz v2 parsing and evaluation ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationLimits {
    max_capability_ttl_seconds: i64,
    max_bundle_bytes: usize,
    max_payload_bytes: usize,
}

impl VerificationLimits {
    /// v2 maximum encoded owner-authorization bundle size.
    pub const MAX_BUNDLE_BYTES: usize = 1_048_576;
    /// v2 maximum owner-state transition count.
    pub const MAX_TRANSITIONS: usize = 256;
    /// v2 maximum capability-chain depth (only depth one can authorize purge).
    pub const MAX_CAPABILITIES: usize = 64;
    /// v2 maximum grants in one capability.
    pub const MAX_GRANTS: usize = 64;
    /// v2 maximum path segments in one selector or keyring path.
    pub const MAX_PATH_SEGMENTS: usize = 64;
    /// v2 maximum UTF-8 byte length of one path segment.
    pub const MAX_PATH_SEGMENT_BYTES: usize = 255;
    /// v2 maximum raw purge payload size.
    pub const MAX_PAYLOAD_BYTES: usize = 67_108_864;

    /// Construct limits with the caller's product TTL ceiling.
    ///
    /// Structural byte and count ceilings remain fixed at the v2 contract
    /// values and cannot be relaxed by untrusted input.
    pub fn new(max_capability_ttl_seconds: i64) -> Result<Self> {
        if max_capability_ttl_seconds <= 0 {
            return Err(Error::Invalid(
                "capability TTL ceiling must be positive".to_owned(),
            ));
        }
        Ok(Self {
            max_capability_ttl_seconds,
            max_bundle_bytes: Self::MAX_BUNDLE_BYTES,
            max_payload_bytes: Self::MAX_PAYLOAD_BYTES,
        })
    }

    /// Maximum owner-capability lifetime and key-handover overlap.
    #[must_use]
    pub const fn max_capability_ttl_seconds(self) -> i64 {
        self.max_capability_ttl_seconds
    }

    /// Maximum encoded portable bundle size.
    #[must_use]
    pub const fn max_bundle_bytes(self) -> usize {
        self.max_bundle_bytes
    }

    /// Maximum raw sidecar payload size.
    #[must_use]
    pub const fn max_payload_bytes(self) -> usize {
        self.max_payload_bytes
    }
}
