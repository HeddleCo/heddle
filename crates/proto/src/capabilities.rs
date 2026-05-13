// SPDX-License-Identifier: Apache-2.0
//! Capability negotiation for Heddle protocol.
//!
//! Capabilities allow clients and servers to negotiate features and
//! protocol extensions during the handshake phase.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const CAPABILITY_CHUNKED_TRANSFER: &str = "chunked-transfer";
pub const CAPABILITY_RESUMABLE_TRANSFER: &str = "resumable-transfer";
pub const CAPABILITY_PACK_TRANSFER: &str = "pack-transfer";
pub const CAPABILITY_PARTIAL_FETCH: &str = "partial-fetch";

/// Set of protocol capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    /// Protocol version.
    pub version: u32,
    /// Supported capability flags.
    pub flags: HashSet<String>,
    /// Maximum object size supported (in bytes).
    pub max_object_size: u64,
    /// Preferred chunk size for streaming (in bytes).
    pub chunk_size: u32,
    /// Whether delta compression is supported.
    pub delta_compression: bool,
    /// Supported compression algorithms.
    pub compression: Vec<String>,
}

impl Capabilities {
    /// Create default capabilities.
    pub fn new(version: u32) -> Self {
        let mut flags = HashSet::new();
        flags.insert("baseline".to_string());

        Self {
            version,
            flags,
            max_object_size: 128 * 1024 * 1024,
            chunk_size: 64 * 1024,
            delta_compression: true,
            compression: vec!["none".to_string()],
        }
    }

    pub fn with_flag(mut self, flag: impl Into<String>) -> Self {
        self.flags.insert(flag.into());
        self
    }

    pub fn with_chunked_transfer(mut self, enabled: bool) -> Self {
        if enabled {
            self.flags.insert(CAPABILITY_CHUNKED_TRANSFER.to_string());
        } else {
            self.flags.remove(CAPABILITY_CHUNKED_TRANSFER);
        }
        self
    }

    pub fn with_resumable_transfer(mut self, enabled: bool) -> Self {
        if enabled {
            self.flags.insert(CAPABILITY_RESUMABLE_TRANSFER.to_string());
        } else {
            self.flags.remove(CAPABILITY_RESUMABLE_TRANSFER);
        }
        self
    }

    pub fn with_pack_transfer(mut self, enabled: bool) -> Self {
        if enabled {
            self.flags.insert(CAPABILITY_PACK_TRANSFER.to_string());
        } else {
            self.flags.remove(CAPABILITY_PACK_TRANSFER);
        }
        self
    }

