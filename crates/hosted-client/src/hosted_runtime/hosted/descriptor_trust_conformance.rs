//! Cross-repo pin: heddle's consumer path verifies the committed
//! `heddle_api::descriptor_trust::conformance` vector that weft's producer
//! emits. A wrong-version document is rejected fail-closed.

use api::descriptor_trust::{
    AttestedEndpointDescriptorEntry, DescriptorSetError, EndpointDescriptorSetDocument,
    conformance::{
        ATTESTATION_SIGNATURE_HEX, CANONICAL_BYTES_HEX, DIRECT_ADDRESS, EPHEMERAL_KEY_ID,
        EPHEMERAL_PUBLIC_KEY_HEX, NOT_AFTER_UNIX_MILLIS, NOT_BEFORE_UNIX_MILLIS, REGION, RELAY_URL,
        ROOT_PUBLIC_KEY_HEX, SIGNED_DESCRIPTOR_HEX,
    },
    parse_endpoint_descriptor_set, trusted_live_entries, verify_ephemeral_attestation,
};

use super::{HostedError, VerifiedEndpointDescriptor, resolver::fail_if_none_trusted};

fn committed_entry() -> AttestedEndpointDescriptorEntry {
    AttestedEndpointDescriptorEntry {
        ephemeral_key_id: EPHEMERAL_KEY_ID.to_string(),
        ephemeral_public_key: EPHEMERAL_PUBLIC_KEY_HEX.to_string(),
        not_before_unix_millis: NOT_BEFORE_UNIX_MILLIS,
        not_after_unix_millis: NOT_AFTER_UNIX_MILLIS,
        region: REGION.to_string(),
        attestation_signature: ATTESTATION_SIGNATURE_HEX.to_string(),
        signed_descriptor: SIGNED_DESCRIPTOR_HEX.to_string(),
    }
}

fn root_public_key() -> [u8; 32] {
    let decoded = hex::decode(ROOT_PUBLIC_KEY_HEX).expect("committed root public key");
    decoded
        .try_into()
        .expect("committed root public key is 32 bytes")
}

#[test]
fn consumer_path_verifies_the_committed_conformance_vector() {
    let canonical = api::descriptor_trust::ephemeral_attestation_bytes(
        EPHEMERAL_KEY_ID,
        &hex::decode(EPHEMERAL_PUBLIC_KEY_HEX)
            .expect("committed ephemeral public key")
            .try_into()
            .expect("committed ephemeral public key is 32 bytes"),
        NOT_BEFORE_UNIX_MILLIS,
        NOT_AFTER_UNIX_MILLIS,
        REGION,
    );
    assert_eq!(
        hex::encode(&canonical),
        CANONICAL_BYTES_HEX,
        "heddle must hash the same canonical bytes weft signs"
    );

    let entry = committed_entry();
    let body = serde_json::to_vec(&EndpointDescriptorSetDocument {
        version: 1,
        root_key_id: "conformance-root".to_string(),
        entries: vec![entry.clone()],
    })
    .expect("serialize committed set");
    let set = parse_endpoint_descriptor_set(&body).expect("version 1 committed set must parse");
    let root = root_public_key();
    let now = NOT_BEFORE_UNIX_MILLIS;
    let (trusted, rejects) = trusted_live_entries(&set, &root, now);
    fail_if_none_trusted(&trusted, &rejects).expect("committed vector must be dialable");
    assert!(rejects.is_empty());
    assert_eq!(trusted.len(), 1);
    assert_eq!(
        hex::encode(trusted[0].ephemeral_public_key),
        EPHEMERAL_PUBLIC_KEY_HEX
    );
    assert_eq!(
        trusted[0].endpoint_descriptor.direct_addresses,
        vec![DIRECT_ADDRESS]
    );
    assert_eq!(trusted[0].endpoint_descriptor.relay_urls, vec![RELAY_URL]);
    assert_eq!(
        trusted[0].endpoint_descriptor.endpoint_id,
        EPHEMERAL_PUBLIC_KEY_HEX
    );

    let verified = verify_ephemeral_attestation(&root, &entry, now)
        .expect("two-layer verify of the committed vector");
    let descriptor = VerifiedEndpointDescriptor::from_verified_endpoint(&verified, now)
        .expect("dial from the signed descriptor");
    assert_eq!(descriptor.document().endpoint_id, EPHEMERAL_PUBLIC_KEY_HEX);
    assert_eq!(
        descriptor.document().direct_addresses,
        vec![DIRECT_ADDRESS.to_string()]
    );
}

#[test]
fn wrong_version_document_is_rejected_fail_closed() {
    let entry = committed_entry();
    for version in [0_u8, 2, 255] {
        let body = serde_json::to_vec(&EndpointDescriptorSetDocument {
            version,
            root_key_id: "conformance-root".to_string(),
            entries: vec![entry.clone()],
        })
        .expect("serialize wrong-version set");
        assert_eq!(
            parse_endpoint_descriptor_set(&body),
            Err(DescriptorSetError::UnsupportedVersion(version)),
            "version {version} must not parse"
        );
        let set = EndpointDescriptorSetDocument {
            version,
            root_key_id: "conformance-root".to_string(),
            entries: vec![entry.clone()],
        };
        let (trusted, rejects) =
            trusted_live_entries(&set, &root_public_key(), NOT_BEFORE_UNIX_MILLIS);
        assert!(trusted.is_empty(), "version {version} must not be dialed");
        let error =
            fail_if_none_trusted(&trusted, &rejects).expect_err("wrong version fail-closed");
        assert!(
            matches!(
                error,
                HostedError::InvalidDescriptor(_) | HostedError::EndpointDescriptorUnavailable
            ),
            "wrong version must not become a dialable endpoint: {error}"
        );
    }
}

#[test]
fn served_root_key_id_is_never_the_pin() {
    let foreign = [0x41; 32];
    let body = serde_json::to_vec(&EndpointDescriptorSetDocument {
        version: 1,
        root_key_id: ROOT_PUBLIC_KEY_HEX.to_string(),
        entries: vec![committed_entry()],
    })
    .expect("serialize set that names the real root id");
    let set = parse_endpoint_descriptor_set(&body).expect("version 1 parses");
    let (trusted, rejects) = trusted_live_entries(&set, &foreign, NOT_BEFORE_UNIX_MILLIS);
    assert!(trusted.is_empty());
    assert_eq!(
        rejects,
        vec![api::descriptor_trust::EntryReject::InvalidSignature]
    );
    let error = fail_if_none_trusted(&trusted, &rejects).unwrap_err();
    assert!(matches!(error, HostedError::InvalidDescriptorSignature));
}
