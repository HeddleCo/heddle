//! Pairing proves each actual receiver key, never independent account authority.
use crypto::{Ed25519Signer, Signer};
use prost::Message;

use crate::{contract::*, transport::Error};

pub const INITIATION_FORMAT: &str = "heddle.pairing-initiation.v2";
const DOMAIN: &[u8] = b"heddle.pairing-initiation.v2\0";
pub const BROWSER_COMPLETION_FORMAT: &str = "heddle.browser-pairing-completion.v1";
const BROWSER_DOMAIN: &[u8] = b"heddle.browser-pairing-completion.v1\0";
pub const MAX_PAIRING_SECONDS: i64 = 600;

pub fn sign_initiation(
    subject: &impl Signer,
    endpoint: &impl Signer,
    host: EndpointRef,
    operation: String,
    now: i64,
) -> Result<BeginPairingRequest, Error> {
    let binding = initiation_binding(
        subject,
        host,
        operation,
        now,
        pairing_initiation_binding::Receiver::Device(EndpointRef {
            public_key: endpoint.public_key().to_vec(),
            kind: EndpointKind::Device as i32,
        }),
    )?;
    sign_initiation_binding(binding, subject, Some(endpoint))
}
/// Browser receivers retain only a credential key, with no fabricated Iroh endpoint.
pub fn sign_browser_initiation(
    subject: &impl Signer,
    host: EndpointRef,
    operation: String,
    now: i64,
) -> Result<BeginPairingRequest, Error> {
    let binding = initiation_binding(
        subject,
        host,
        operation,
        now,
        pairing_initiation_binding::Receiver::Browser(BrowserPairingReceiver {}),
    )?;
    sign_initiation_binding(binding, subject, None::<&Ed25519Signer>)
}
fn initiation_binding(
    subject: &impl Signer,
    host: EndpointRef,
    operation: String,
    now: i64,
    receiver: pairing_initiation_binding::Receiver,
) -> Result<PairingInitiationBinding, Error> {
    Ok(PairingInitiationBinding {
        host: Some(host),
        client_operation_id: operation,
        receiver: Some(receiver),
        subject_public_key: subject.public_key().to_vec(),
        not_before_unix_seconds: now,
        expires_at_unix_seconds: now
            .checked_add(MAX_PAIRING_SECONDS)
            .ok_or(Error::Protocol("pairing timestamp overflow"))?,
    })
}
fn sign_initiation_binding(
    binding: PairingInitiationBinding,
    subject: &impl Signer,
    endpoint: Option<&impl Signer>,
) -> Result<BeginPairingRequest, Error> {
    let canonical_record = initiation_bytes(&binding);
    let payload = [DOMAIN, canonical_record.as_slice()].concat();
    let mut signatures = vec![RecordSignature {
        public_key: subject.public_key().to_vec(),
        signature: subject.sign(&payload).map_err(io)?,
    }];
    if let Some(endpoint) = endpoint
        && endpoint.public_key() != subject.public_key()
    {
        signatures.push(RecordSignature {
            public_key: endpoint.public_key().to_vec(),
            signature: endpoint.sign(&payload).map_err(io)?,
        });
    }
    let receiver = binding.receiver.map(|receiver| match receiver {
        pairing_initiation_binding::Receiver::Device(value) => {
            begin_pairing_request::Receiver::Device(value)
        }
        pairing_initiation_binding::Receiver::Browser(value) => {
            begin_pairing_request::Receiver::Browser(value)
        }
    });
    Ok(BeginPairingRequest {
        client_operation_id: binding.client_operation_id,
        receiver,
        subject_public_key: binding.subject_public_key,
        subject_possession: Some(SignedRecord {
            format: INITIATION_FORMAT.into(),
            canonical_record,
            signatures,
        }),
    })
}
/// Normative signed format: ascending protobuf tags, preserving oneof presence.
/// Prost emits a oneof at its declaration position whereas protobuf-es emits
/// tag order; ordinary RPC bodies need not agree, but signed records must.
pub fn initiation_bytes(binding: &PairingInitiationBinding) -> Vec<u8> {
    use prost::encoding::{bytes, int64, message, string};
    let mut encoded = Vec::new();
    if let Some(host) = &binding.host {
        message::encode(1, host, &mut encoded);
    }
    if !binding.client_operation_id.is_empty() {
        string::encode(2, &binding.client_operation_id, &mut encoded);
    }
    if let Some(pairing_initiation_binding::Receiver::Device(device)) = &binding.receiver {
        message::encode(3, device, &mut encoded);
    }
    if !binding.subject_public_key.is_empty() {
        bytes::encode(4, &binding.subject_public_key, &mut encoded);
    }
    if binding.not_before_unix_seconds != 0 {
        int64::encode(5, &binding.not_before_unix_seconds, &mut encoded);
    }
    if binding.expires_at_unix_seconds != 0 {
        int64::encode(6, &binding.expires_at_unix_seconds, &mut encoded);
    }
    if let Some(pairing_initiation_binding::Receiver::Browser(browser)) = &binding.receiver {
        message::encode(7, browser, &mut encoded);
    }
    encoded
}

