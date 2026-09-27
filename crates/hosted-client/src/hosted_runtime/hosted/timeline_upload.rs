//! Exact native timeline writes. Each call signs a new method-bound nonce.
use api::{
    heddle::api::{
        common::{CallFailureCode, ErrorReason},
        v1alpha2 as contract,
    },
    v2::client::ClientError,
};
use thread_api::{rpc, transport::Error as TransportError};

use super::HostedClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineUploadFailureKind {
    Retry,
    Gone,
    Conflict,
    Denied,
}

impl HostedClient {
    pub async fn register_timeline_origin(
        &self,
        request: &contract::RegisterTimelineOriginRequest,
    ) -> Result<contract::RegisterTimelineOriginResponse, TimelineUploadFailureKind> {
        api::timeline_upload::validate_registration(request)
            .map_err(|_| TimelineUploadFailureKind::Denied)?;
        self.native()
            .await
            .map_err(|_| TimelineUploadFailureKind::Retry)?
            .api
            .call::<rpc::SyncServiceRegisterTimelineOrigin>(request)
            .await
            .map_err(classify)
    }

    pub async fn upload_scrubbed_timeline(
        &self,
        request: &contract::UploadScrubbedTimelineRequest,
    ) -> Result<contract::UploadScrubbedTimelineResponse, TimelineUploadFailureKind> {
        let now_micros = i128::from(chrono::Utc::now().timestamp_micros());
        api::timeline_upload::validate_upload(request, now_micros)
            .map_err(|_| TimelineUploadFailureKind::Denied)?;
        self.native()
            .await
            .map_err(|_| TimelineUploadFailureKind::Retry)?
            .api
            .call::<rpc::SyncServiceUploadScrubbedTimeline>(request)
            .await
            .map_err(classify)
    }
}

