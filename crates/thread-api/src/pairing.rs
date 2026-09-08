//! Provisional pairing possession before an approving account is known.
//! These signatures establish keys, not authority or account membership.
use crypto::{Ed25519Signer, Signer};
use prost::Message;

use crate::{contract::*, transport::Error};

pub const INITIATION_FORMAT: &str = "heddle.pairing-initiation.v1";
const DOMAIN: &[u8] = b"heddle.pairing-initiation.v1\0";
pub const MAX_PAIRING_SECONDS: i64 = 600;

pub fn sign_initiation(
    subject: &impl Signer,
    endpoint: &impl Signer,
    host: EndpointRef,
    operation: String,
    now: i64,
) -> Result<BeginPairingRequest, Error> {
    let binding = PairingInitiationBinding {
        host: Some(host),
        client_operation_id: operation.clone(),
        device: Some(EndpointRef {
            public_key: endpoint.public_key().to_vec(),
            kind: EndpointKind::Device as i32,
        }),
        subject_public_key: subject.public_key().to_vec(),
        not_before_unix_seconds: now,
        expires_at_unix_seconds: now
            .checked_add(MAX_PAIRING_SECONDS)
            .ok_or(Error::Protocol("pairing timestamp overflow"))?,
    };
    let canonical_record = binding.encode_to_vec();
    let payload = [DOMAIN, canonical_record.as_slice()].concat();
    let mut signatures = vec![RecordSignature {
        public_key: subject.public_key().to_vec(),
        signature: subject.sign(&payload).map_err(io)?,
    }];
    if subject.public_key() != endpoint.public_key() {
        signatures.push(RecordSignature {
            public_key: endpoint.public_key().to_vec(),
            signature: endpoint.sign(&payload).map_err(io)?,
        });
    }
    Ok(BeginPairingRequest {
        client_operation_id: operation,
        device: binding.device,
        subject_public_key: binding.subject_public_key,
        subject_possession: Some(SignedRecord {
            format: INITIATION_FORMAT.into(),
            canonical_record,
            signatures,
        }),
    })
}

/// Validate the endpoint and subject against the exact request and serving host.
/// A fresh RPC proof and host-side nonce admission are additionally required.
pub fn verify_initiation(
    request: &BeginPairingRequest,
    host: &EndpointRef,
    now: i64,
) -> Result<PairingInitiationBinding, Error> {
    let record = request
        .subject_possession
        .as_ref()
        .ok_or(Error::Protocol("missing pairing possession"))?;
    if record.format != INITIATION_FORMAT || record.canonical_record.len() > 1024 {
        return Err(Error::Protocol("invalid pairing possession format or size"));
    }
    let binding = PairingInitiationBinding::decode(record.canonical_record.as_slice())?;
    if binding.encode_to_vec() != record.canonical_record
        || binding.host.as_ref() != Some(host)
        || host.public_key.len() != 32
        || host.kind != EndpointKind::Weft as i32
        || binding.client_operation_id != request.client_operation_id
        || binding.client_operation_id.is_empty()
        || binding.client_operation_id.len() > 256
        || binding.subject_public_key != request.subject_public_key
        || binding.subject_public_key.len() != 32
        || binding.device != request.device
        || binding.device.as_ref().is_none_or(|device| {
            device.kind != EndpointKind::Device as i32 || device.public_key.len() != 32
        })
        || binding.not_before_unix_seconds > now.saturating_add(30)
        || binding.expires_at_unix_seconds <= now
        || binding
            .expires_at_unix_seconds
            .saturating_sub(binding.not_before_unix_seconds)
            > MAX_PAIRING_SECONDS
        || binding.expires_at_unix_seconds <= binding.not_before_unix_seconds
    {
        return Err(Error::Protocol(
            "pairing possession binding mismatch or expiry",
        ));
    }
    let device = binding
        .device
        .as_ref()
        .ok_or(Error::Protocol("missing pairing endpoint"))?;
    let keys = if device.public_key == binding.subject_public_key {
        vec![binding.subject_public_key.as_slice()]
    } else {
        vec![
            binding.subject_public_key.as_slice(),
            device.public_key.as_slice(),
        ]
    };
    if record.signatures.len() != keys.len() {
        return Err(Error::Protocol(
            "pairing requires endpoint and subject possession",
        ));
    }
    let payload = [DOMAIN, record.canonical_record.as_slice()].concat();
    for (signature, key) in record.signatures.iter().zip(keys) {
        if signature.public_key != key {
            return Err(Error::Protocol("unexpected pairing possession signer"));
        }
        Ed25519Signer::verify_with_public_key(&payload, key, &signature.signature)
            .map_err(|_| Error::Protocol("invalid pairing possession signature"))?;
    }
    Ok(binding)
}
fn io(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provisional_pairing_binds_both_keys_host_operation_and_deadline() {
        let subject = Ed25519Signer::from_seed(&[31; 32]).expect("subject");
        let endpoint = Ed25519Signer::from_seed(&[32; 32]).expect("endpoint");
        let host = EndpointRef {
            public_key: vec![33; 32],
            kind: EndpointKind::Weft as i32,
        };
        let request = sign_initiation(
            &subject,
            &endpoint,
            host.clone(),
            "pairing-operation".into(),
            1_800_000_000,
        )
        .expect("request");
        verify_initiation(&request, &host, 1_800_000_001).expect("provisional possession");
        let mut missing = request.clone();
        missing
            .subject_possession
            .as_mut()
            .expect("proof")
            .signatures
            .pop();
        assert!(verify_initiation(&missing, &host, 1_800_000_001).is_err());
        let mut wrong = request.clone();
        wrong.subject_possession.as_mut().expect("proof").signatures[1].signature[0] ^= 1;
        assert!(
            verify_initiation(&wrong, &host, 1_800_000_001).is_err(),
            "endpoint signature required"
        );
        let mut other = request.clone();
        other.client_operation_id = "different".into();
        assert!(verify_initiation(&other, &host, 1_800_000_001).is_err());
        let mut other_host = host.clone();
        other_host.public_key[0] ^= 1;
        assert!(verify_initiation(&request, &other_host, 1_800_000_001).is_err());
        assert!(verify_initiation(&request, &host, 1_800_000_600).is_err());
    }
}