/// A fresh exact RPC proof and host-side nonce admission remain mandatory.
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
    let receiver = request.receiver.as_ref().map(|receiver| match receiver {
        begin_pairing_request::Receiver::Device(value) => {
            pairing_initiation_binding::Receiver::Device(value.clone())
        }
        begin_pairing_request::Receiver::Browser(value) => {
            pairing_initiation_binding::Receiver::Browser(*value)
        }
    });
    if initiation_bytes(&binding) != record.canonical_record
        || binding.host.as_ref() != Some(host)
        || host.public_key.len() != 32
        || host.kind != EndpointKind::Weft as i32
        || binding.client_operation_id != request.client_operation_id
        || binding.client_operation_id.is_empty()
        || binding.client_operation_id.len() > 256
        || binding.subject_public_key != request.subject_public_key
        || binding.subject_public_key.len() != 32
        || binding.receiver != receiver
        || binding.not_before_unix_seconds > now.saturating_add(30)
        || binding.not_before_unix_seconds <= 0
        || binding.expires_at_unix_seconds <= now
        || binding.expires_at_unix_seconds <= binding.not_before_unix_seconds
        || binding
            .expires_at_unix_seconds
            .saturating_sub(binding.not_before_unix_seconds)
            > MAX_PAIRING_SECONDS
    {
        return Err(Error::Protocol(
            "pairing possession binding mismatch or expiry",
        ));
    }
    let mut keys = vec![binding.subject_public_key.as_slice()];
    match binding.receiver.as_ref() {
        Some(pairing_initiation_binding::Receiver::Device(device))
            if device.kind == EndpointKind::Device as i32 && device.public_key.len() == 32 =>
        {
            if device.public_key != binding.subject_public_key {
                keys.push(device.public_key.as_slice());
            }
        }
        Some(pairing_initiation_binding::Receiver::Browser(_)) => {}
        _ => return Err(Error::Protocol("invalid pairing receiver")),
    }
    verify_signatures(record, DOMAIN, &keys)?;
    Ok(binding)
}