fn classify(error: ClientError<TransportError>) -> TimelineUploadFailureKind {
    let ClientError::Transport(TransportError::Remote(failure)) = error else {
        return TimelineUploadFailureKind::Retry;
    };
    let code = CallFailureCode::try_from(failure.code).unwrap_or_default();
    let reason = failure
        .detail()
        .ok()
        .flatten()
        .and_then(|detail| ErrorReason::try_from(detail.reason).ok());
    match (code, reason) {
        (CallFailureCode::FailedPrecondition, Some(ErrorReason::ResourceGone)) => {
            TimelineUploadFailureKind::Gone
        }
        (CallFailureCode::AlreadyExists, Some(ErrorReason::OperationIdReused))
        | (CallFailureCode::Aborted, Some(ErrorReason::VersionConflict)) => {
            TimelineUploadFailureKind::Conflict
        }
        (CallFailureCode::NotFound, _)
        | (CallFailureCode::Unavailable, _)
        | (CallFailureCode::ResourceExhausted, _)
        | (CallFailureCode::DeadlineExceeded, _)
        | (CallFailureCode::Unimplemented, _) => TimelineUploadFailureKind::Retry,
        _ => TimelineUploadFailureKind::Denied,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use api::{
        framing::{decode_request_frame, encode_failure_response, encode_success_response},
        heddle::api::{
            common::{CallFailure, ErrorDetail},
            v1alpha2::{
                RecordRef, SpoolRef, ThreadId, ThreadRef, TimelineOriginCredentialClass,
                TimelineOriginCredentialIdentity, TimelineOriginEndorsement,
                TimelineServerIssuedCredential, UploadRunSummary, UploadScrubbedTimelineAck,
                UploadTimelineEvent, UploadTimelineEventKind, UploadTimelinePositionGap,
                operation_record::State, timeline_origin_credential_identity::Identity,
                upload_scrubbed_timeline_response::Outcome,
            },
        },
    };
    use crypto::Ed25519Signer;
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use prost::Message;

    use super::*;
    use crate::hosted_runtime::hosted::CallContextFactory;

    fn request() -> contract::UploadScrubbedTimelineRequest {
        let spool = SpoolRef {
            id: uuid::Uuid::from_u128(1).to_string(),
        };
        let run = RecordRef {
            spool: Some(spool.clone()),
            id: "run_1".into(),
        };
        let thread = ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId { value: vec![2; 32] }),
        };
        contract::UploadScrubbedTimelineRequest {
            client_operation_id: uuid::Uuid::from_u128(10).to_string(),
            thread: Some(thread),
            run: Some(run),
            canonicalization_version: 1,
            run_revision: 1,
            snapshot: Some(UploadRunSummary {
                state: State::Running as i32,
                harness: "codex".into(),
            }),
            events: vec![UploadTimelineEvent {
                position: 0,
                kind: UploadTimelineEventKind::RunStarted as i32,
                recorded_at: Some(prost_types::Timestamp {
                    seconds: chrono::Utc::now().timestamp() - 60,
                    nanos: 0,
                }),
                tool_name: None,
            }],
            origin: Some(TimelineOriginEndorsement {
                deployment_public_key: vec![3; 32],
                spool_id: spool.id,
                thread_id: vec![2; 32],
                run_id: "run_1".into(),
                principal_id: uuid::Uuid::from_u128(4).to_string(),
                credential_class: TimelineOriginCredentialClass::DirectHuman as i32,
                effective_pop_key_sha256: vec![5; 32],
                credential_identity: Some(TimelineOriginCredentialIdentity {
                    identity: Some(Identity::ServerIssued(TimelineServerIssuedCredential {
                        credential_id: b"issued".to_vec(),
                    })),
                }),
                uploader_device_public_key: vec![6; 32],
                signature: vec![7; 64],
            }),
            acceptance: None,
            first_position: 0,
            origin_credential_biscuit: Vec::new(),
        }
    }

    fn failure(code: CallFailureCode, reason: ErrorReason) -> Vec<u8> {
        encode_failure_response(&CallFailure {
            code: code as i32,
            message: "timeline unavailable".into(),
            error: Some(ErrorDetail {
                reason: reason as i32,
                ..Default::default()
            }),
        })
        .expect("failure frame")
    }

    #[tokio::test]
    async fn retry_after_lost_ack_reuses_logical_request_with_a_fresh_proof() {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("server address")
            .bind()
            .await
            .expect("server");
        let address = server.addr();
        let server_key = server.id().as_bytes().to_vec();
        let server_task = tokio::spawn(async move {
            let connection = server
                .accept()
                .await
                .expect("client")
                .await
                .expect("connection");
            let mut receipt: Option<(String, [u8; 32], UploadScrubbedTimelineAck)> = None;
            let mut nonces = Vec::new();
            let mut next_position = 0;
            for attempt in 0..7 {
                let (mut send, mut recv) = connection.accept_bi().await.expect("call");
                let raw = recv.read_to_end(1024 * 1024).await.expect("request frame");
                let frame = decode_request_frame(&raw).expect("native frame");
                if attempt == 0 {
                    assert_eq!(
                        frame.method,
                        "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint"
                    );
                    let description = contract::DescribeEndpointResponse {
                        endpoint: Some(contract::EndpointRef {
                            kind: contract::EndpointKind::Weft as i32,
                            public_key: server_key.clone(),
                        }),
                        supported_packages: vec!["heddle.api.v1alpha2".into()],
                        implemented_methods: vec![
                            "/heddle.api.v1alpha2.SyncService/RegisterTimelineOrigin".into(),
                            "/heddle.api.v1alpha2.SyncService/UploadScrubbedTimeline".into(),
                        ],
                        ..Default::default()
                    };
                    send.write_all(
                        &encode_success_response(&description.encode_to_vec())
                            .expect("description"),
                    )
                    .await
                    .expect("send description");
                    send.finish().expect("description FIN");
                    continue;
                }
                if attempt == 1 {
                    assert_eq!(
                        frame.method,
                        "/heddle.api.v1alpha2.SyncService/RegisterTimelineOrigin"
                    );
                    let body = contract::RegisterTimelineOriginRequest::decode(frame.body)
                        .expect("registration");
                    api::timeline_upload::validate_registration(&body).expect("valid registration");
                    let origin = body.origin.as_ref().expect("origin");
                    let response = contract::RegisterTimelineOriginResponse {
                        origin_sha256: api::timeline_upload::origin_digest(origin)
                            .expect("digest")
                            .to_vec(),
                        registered_at: Some(prost_types::Timestamp {
                            seconds: chrono::Utc::now().timestamp(),
                            nanos: 0,
                        }),
                    };
                    send.write_all(
                        &encode_success_response(&response.encode_to_vec())
                            .expect("registration response"),
                    )
                    .await
                    .expect("send registration");
                    send.finish().expect("registration FIN");
                    continue;
                }
                assert_eq!(
                    frame.method,
                    "/heddle.api.v1alpha2.SyncService/UploadScrubbedTimeline"
                );
                let proof = frame.context.request_proof.expect("method proof");
                assert_eq!(proof.nonce.len(), 16);
                nonces.push(proof.nonce);
                let body =
                    contract::UploadScrubbedTimelineRequest::decode(frame.body).expect("upload");
                let digest = api::timeline_upload::logical_request_digest(
                    &body,
                    i128::from(chrono::Utc::now().timestamp_micros()),
                )
                .expect("logical digest");
                let response = if let Some((operation, stored_digest, ack)) = &receipt {
                    if *operation == body.client_operation_id {
                        if *stored_digest != digest {
                            failure(
                                CallFailureCode::AlreadyExists,
                                ErrorReason::OperationIdReused,
                            )
                        } else if attempt == 6 {
                            failure(
                                CallFailureCode::FailedPrecondition,
                                ErrorReason::ResourceGone,
                            )
                        } else {
                            encode_success_response(
                                &contract::UploadScrubbedTimelineResponse {
                                    outcome: Some(Outcome::Ack(ack.clone())),
                                }
                                .encode_to_vec(),
                            )
                            .expect("replay ack")
                        }
                    } else if body.first_position != next_position {
                        encode_success_response(
                            &contract::UploadScrubbedTimelineResponse {
                                outcome: Some(Outcome::Gap(UploadTimelinePositionGap {
                                    expected_next_position: next_position,
                                })),
                            }
                            .encode_to_vec(),
                        )
                        .expect("gap")
                    } else {
                        panic!("unexpected new write");
                    }
                } else {
                    assert_eq!(body.first_position, next_position);
                    next_position += body.events.len() as u64;
                    let ack = UploadScrubbedTimelineAck {
                        run: body.run.clone(),
                        run_revision: body.run_revision,
                        run_version: vec![8; 32],
                        next_position,
                        accepted_event_count: body.events.len() as u32,
                        operation: body.run.clone(),
                    };
                    receipt = Some((body.client_operation_id, digest, ack));
                    Vec::new() // Commit, then lose the response.
                };
                if !response.is_empty() {
                    send.write_all(&response).await.expect("send result");
                }
                send.finish().expect("response FIN");
            }
            connection.closed().await;
            server.close().await;
            nonces
        });
        let local = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("client address")
            .bind()
            .await
            .expect("client endpoint");
        let signer = Ed25519Signer::from_seed(&[42; 32]).expect("client key");
        let context = CallContextFactory::default()
            .with_signing_key_pem(&signer.to_pem().expect("PEM"), "principal:test")
            .expect("signed context");
        let client = HostedClient::connect_addr_with_context(local, address, context)
            .await
            .expect("client");
        let original = request();
        let registration = contract::RegisterTimelineOriginRequest {
            client_operation_id: uuid::Uuid::from_u128(9).to_string(),
            thread: original.thread.clone(),
            run: original.run.clone(),
            origin: original.origin.clone(),
            origin_credential_biscuit: Vec::new(),
        };
        let registered = client
            .register_timeline_origin(&registration)
            .await
            .expect("origin registration");
        assert_eq!(
            registered.origin_sha256,
            api::timeline_upload::origin_digest(original.origin.as_ref().expect("origin"))
                .expect("digest")
        );
        assert_eq!(
            client.upload_scrubbed_timeline(&original).await,
            Err(TimelineUploadFailureKind::Retry)
        );
        let replay = client
            .upload_scrubbed_timeline(&original)
            .await
            .expect("exact ack replay");
        assert!(matches!(replay.outcome, Some(Outcome::Ack(ref ack)) if ack.next_position == 1));
        let mut gap = original.clone();
        gap.client_operation_id = uuid::Uuid::from_u128(11).to_string();
        gap.first_position = 3;
        gap.events[0].position = 3;
        let response = client
            .upload_scrubbed_timeline(&gap)
            .await
            .expect("gap response");
        assert!(
            matches!(response.outcome, Some(Outcome::Gap(ref gap)) if gap.expected_next_position == 1)
        );
        let mut conflict = original.clone();
        conflict.run_revision += 1;
        assert_eq!(
            client.upload_scrubbed_timeline(&conflict).await,
            Err(TimelineUploadFailureKind::Conflict)
        );
        assert_eq!(
            client.upload_scrubbed_timeline(&original).await,
            Err(TimelineUploadFailureKind::Gone)
        );
        client.close().await;
        let nonces = server_task.await.expect("server finished");
        assert_eq!(nonces.len(), 5);
        assert!(nonces.windows(2).all(|pair| pair[0] != pair[1]));
    }
}
