//! The assembled CLI client must accept the native contract's source frames.
use std::net::Ipv4Addr;

use api::{
    framing::{decode_request_frame, encode_stream_message, encode_success_response},
    heddle::api::v1alpha2 as v2,
};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;

use super::{CallContextFactory, HostedClient};

#[tokio::test]
async fn mandatory_feature_call_has_no_protocol_field_11_on_the_wire() {
    #[derive(prost::Message)]
    struct RemovedProtocolField {
        #[prost(message, optional, tag = "11")]
        protocol: Option<api::heddle::api::common::ProtocolCompatibility>,
    }

    let _guard = crate::test_process_env::shared().await;
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("server address")
        .bind()
        .await
        .expect("server");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let method = "/heddle.api.v1alpha2.IntegrationService/PrepareImportJob";
    assert!(
        !api::method_descriptor(method)
            .expect("method")
            .mandatory_features
            .is_empty()
    );
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("endpoint negotiation");
        let bytes = recv.read_to_end(65536).await.expect("request");
        assert_eq!(
            decode_request_frame(&bytes).expect("frame").method,
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint"
        );
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key,
            }),
            supported_packages: vec!["heddle.api.v1alpha2".into()],
            implemented_methods: vec![method.into()],
            protocol: Some(thread_api::hybrid::protocol()),
            ..Default::default()
        };
        send.write_all(&encode_success_response(&description.encode_to_vec()).expect("frame"))
            .await
            .expect("description");
        send.finish().expect("FIN");
        let (mut send, mut recv) = connection.accept_bi().await.expect("gated call");
        let bytes = recv.read_to_end(65536).await.expect("call bytes");
        assert_eq!(decode_request_frame(&bytes).expect("frame").method, method);
        let method_len = usize::from(u16::from_be_bytes(bytes[..2].try_into().expect("length")));
        let context_len = u32::from_be_bytes(bytes[2..6].try_into().expect("length")) as usize;
        // Inspect the original context bytes: decoding and re-encoding through
        // the current CallContext would silently discard an unknown field 11.
        let context = &bytes[6 + method_len..6 + method_len + context_len];
        assert!(
            RemovedProtocolField::decode(context)
                .expect("context wire")
                .protocol
                .is_none()
        );
        send.write_all(
            &encode_success_response(&v2::PrepareImportJobResponse::default().encode_to_vec())
                .expect("frame"),
        )
        .await
        .expect("response");
        send.finish().expect("FIN");
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("hosted client");
    let _: v2::PrepareImportJobResponse = client
        .call_unary(
            method,
            &v2::PrepareImportJobRequest {
                client_operation_id: "wire-field-11".into(),
                ..Default::default()
            },
        )
        .await
        .expect("negotiated gated call");
    client.close().await;
    task.await.expect("server finished");
}

#[tokio::test]
async fn native_client_accepts_source_frames_above_legacy_control_limit() {
    let _process_env_guard = crate::test_process_env::shared().await;
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("server bind address")
        .bind()
        .await
        .expect("server endpoint");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let payload = vec![7; 500 * 1024];
    let expected = payload.clone();
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("discovery");
        let request = recv
            .read_to_end(1024 * 1024)
            .await
            .expect("discovery request");
        assert_eq!(
            decode_request_frame(&request).expect("request").method,
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint"
        );
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key,
            }),
            supported_packages: vec!["heddle.api.v1alpha2".into()],
            implemented_methods: vec!["/heddle.api.v1alpha2.ContentService/ReadContent".into()],
            ..Default::default()
        };
        send.write_all(
            &encode_success_response(&description.encode_to_vec()).expect("description frame"),
        )
        .await
        .expect("send discovery");
        send.finish().expect("discovery FIN");
        let (mut send, mut recv) = connection.accept_bi().await.expect("content read");
        let request = recv
            .read_to_end(1024 * 1024)
            .await
            .expect("content request");
        assert_eq!(
            decode_request_frame(&request).expect("request").method,
            "/heddle.api.v1alpha2.ContentService/ReadContent"
        );
        let event = v2::ContentEvent {
            payload: Some(v2::content_event::Payload::Blob(v2::BlobChunk {
                data: payload,
                ..Default::default()
            })),
            ..Default::default()
        };
        send.write_all(&encode_stream_message(&event.encode_to_vec()).expect("content frame"))
            .await
            .expect("send native source frame");
        send.finish().expect("content FIN");
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client endpoint");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("assembled hosted client");
    let remote = client.native().await.expect("native discovery");
    let mut messages = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&v2::ReadContentRequest::default())
        .await
        .expect("content stream");
    let event = messages
        .next()
        .await
        .expect("native frame within supported budget")
        .expect("content");
    let Some(v2::content_event::Payload::Blob(chunk)) = event.payload else {
        panic!("blob chunk");
    };
    assert_eq!(chunk.data, expected);
    drop(messages);
    drop(remote);
    client.close().await;
    task.await.expect("server finished");
}

