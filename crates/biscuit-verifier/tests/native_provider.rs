#![cfg(feature = "native-provider")]

use chrono::{TimeZone, Utc};
use heddle_api::{
    heddle::api::v1alpha2::{
        EndpointKind, EndpointRef, ObjectAddress, ProviderAssemblyRecord, ProviderExtent,
        ProviderPhysicalRange, ProviderPlan, ProviderPlanChallenge, ProviderRangeSource,
        ProviderReadTicket, ReadProviderExtentRequest, RevisionRef, SpoolRef, ThreadId, ThreadRef,
        TransferObject, provider_assembly_record, revision_ref,
    },
    provider_v2::{
        provider_assembly_digest, provider_extent_set_digest, provider_record_set_commitment,
    },
};
use heddle_biscuit_verifier::edge::{EdgeError, authorize_native_provider_extent};

fn endpoint(kind: EndpointKind, byte: u8) -> EndpointRef {
    EndpointRef {
        kind: kind as i32,
        public_key: vec![byte; 32],
    }
}

fn plan() -> ProviderPlan {
    let spool = SpoolRef {
        id: "123e4567-e89b-12d3-a456-426614174000".into(),
    };
    let provider = endpoint(EndpointKind::Provider, 4);
    let client = endpoint(EndpointKind::Device, 3);
    let expiry = prost_types::Timestamp {
        seconds: 1_800_000_000,
        nanos: 500,
    };
    let records = vec![ProviderAssemblyRecord {
        object: Some(TransferObject {
            address: Some(ObjectAddress {
                algorithm: "blake3".into(),
                digest: vec![8; 32],
            }),
            kind: "blob".into(),
            facet: 1,
            size: 8,
            availability: 4,
        }),
        encoded_length: 8,
        encoded_digest: Some(ObjectAddress {
            algorithm: "blake3".into(),
            digest: vec![9; 32],
        }),
        output_offset: 16,
        source: Some(provider_assembly_record::Source::Provider(
            ProviderRangeSource {
                extent_index: 0,
                source_offset: 0,
            },
        )),
    }];
    let mut range = ProviderPhysicalRange {
        pack_id: vec![5; 32],
        object_etag: "etag-1".into(),
        offset: 128,
        length: 8,
        record_set_commitment: vec![],
    };
    range.record_set_commitment = provider_record_set_commitment(&range, &records, 0)
        .expect("tiled fixture range")
        .to_vec();
    let ticket = ProviderReadTicket {
        attenuated_capability: b"not-a-token".to_vec(),
        extent_set_digest: vec![],
        spool: Some(spool.clone()),
        facet: 1,
        audience: "public".into(),
        content_root: vec![6; 32],
        pack_id: range.pack_id.clone(),
        object_etag: range.object_etag.clone(),
        offset: range.offset,
        length: range.length,
        provider: Some(provider.clone()),
        client: Some(client.clone()),
        assembly_digest: vec![],
        expires_at: Some(expiry),
        record_set_commitment: range.record_set_commitment.clone(),
    };
    let mut header = b"LMPK".to_vec();
    header.extend_from_slice(&4_u32.to_be_bytes());
    header.extend_from_slice(&1_u64.to_be_bytes());
    let mut plan = ProviderPlan {
        extent_set_digest: vec![],
        extents: vec![ProviderExtent {
            provider: Some(provider),
            ticket: Some(ticket),
            range: Some(range),
        }],
        challenge: Some(ProviderPlanChallenge {
            nonce: vec![7; 16],
            thread: Some(ThreadRef {
                spool: Some(spool.clone()),
                id: Some(ThreadId { value: vec![1; 32] }),
            }),
            revision: Some(RevisionRef {
                spool: Some(spool),
                revision: Some(revision_ref::Revision::State(
                    heddle_api::heddle::api::common::StateId { value: vec![2; 32] },
                )),
            }),
            issuer: Some(endpoint(EndpointKind::Weft, 2)),
            client: Some(client),
            expires_at: Some(expiry),
            extent_set_digest: vec![],
            assembly_digest: vec![],
        }),
        assembly_digest: vec![],
        pack_header: header,
        output_pack_length: 56,
        records,
    };
    let set = provider_extent_set_digest(&plan).expect("extent-set commitment");
    plan.extent_set_digest = set.to_vec();
    plan.challenge
        .as_mut()
        .expect("challenge")
        .extent_set_digest = set.to_vec();
    plan.extents[0]
        .ticket
        .as_mut()
        .expect("ticket")
        .extent_set_digest = set.to_vec();
    let assembly = provider_assembly_digest(&plan).expect("assembly commitment");
    plan.assembly_digest = assembly.to_vec();
    plan.challenge.as_mut().expect("challenge").assembly_digest = assembly.to_vec();
    plan.extents[0]
        .ticket
        .as_mut()
        .expect("ticket")
        .assembly_digest = assembly.to_vec();
    plan
}

#[test]
fn native_read_requires_retained_exact_range_and_transport_peers_before_biscuit() {
    let plan = plan();
    let ticket = plan.extents[0].ticket.clone().expect("ticket");
    let range = plan.extents[0].range.clone().expect("range");
    let mut request = ReadProviderExtentRequest {
        ticket: Some(ticket),
        extent_set_digest: plan.extent_set_digest.clone(),
        range: Some(range),
    };
    let now = Utc.timestamp_opt(1_700_000_000, 0).single().expect("time");
    let denied = |request: &ReadProviderExtentRequest,
                  client: &[u8; 32],
                  provider: &[u8; 32],
                  root: &[u8; 32]| {
        authorize_native_provider_extent(
            None,
            &[],
            &[],
            &plan,
            request,
            client,
            provider,
            root,
            now,
        )
    };

    assert!(
        matches!(denied(&request, &[2; 32], &[4; 32], &[6; 32]), Err(EdgeError::Unauthorized(message)) if message.contains("client peer"))
    );
    assert!(
        matches!(denied(&request, &[3; 32], &[5; 32], &[6; 32]), Err(EdgeError::Unauthorized(message)) if message.contains("plan binding"))
    );
    assert!(
        matches!(denied(&request, &[3; 32], &[4; 32], &[7; 32]), Err(EdgeError::Unauthorized(message)) if message.contains("plan binding"))
    );
    request.range.as_mut().expect("range").offset += 1;
    assert!(
        matches!(denied(&request, &[3; 32], &[4; 32], &[6; 32]), Err(EdgeError::Unauthorized(message)) if message.contains("absent from retained plan"))
    );
    request.range = plan.extents[0].range.clone();
    let at_expiry = Utc
        .timestamp_opt(1_800_000_000, 500)
        .single()
        .expect("expiry");
    assert!(matches!(
        authorize_native_provider_extent(
            None,
            &[],
            &[],
            &plan,
            &request,
            &[3; 32],
            &[4; 32],
            &[6; 32],
            at_expiry,
        ),
        Err(EdgeError::Unauthorized(message)) if message.contains("expired")
    ));
}
