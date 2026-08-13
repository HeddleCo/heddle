// SPDX-License-Identifier: Apache-2.0
//! Wire representation of the offline key-binding registry.

use chrono::{DateTime, Utc};
use objects::object::{ContentHash, KeyBinding, KeyBindingRegistry, StateSignature};
use serde::{Deserialize, Serialize};

use crate::{ObjectData, ObjectId, ObjectType, ProtocolError, Result};

/// Liveness portion of a signed key binding, explicit at the wire boundary.
///
/// The conversion back to the object model restores `revoked_at` before any
/// signature verification, so the fields covered by `added_by_sig` remain
/// byte-for-byte equivalent to the source binding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireKeyBindingLiveness {
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Transport form of a [`KeyBinding`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireKeyBinding {
    pub algorithm: String,
    pub public_key: String,
    pub identity_ref: String,
    pub role: String,
    pub added_by_sig: StateSignature,
    pub valid_from: DateTime<Utc>,
    pub delegated_from: Option<ContentHash>,
    pub liveness: WireKeyBindingLiveness,
}

/// Versioned set of key bindings carried as one closure object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireKeyBindingRegistry {
    pub format_version: u8,
    pub epoch: u64,
    pub previous_registry: Option<ContentHash>,
    pub authority_signature: StateSignature,
    pub bindings: Vec<WireKeyBinding>,
}

impl From<&KeyBinding> for WireKeyBinding {
    fn from(binding: &KeyBinding) -> Self {
        Self {
            algorithm: binding.algorithm.clone(),
            public_key: binding.public_key.clone(),
            identity_ref: binding.identity_ref.clone(),
            role: binding.role.clone(),
            added_by_sig: binding.added_by_sig.clone(),
            valid_from: binding.valid_from,
            delegated_from: binding.delegated_from,
            liveness: WireKeyBindingLiveness {
                revoked_at: binding.revoked_at,
            },
        }
    }
}

impl From<WireKeyBinding> for KeyBinding {
    fn from(binding: WireKeyBinding) -> Self {
        Self {
            algorithm: binding.algorithm,
            public_key: binding.public_key,
            identity_ref: binding.identity_ref,
            role: binding.role,
            added_by_sig: binding.added_by_sig,
            valid_from: binding.valid_from,
            revoked_at: binding.liveness.revoked_at,
            delegated_from: binding.delegated_from,
        }
    }
}

impl From<&KeyBindingRegistry> for WireKeyBindingRegistry {
    fn from(registry: &KeyBindingRegistry) -> Self {
        Self {
            format_version: registry.format_version,
            epoch: registry.epoch,
            previous_registry: registry.previous_registry,
            authority_signature: registry.authority_signature.clone(),
            bindings: registry.bindings.iter().map(WireKeyBinding::from).collect(),
        }
    }
}

impl From<WireKeyBindingRegistry> for KeyBindingRegistry {
    fn from(registry: WireKeyBindingRegistry) -> Self {
        Self {
            format_version: registry.format_version,
            epoch: registry.epoch,
            previous_registry: registry.previous_registry,
            authority_signature: registry.authority_signature,
            bindings: registry
                .bindings
                .into_iter()
                .map(KeyBinding::from)
                .collect(),
        }
    }
}

/// Encode a validated registry as a typed, content-addressed wire object.
pub fn encode_key_binding_registry(registry: &KeyBindingRegistry) -> Result<ObjectData> {
    registry
        .validate()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let id = registry
        .content_hash()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let wire = WireKeyBindingRegistry::from(registry);
    Ok(ObjectData {
        id: ObjectId::Hash(id),
        obj_type: ObjectType::KeyBinding,
        data: rmp_serde::to_vec_named(&wire)?,
        is_delta: false,
    })
}

/// Decode a key-binding wire object and verify its advertised content address.
pub fn decode_key_binding_registry(data: &ObjectData) -> Result<KeyBindingRegistry> {
    let (ObjectId::Hash(expected), ObjectType::KeyBinding) = (&data.id, data.obj_type) else {
        return Err(ProtocolError::InvalidState(
            "object id/type mismatch for key-binding registry".to_string(),
        ));
    };
    if data.is_delta {
        return Err(ProtocolError::InvalidState(
            "key-binding registry objects cannot be delta encoded".to_string(),
        ));
    }
    let wire: WireKeyBindingRegistry = rmp_serde::from_slice(&data.data)?;
    let registry = KeyBindingRegistry::from(wire);
    registry
        .validate()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let actual = registry
        .content_hash()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    if actual != *expected {
        return Err(ProtocolError::InvalidState(format!(
            "key-binding registry hash mismatch: expected {}, computed {}",
            expected.to_hex(),
            actual.to_hex()
        )));
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use objects::object::{ContentHash, KeyBinding, KeyBindingRegistry, StateSignature};

    use super::{WireKeyBinding, decode_key_binding_registry, encode_key_binding_registry};
    use crate::{ObjectData, ObjectType};

    #[test]
    fn key_binding_registry_roundtrips_with_liveness_overlay_losslessly() {
        let binding = KeyBinding {
            algorithm: "ed25519".to_string(),
            public_key: "11".repeat(32),
            identity_ref: "user:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_string(),
            role: "author".to_string(),
            added_by_sig: StateSignature {
                algorithm: "ed25519".to_string(),
                public_key: "22".repeat(32),
                signature: "33".repeat(64),
            },
            valid_from: Utc.timestamp_opt(1_725_000_000, 123_456_789).unwrap(),
            revoked_at: Some(Utc.timestamp_opt(1_735_000_000, 987_654_321).unwrap()),
            delegated_from: Some(ContentHash::from_bytes([0x44; 32])),
        };
        let wire_binding = WireKeyBinding::from(&binding);
        assert_eq!(wire_binding.liveness.revoked_at, binding.revoked_at);
        assert_eq!(KeyBinding::from(wire_binding), binding);
        let registry = KeyBindingRegistry::new(
            1,
            Some(ContentHash::from_bytes([0x55; 32])),
            StateSignature {
                algorithm: "ed25519".to_string(),
                public_key: "66".repeat(32),
                signature: "77".repeat(64),
            },
            vec![binding],
        );

        let wire = encode_key_binding_registry(&registry).expect("encode wire registry");
        assert_eq!(wire.obj_type, ObjectType::KeyBinding);
        assert_eq!(ObjectType::KeyBinding.wire_name(), "key_binding");
        assert_eq!(
            ObjectType::from_wire("key_binding").expect("parse key-binding type"),
            ObjectType::KeyBinding
        );
        let encoded_frame = rmp_serde::to_vec_named(&wire).expect("encode object frame");
        let transferred: ObjectData =
            rmp_serde::from_slice(&encoded_frame).expect("decode object frame");
        let decoded = decode_key_binding_registry(&transferred).expect("decode wire registry");

        assert_eq!(decoded, registry);
        assert_eq!(
            decoded.content_hash().unwrap(),
            registry.content_hash().unwrap()
        );
    }

    #[test]
    fn key_binding_registry_rejects_a_mismatched_content_address() {
        let registry = KeyBindingRegistry::new(
            0,
            None,
            StateSignature {
                algorithm: "ed25519".to_string(),
                public_key: "66".repeat(32),
                signature: "77".repeat(64),
            },
            Vec::new(),
        );
        let mut wire = encode_key_binding_registry(&registry).expect("encode wire registry");
        wire.id = crate::ObjectId::Hash(ContentHash::from_bytes([0x55; 32]));

        let error = decode_key_binding_registry(&wire).expect_err("hash mismatch must fail closed");
        assert!(error.to_string().contains("hash mismatch"));
    }
}