// The peer holds a once observation open at each seam, including the next page.
async fn stalled_collaboration_peer(
    stall: usize,
) -> (
    HostedClient,
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind")
        .bind()
        .await
        .expect("server");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let (release, mut released) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("discovery");
        recv.read_to_end(65536).await.expect("discovery request");
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key.clone(),
            }),
            supported_packages: vec!["heddle.api.v1alpha2".into()],
            implemented_methods: vec![
                "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration".into(),
            ],
            default_read_budget: Some(v2::ReadBudget {
                max_items: 64,
                max_frame_bytes: 65536,
                max_snapshot_bytes: 1048576,
            }),
            max_pending_batch_bytes: 1048576,
            ..Default::default()
        };
        send.write_all(
            &encode_success_response(&description.encode_to_vec()).expect("description"),
        )
        .await
        .expect("discovery response");
        send.finish().expect("FIN");
        for page in 0..=1 {
            let (mut send, mut recv) = connection.accept_bi().await.expect("observation");
            let bytes = recv.read_to_end(65536).await.expect("request");
            let request = v2::ObserveCollaborationRequest::decode(
                decode_request_frame(&bytes).expect("frame").body,
            )
            .expect("observation request");
            if page == 1 {
                assert_eq!(request.page.expect("page").after_page, vec![7]);
            }
            let bodies = [
                v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(v2::EndpointRef {
                        kind: v2::EndpointKind::Weft as i32,
                        public_key: key.clone(),
                    }),
                    binding_digest: vec![9; 32],
                    accepted_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 65536,
                        max_snapshot_bytes: 1048576,
                    }),
                    ..Default::default()
                }),
                v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
                    cursor: vec![1],
                    snapshot_complete: true,
                    page: Some(v2::PageInfo {
                        exhausted: stall != 3,
                        next_page: if stall == 3 { vec![7] } else { vec![] },
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                v2::stream_frame::Body::Complete(v2::StreamComplete { cursor: vec![1] }),
            ];
            for (index, body) in bodies.into_iter().enumerate() {
                if (stall < 3 && index == stall) || (stall == 3 && page == 1 && index == 0) {
                    // Keep the connection and response stream alive until the
                    // test releases it; absence of Complete must be bounded.
                    let _ = (&mut released).await;
                }
                let event = v2::CollaborationEvent {
                    frame: Some(v2::StreamFrame {
                        sequence: index as u64 + 1,
                        body: Some(body),
                    }),
                    ..Default::default()
                };
                if send
                    .write_all(&encode_stream_message(&event.encode_to_vec()).expect("frame"))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = send.finish();
            if stall != 3 {
                break;
            }
        }
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind")
        .bind()
        .await
        .expect("client");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("hosted client");
    client.native().await.expect("discovery");
    (client, task, release)
}

