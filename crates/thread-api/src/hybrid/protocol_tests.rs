use crate::{Remote, contract::*, rpc, transport::Error};
use api::v2::client::{ClientError, MessageReader, MessageWriter, Rpc, RpcTransport};
use prost::Message;
use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

struct Empty;
#[test]
fn sync_call_context_and_openings_share_the_disabled_cutover_switch() {
    let call_protocol = |method: &api::v2::MethodDescriptor| {
        super::call_protocol(method.path, !method.mandatory_features.is_empty())
    };
    assert_eq!(super::sync_protocol(), None);
    assert_eq!(call_protocol(rpc::SyncServiceFetch::METHOD), None);
    assert_eq!(
        super::call_protocol(rpc::SyncServiceFetch::METHOD.path, true),
        None,
        "API metadata alone cannot activate the coordinated Sync cutover"
    );
    assert_eq!(call_protocol(rpc::SyncServicePublishContent::METHOD), None);
    assert_eq!(call_protocol(rpc::SyncServiceReplicateThread::METHOD), None);
    assert_eq!(
        call_protocol(rpc::IntegrationServicePrepareImportJob::METHOD),
        Some(super::protocol())
    );
    assert_eq!(
        call_protocol(rpc::EndpointServiceDescribeEndpoint::METHOD),
        None
    );
}
#[test]
fn a_complete_public_bundle_has_a_capable_transport_control() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha18.json"))
            .expect("alpha.18 vectors");
    let bytes = hex::decode(
        fixture["wire_vectors"]["complete_renewed_export"]["wire_hex"]
            .as_str()
            .expect("wire bytes"),
    )
    .expect("hex");
    let bundle = ImportPublicProofBundleV1::decode(bytes.as_slice()).expect("complete export");
    let open = ReplicationOpen {
        protocol: Some(super::protocol()),
        import_authority: Some(bundle.clone()),
        ..Default::default()
    };
    super::replication_open(&open).expect("supported complete carrier");
    super::operations(&ReplicationOperations {
        import_authority: Some(bundle),
        ..Default::default()
    })
    .expect("structural complete closure");
    let mut incomplete = open.clone();
    incomplete
        .import_authority
        .as_mut()
        .expect("bundle")
        .genesis_witnesses
        .clear();
    assert!(super::replication_open(&incomplete).is_err());
    let mut old = open.clone();
    old.protocol = None;
    assert!(super::replication_open(&old).is_err());
    super::replication_open(&open).expect("complete capable control remains accepted");
}
impl MessageReader for Empty {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }
    fn cancel(&mut self) {}
}
impl MessageWriter for Empty {
    type Error = Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
        Ok(())
    }
    async fn finish(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}
struct Peer {
    description: DescribeEndpointResponse,
    calls: Arc<AtomicUsize>,
}
impl RpcTransport for Peer {
    type Error = Error;
    type Reader = Empty;
    type Writer = Empty;
    async fn unary(
        &self,
        method: &'static api::v2::MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if method.path == rpc::EndpointServiceDescribeEndpoint::METHOD.path {
            return Ok(self.description.encode_to_vec());
        }
        Ok(PrepareImportJobResponse::default().encode_to_vec())
    }
    async fn observe(
        &self,
        _: &'static api::v2::MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<Empty, Error> {
        Ok(Empty)
    }
    async fn exchange(
        &self,
        _: &'static api::v2::MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(Empty, Empty), Error> {
        Ok((Empty, Empty))
    }
}
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future)
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("immediate fixture"),
    }
}

#[test]
fn discovery_retains_peer_support_and_rejects_old_peers_before_the_gated_call() {
    use api::heddle::api::common::ProtocolCompatibility;
    for (protocol, accepted) in [
        (None, false),
        (Some(ProtocolCompatibility::default()), false),
        (
            Some(ProtocolCompatibility {
                protocol_version: 2,
                mandatory_features: vec![1, 1],
            }),
            false,
        ),
        (
            Some(ProtocolCompatibility {
                protocol_version: 2,
                mandatory_features: vec![1, 2],
            }),
            false,
        ),
        (Some(super::protocol()), true),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = Peer {
            calls: calls.clone(),
            description: DescribeEndpointResponse {
                endpoint: Some(EndpointRef {
                    public_key: vec![7; 32],
                    kind: EndpointKind::Weft as i32,
                }),
                implemented_methods: vec![
                    rpc::IntegrationServicePrepareImportJob::METHOD.path.into(),
                ],
                supported_packages: vec!["heddle.api.v1alpha2".into()],
                protocol,
                ..Default::default()
            },
        };
        let remote = ready(Remote::discover(peer, [7; 32], EndpointKind::Weft))
            .expect("authenticated discovery");
        let result = ready(remote.api.call::<rpc::IntegrationServicePrepareImportJob>(
            &PrepareImportJobRequest {
                client_operation_id: "stable-operation".into(),
                ..Default::default()
            },
        ));
        if accepted {
            result.expect("capable peer");
        } else {
            assert!(matches!(result, Err(ClientError::Protocol(_))));
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            if accepted { 2 } else { 1 },
            "gate must precede request transport"
        );
    }
}

#[test]
fn sync_mandatory_gate_stays_off_and_optional_negotiation_is_exact() {
    const { assert!(!super::SYNC_MANDATORY_GATE) };
    for method in [
        rpc::SyncServiceFetch::METHOD,
        rpc::SyncServicePublishContent::METHOD,
        rpc::SyncServiceReplicateThread::METHOD,
    ] {
        assert!(method.mandatory_features.is_empty());
    }
    super::negotiated(None, None).expect("ordinary Sync");
    super::negotiated(Some(&super::protocol()), Some(&super::protocol()))
        .expect("capable Sync path");
    assert!(super::negotiated(Some(&super::protocol()), None).is_err());
}