    pub fn with_partial_fetch(mut self, enabled: bool) -> Self {
        if enabled {
            self.flags.insert(CAPABILITY_PARTIAL_FETCH.to_string());
        } else {
            self.flags.remove(CAPABILITY_PARTIAL_FETCH);
        }
        self
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    pub fn supports_chunked_transfer(&self) -> bool {
        self.has_flag(CAPABILITY_CHUNKED_TRANSFER)
    }

    pub fn supports_resumable_transfer(&self) -> bool {
        self.has_flag(CAPABILITY_RESUMABLE_TRANSFER)
    }

    pub fn supports_pack_transfer(&self) -> bool {
        self.has_flag(CAPABILITY_PACK_TRANSFER)
    }

    pub fn supports_partial_fetch(&self) -> bool {
        self.has_flag(CAPABILITY_PARTIAL_FETCH)
    }

    pub fn with_delta(mut self, enabled: bool) -> Self {
        self.delta_compression = enabled;
        self
    }

    pub fn with_compression(mut self, algo: impl Into<String>) -> Self {
        let algo = algo.into();
        if !self.compression.contains(&algo) {
            self.compression.push(algo);
        }
        self
    }

    pub fn with_chunk_size(mut self, size: u32) -> Self {
        self.chunk_size = size;
        self
    }

    pub fn with_max_object_size(mut self, size: u64) -> Self {
        self.max_object_size = size;
        self
    }

    pub fn negotiate(&self, other: &Capabilities) -> Capabilities {
        let version = self.version.min(other.version);
        let flags: HashSet<_> = self.flags.intersection(&other.flags).cloned().collect();
        let max_object_size = self.max_object_size.min(other.max_object_size);
        let chunk_size = self.chunk_size.min(other.chunk_size);
        let delta_compression = self.delta_compression && other.delta_compression;
        let compression: Vec<_> = self
            .compression
            .iter()
            .filter(|candidate| other.compression.contains(*candidate))
            .cloned()
            .collect();

        Capabilities {
            version,
            flags,
            max_object_size,
            chunk_size,
            delta_compression,
            compression,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.has_flag("baseline") {
            return Err("missing baseline capability".to_string());
        }
        if self.version == 0 {
            return Err("invalid protocol version".to_string());
        }
        if self.chunk_size == 0 {
            return Err("invalid chunk size".to_string());
        }
        if self.max_object_size == 0 {
            return Err("invalid max object size".to_string());
        }
        if self.compression.is_empty() {
            return Err("no common compression algorithms".to_string());
        }
        Ok(())
    }

    pub fn validate_with_required(&self, required_flags: &[&str]) -> Result<(), String> {
        self.validate()?;

        for flag in required_flags {
            if !self.has_flag(flag) {
                return Err(format!("missing required capability: {flag}"));
            }
        }

        Ok(())
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::new(1)
    }
}

/// A set of capabilities that have been negotiated.
#[derive(Debug, Clone)]
pub struct CapabilitySet {
    pub caps: Capabilities,
    pub valid: bool,
    pub error: Option<String>,
}

impl CapabilitySet {
    pub fn new(client: &Capabilities, server: &Capabilities) -> Self {
        let caps = client.negotiate(server);

        match caps.validate() {
            Ok(()) => Self {
                caps,
                valid: true,
                error: None,
            },
            Err(error) => Self {
                caps,
                valid: false,
                error: Some(error),
            },
        }
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        self.valid && self.caps.has_flag(flag)
    }

    pub fn delta_enabled(&self) -> bool {
        self.valid && self.caps.delta_compression
    }

    pub fn chunk_size(&self) -> usize {
        self.caps.chunk_size as usize
    }

    pub fn max_object_size(&self) -> usize {
        self.caps.max_object_size.min(usize::MAX as u64) as usize
    }

    pub fn chunked_transfer_enabled(&self) -> bool {
        self.has_flag(CAPABILITY_CHUNKED_TRANSFER)
    }

    pub fn resumable_transfer_enabled(&self) -> bool {
        self.has_flag(CAPABILITY_RESUMABLE_TRANSFER)
    }

    pub fn pack_transfer_enabled(&self) -> bool {
        self.has_flag(CAPABILITY_PACK_TRANSFER)
    }

    pub fn partial_fetch_enabled(&self) -> bool {
        self.has_flag(CAPABILITY_PARTIAL_FETCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_default() {
        let caps = Capabilities::default();
        assert!(caps.has_flag("baseline"));
        assert!(caps.delta_compression);
        assert_eq!(caps.version, 1);
    }

    #[test]
    fn test_capabilities_negotiate() {
        let client = Capabilities::new(1)
            .with_flag("fast-import")
            .with_delta(true);
        let server = Capabilities::new(1)
            .with_flag("fast-import")
            .with_flag("server-side-merging")
            .with_delta(true);

        let negotiated = client.negotiate(&server);

        assert!(negotiated.has_flag("baseline"));
        assert!(negotiated.has_flag("fast-import"));
        assert!(!negotiated.has_flag("server-side-merging"));
        assert!(negotiated.delta_compression);
    }

    #[test]
    fn test_capabilities_version_negotiate() {
        let client = Capabilities::new(1);
        let server = Capabilities::new(2);
        let negotiated = client.negotiate(&server);
        assert_eq!(negotiated.version, 1);
    }

    #[test]
    fn test_capability_set() {
        let client = Capabilities::new(1).with_flag("test-feature");
        let server = Capabilities::new(1).with_flag("test-feature");
        let set = CapabilitySet::new(&client, &server);
        assert!(set.valid);
        assert!(set.has_flag("test-feature"));
        assert!(set.has_flag("baseline"));
    }

    #[test]
    fn test_capability_set_invalid() {
        let mut client = Capabilities::new(1);
        client.flags.clear();
        let server = Capabilities::new(1);
        let set = CapabilitySet::new(&client, &server);
        assert!(!set.valid);
        assert!(set.error.is_some());
    }

    #[test]
    fn test_capabilities_validate_required_flags() {
        let caps = Capabilities::new(1).with_flag("refs");
        assert!(caps.validate_with_required(&["refs"]).is_ok());
        assert!(caps.validate_with_required(&["objects"]).is_err());
    }

    #[test]
    fn test_capabilities_validate_limits() {
        let caps = Capabilities::new(1).with_chunk_size(0);
        assert!(caps.validate().is_err());
    }

    #[test]
    fn test_transport_capability_helpers_round_trip() {
        let caps = Capabilities::new(1)
            .with_chunked_transfer(true)
            .with_resumable_transfer(true)
            .with_pack_transfer(true)
            .with_partial_fetch(true);

        assert!(caps.supports_chunked_transfer());
        assert!(caps.supports_resumable_transfer());
        assert!(caps.supports_pack_transfer());
        assert!(caps.supports_partial_fetch());
    }

    #[test]
    fn test_transport_capability_helpers_toggle_off() {
        let caps = Capabilities::new(1)
            .with_chunked_transfer(true)
            .with_chunked_transfer(false)
            .with_resumable_transfer(true)
            .with_resumable_transfer(false);

        assert!(!caps.supports_chunked_transfer());
        assert!(!caps.supports_resumable_transfer());
    }
}