async fn assert_once_observation_stall_is_bounded(stall: usize) {
    let _guard = crate::test_process_env::shared().await;
    let (client, server, _release) = stalled_collaboration_peer(stall).await;
    let request = v2::ObserveCollaborationRequest {
        observe: Some(v2::ObserveOptions {
            mode: v2::ObservationMode::Once as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.observe_collaboration_events_with_timeout(
            request,
            std::time::Duration::from_millis(100),
        ),
    )
    .await;
    client.close().await;
    server.abort();
    let error = result
        .expect("once observation exceeded its no-progress bound")
        .expect_err("stalled peer must fail");
    assert!(
        error
            .to_string()
            .contains("collaboration observation timed out"),
        "{error}"
    );
}

#[tokio::test]
async fn once_observation_bounds_opening() {
    assert_once_observation_stall_is_bounded(0).await;
}
#[tokio::test]
async fn once_observation_bounds_open_without_complete() {
    assert_once_observation_stall_is_bounded(1).await;
}
#[tokio::test]
async fn once_observation_bounds_checkpoint_without_complete() {
    assert_once_observation_stall_is_bounded(2).await;
}
#[tokio::test]
async fn once_observation_bounds_next_page() {
    assert_once_observation_stall_is_bounded(3).await;
}

#[tokio::test]
async fn live_observation_keeps_long_lived_semantics() {
    let _guard = crate::test_process_env::shared().await;
    let (client, server, release) = stalled_collaboration_peer(1).await;
    let request = v2::ObserveCollaborationRequest {
        observe: Some(v2::ObserveOptions {
            mode: v2::ObservationMode::Follow as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    {
        let mut observation = std::pin::pin!(client.observe_collaboration_events_with_timeout(
            request,
            std::time::Duration::from_millis(100)
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut observation)
                .await
                .is_err()
        );
        release.send(()).expect("release peer");
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut observation)
            .await
            .expect("released")
            .expect("live observation completes");
    }
    client.close().await;
    server.abort();
}

/// Endpoint advertisement must reach the typed client's delivery checks. The
/// transport counter proves missing support refuses before a write RPC.
#[tokio::test]
async fn collaboration_features_are_negotiated_before_transport() {
    use super::helpers::native_client_error;
    use api::heddle::api::common::{MandatoryProtocolFeature as Feature, ProtocolCompatibility};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use thread_api::rpc;
    use wire::ProtocolError;

    let _guard = crate::test_process_env::shared().await;
    for features in [
        None,
        Some(vec![]),
        Some(vec![2]),
        Some(vec![3]),
        Some(vec![2, 3, 4]),
    ] {
        let supported_features = features.clone();
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("server address")
            .bind()
            .await
            .expect("server");
        let address = server.addr();
        let key = server.id().as_bytes().to_vec();
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let task = tokio::spawn(async move {
            let connection = server
                .accept()
                .await
                .expect("client")
                .await
                .expect("connection");
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let bytes = recv.read_to_end(65536).await.expect("request");
                let request = decode_request_frame(&bytes).expect("frame");
                let response = if request.method
                    == "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint"
                {
                    v2::DescribeEndpointResponse {
                        endpoint: Some(v2::EndpointRef {
                            kind: v2::EndpointKind::Weft as i32,
                            public_key: key.clone(),
                        }),
                        supported_packages: vec!["heddle.api.v1alpha2".into()],
                        implemented_methods: [
                            "OpenDiscussion",
                            "AppendTurn",
                            "PutContext",
                            "ResolveDiscussion",
                            "EditDiscussionTags",
                        ]
                        .map(|method| format!("/heddle.api.v1alpha2.CollaborationService/{method}"))
                        .to_vec(),
                        protocol: features.clone().map(|mandatory_features| {
                            ProtocolCompatibility {
                                mandatory_features,
                                protocol_version: 2,
                            }
                        }),
                        ..Default::default()
                    }
                    .encode_to_vec()
                } else {
                    captured.fetch_add(1, Ordering::SeqCst);
                    v2::MutationResponse::default().encode_to_vec()
                };
                send.write_all(&encode_success_response(&response).expect("response frame"))
                    .await
                    .expect("response");
                send.finish().expect("FIN");
            }
            server.close().await;
        });
        let local = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("client address")
            .bind()
            .await
            .expect("client");
        let client =
            HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
                .await
                .expect("hosted client");
        let remote = client.native().await.expect("negotiated endpoint");
        let principal = v2::EntityRef {
            entity: Some(v2::entity_ref::Entity::Principal(v2::PrincipalRef {
                id: uuid::Uuid::from_u128(1).to_string(),
            })),
        };
        let source = v2::EntityRef {
            entity: Some(v2::entity_ref::Entity::Source(v2::SourceMention {
                revision: Some(v2::RevisionRef {
                    spool: Some(v2::SpoolRef {
                        id: uuid::Uuid::from_u128(2).to_string(),
                    }),
                    revision: Some(v2::revision_ref::Revision::GitCommitOid("a".repeat(40))),
                }),
                path: "src/lib.rs".into(),
                ..Default::default()
            })),
        };
        let tags = vec![thread_api::collaboration::annotation_tag_ref(
            &objects::object::AnnotationTag::from("decision"),
        )];
        let check = |result: Result<v2::MutationResponse, _>, feature: Feature, before: usize| {
            if supported_features
                .as_ref()
                .is_some_and(|f| f.contains(&(feature as i32)))
            {
                result.expect("advertised variant reaches transport");
                assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            } else {
                let error = native_client_error(result.expect_err("unsupported variant"));
                assert!(
                    matches!(&error, ProtocolError::CapabilityNotSupported(_)),
                    "{error:?}"
                );
                assert!(error.to_string().contains(feature.as_str_name()), "{error}");
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    before,
                    "refusal must precede transport"
                );
            }
        };
        for mention in [principal.clone(), source] {
            let before = calls.load(Ordering::SeqCst);
            check(
                remote
                    .api
                    .call::<rpc::CollaborationServiceOpenDiscussion>(&v2::OpenDiscussionRequest {
                        client_operation_id: "feature-negotiation".into(),
                        mentions: vec![mention],
                        ..Default::default()
                    })
                    .await,
                Feature::CollaborationMentionsV1,
                before,
            );
        }
        let before = calls.load(Ordering::SeqCst);
        check(
            remote
                .api
                .call::<rpc::CollaborationServiceAppendTurn>(&v2::AppendDiscussionRequest {
                    client_operation_id: "feature-negotiation".into(),
                    mentions: vec![principal.clone()],
                    ..Default::default()
                })
                .await,
            Feature::CollaborationMentionsV1,
            before,
        );
        let context = v2::ContextDraft {
            mentions: vec![principal],
            ..Default::default()
        };
        let before = calls.load(Ordering::SeqCst);
        check(
            remote
                .api
                .call::<rpc::CollaborationServicePutContext>(&v2::PutContextRequest {
                    client_operation_id: "feature-negotiation".into(),
                    context: Some(context.clone()),
                    ..Default::default()
                })
                .await,
            Feature::CollaborationMentionsV1,
            before,
        );
        let before = calls.load(Ordering::SeqCst);
        check(
            remote
                .api
                .call::<rpc::CollaborationServiceResolveDiscussion>(&v2::ResolveDiscussionRequest {
                    client_operation_id: "feature-negotiation".into(),
                    resolution: Some(v2::resolve_discussion_request::Resolution::ExtractContext(
                        context,
                    )),
                    ..Default::default()
                })
                .await,
            Feature::CollaborationMentionsV1,
            before,
        );
        let before = calls.load(Ordering::SeqCst);
        check(
            remote
                .api
                .call::<rpc::CollaborationServiceOpenDiscussion>(&v2::OpenDiscussionRequest {
                    client_operation_id: "feature-negotiation".into(),
                    tags: tags.clone(),
                    ..Default::default()
                })
                .await,
            Feature::DiscussionTagsV1,
            before,
        );
        for replacement in [tags, vec![]] {
            let before = calls.load(Ordering::SeqCst);
            check(
                remote
                    .api
                    .call::<rpc::CollaborationServiceEditDiscussionTags>(
                        &v2::EditDiscussionTagsRequest {
                            client_operation_id: "feature-negotiation".into(),
                            tags: replacement,
                            ..Default::default()
                        },
                    )
                    .await,
                Feature::DiscussionTagsV1,
                before,
            );
        }
        let before = calls.load(Ordering::SeqCst);
        remote
            .api
            .call::<rpc::CollaborationServiceOpenDiscussion>(&v2::OpenDiscussionRequest {
                client_operation_id: "plain-discussion".into(),
                ..Default::default()
            })
            .await
            .expect("plain opening needs no feature");
        remote
            .api
            .call::<rpc::CollaborationServiceAppendTurn>(&v2::AppendDiscussionRequest {
                client_operation_id: "plain-discussion".into(),
                ..Default::default()
            })
            .await
            .expect("plain reply needs no feature");
        remote
            .api
            .call::<rpc::CollaborationServicePutContext>(&v2::PutContextRequest {
                client_operation_id: "feature-negotiation".into(),
                context: Some(v2::ContextDraft {
                    tags: vec![thread_api::collaboration::annotation_tag_ref(
                        &objects::object::AnnotationTag::from("existing"),
                    )],
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("existing context tags need no feature");
        assert_eq!(calls.load(Ordering::SeqCst), before + 3);
        client.close().await;
        task.await.expect("server finished");
    }
}