pub fn sign_browser_completion(
    subject: &impl Signer,
    host: EndpointRef,
    operation: String,
    pairing: RecordRef,
    approval: BrowserPairingApprovalBinding,
) -> Result<CompletePairingRequest, Error> {
    validate_browser_approval(&approval)?;
    if subject.public_key() != approval.subject_public_key {
        return Err(Error::Protocol("browser subject differs from approval"));
    }
    let binding = BrowserPairingCompletionBinding {
        host: Some(host),
        client_operation_id: operation.clone(),
        pairing: Some(pairing.clone()),
        approval: Some(approval),
    };
    let canonical_record = binding.encode_to_vec();
    let payload = [BROWSER_DOMAIN, canonical_record.as_slice()].concat();
    Ok(CompletePairingRequest {
        client_operation_id: operation,
        pairing: Some(pairing),
        proof: Some(complete_pairing_request::Proof::BrowserPossession(
            SignedRecord {
                format: BROWSER_COMPLETION_FORMAT.into(),
                canonical_record,
                signatures: vec![RecordSignature {
                    public_key: subject.public_key().to_vec(),
                    signature: subject.sign(&payload).map_err(io)?,
                }],
            },
        )),
    })
}
/// Verify only subject possession over the exact stored approval. The host must
/// separately reverify the private derived Biscuit, account, revocations and expiry.
/// This creates neither an endpoint attachment nor an independent mint issuer.
pub fn verify_browser_completion(
    request: &CompletePairingRequest,
    host: &EndpointRef,
    approval: &BrowserPairingApprovalBinding,
    now: i64,
) -> Result<BrowserPairingCompletionBinding, Error> {
    validate_browser_approval(approval)?;
    let Some(complete_pairing_request::Proof::BrowserPossession(record)) = request.proof.as_ref()
    else {
        return Err(Error::Protocol(
            "browser completion requires subject possession",
        ));
    };
    if record.format != BROWSER_COMPLETION_FORMAT || record.canonical_record.len() > 2048 {
        return Err(Error::Protocol("invalid browser completion format or size"));
    }
    let binding = BrowserPairingCompletionBinding::decode(record.canonical_record.as_slice())?;
    if binding.encode_to_vec() != record.canonical_record
        || binding.host.as_ref() != Some(host)
        || host.public_key.len() != 32
        || host.kind != EndpointKind::Weft as i32
        || binding.client_operation_id != request.client_operation_id
        || request.client_operation_id.is_empty()
        || request.client_operation_id.len() > 256
        || binding.pairing != request.pairing
        || request.pairing.as_ref().is_none_or(|reference| {
            reference.spool.is_some()
                || uuid::Uuid::parse_str(&reference.id).map_or(true, |id| id.is_nil())
        })
        || binding.approval.as_ref() != Some(approval)
        || approval.not_before_unix_seconds > now
        || approval.expires_at_unix_seconds <= now
    {
        return Err(Error::Protocol(
            "browser completion does not match current pairing approval",
        ));
    }
    verify_signatures(
        record,
        BROWSER_DOMAIN,
        &[approval.subject_public_key.as_slice()],
    )?;
    Ok(binding)
}
fn validate_browser_approval(approval: &BrowserPairingApprovalBinding) -> Result<(), Error> {
    if approval.format_version != 1
        || uuid::Uuid::parse_str(&approval.account_id).map_or(true, |id| id.is_nil())
        || approval.root_public_key.len() != 32
        || approval.subject_public_key.len() != 32
        || approval.credential_digest.len() != 32
        || approval.pairing_challenge.len() != 32
        || approval.not_before_unix_seconds <= 0
        || approval.expires_at_unix_seconds <= approval.not_before_unix_seconds
    {
        return Err(Error::Protocol("invalid browser pairing approval"));
    }
    Ok(())
}
fn verify_signatures(record: &SignedRecord, domain: &[u8], keys: &[&[u8]]) -> Result<(), Error> {
    if record.signatures.len() != keys.len() {
        return Err(Error::Protocol("pairing requires each actual receiver key"));
    }
    let payload = [domain, record.canonical_record.as_slice()].concat();
    for (signature, key) in record.signatures.iter().zip(keys) {
        if signature.public_key != *key {
            return Err(Error::Protocol("unexpected pairing possession signer"));
        }
        Ed25519Signer::verify_with_public_key(&payload, key, &signature.signature)
            .map_err(|_| Error::Protocol("invalid pairing possession signature"))?;
    }
    Ok(())
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
    #[test]
    fn browser_receiver_proves_only_its_key_and_exact_approved_credential() {
        let subject = Ed25519Signer::from_seed(&[41; 32]).expect("browser key");
        let host = EndpointRef {
            public_key: vec![42; 32],
            kind: EndpointKind::Weft as i32,
        };
        let now = 1_800_000_000;
        let start = sign_browser_initiation(&subject, host.clone(), "browser-start".into(), now)
            .expect("browser initiation");
        let binding = verify_initiation(&start, &host, now).expect("browser possession");
        assert!(matches!(
            binding.receiver,
            Some(pairing_initiation_binding::Receiver::Browser(_))
        ));
        assert_eq!(
            start
                .subject_possession
                .as_ref()
                .expect("proof")
                .signatures
                .len(),
            1
        );
        let mut substituted = start.clone();
        substituted.receiver = Some(begin_pairing_request::Receiver::Device(EndpointRef {
            public_key: subject.public_key().to_vec(),
            kind: EndpointKind::Device as i32,
        }));
        assert!(
            verify_initiation(&substituted, &host, now).is_err(),
            "browser key cannot acquire a fabricated endpoint binding"
        );
        let approval = BrowserPairingApprovalBinding {
            format_version: 1,
            root_public_key: vec![43; 32],
            subject_public_key: subject.public_key().to_vec(),
            account_id: "00000000-0000-0000-0000-000000000044".into(),
            credential_digest: vec![45; 32],
            pairing_challenge: vec![46; 32],
            not_before_unix_seconds: now,
            expires_at_unix_seconds: now + 3600,
        };
        let request = sign_browser_completion(
            &subject,
            host.clone(),
            "browser-complete".into(),
            RecordRef {
                spool: None,
                id: "00000000-0000-0000-0000-000000000047".into(),
            },
            approval.clone(),
        )
        .expect("browser completion");
        verify_browser_completion(&request, &host, &approval, now)
            .expect("current subject accepts exact approval");
        let mut changed = approval.clone();
        changed.account_id = "00000000-0000-0000-0000-000000000048".into();
        assert!(
            verify_browser_completion(&request, &host, &changed, now).is_err(),
            "approval must bind the exact account even when keys match"
        );
        let mut changed = request.clone();
        changed.pairing.as_mut().expect("pairing").id =
            "00000000-0000-0000-0000-000000000048".into();
        assert!(verify_browser_completion(&changed, &host, &approval, now).is_err());
        let mut changed_host = host.clone();
        changed_host.public_key[0] ^= 1;
        assert!(verify_browser_completion(&request, &changed_host, &approval, now).is_err());
        assert!(verify_browser_completion(&request, &host, &approval, now + 3600).is_err());
        let vector = format!(
            "initiation={}\ncompletion={}\n",
            hex::encode(start.encode_to_vec()),
            hex::encode(request.encode_to_vec())
        );
        assert_eq!(
            vector,
            include_str!("../tests/fixtures/browser_pairing_v1.txt")
        );
    }
}
