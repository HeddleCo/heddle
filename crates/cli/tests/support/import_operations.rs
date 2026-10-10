// SPDX-License-Identifier: Apache-2.0
//! Stateful import control fixture, shared with the native hosted CLI harness.

use api::heddle::api::common::{CallFailure, CallFailureCode};

use super::*;

#[derive(Clone, Debug)]
pub struct ImportJob {
    pub owner: uuid::Uuid,
    pub record: v2::OperationRecord,
}

pub async fn serve(
    method: &str,
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: &Fixture,
    server_key: Vec<u8>,
) {
    read_request_body(recv, request).await;
    let frame = decode_request_frame(request).expect("import control frame");
    // Account identity comes from the registered verifying root, as in Weft,
    // never from a caller-supplied account fact.
    let caller = [
        (uuid::Uuid::from_u128(2), 71),
        (uuid::Uuid::from_u128(3), 72),
    ]
    .into_iter()
    .find_map(|(account, seed)| {
        let root = biscuit_auth::KeyPair::from(
            &biscuit_auth::PrivateKey::from_bytes(&[seed; 32], biscuit_auth::Algorithm::Ed25519)
                .expect("registered account root"),
        );
        let token = heddle_biscuit_verifier::signature_v1::verify(
            &frame.context.bearer_capability,
            root.public(),
        )
        .ok()?;
        let inspected =
            heddle_biscuit_verifier::inspect_verified_credential(&token, &root.public())
                .expect("verified caller credential");
        assert_eq!(inspected.asserted_account, Some(account));
        Some(account)
    })
    .expect("registered caller account");
    match method.rsplit('/').next().expect("method") {
        "CommitImportJob" => {
            let body = v2::CommitImportJobRequest::decode(frame.body).expect("commit request");
            let reference = v2::RecordRef {
                spool: body.destination.clone(),
                id: uuid::Uuid::new_v4().to_string(),
            };
            fixture
                .captured
                .lock()
                .expect("jobs")
                .import_jobs
                .push(ImportJob {
                    owner: caller,
                    record: v2::OperationRecord {
                        r#ref: Some(reference.clone()),
                        client_operation_id: body.client_operation_id.clone(),
                        version: vec![1; 32],
                        state: v2::operation_record::State::Running as i32,
                        cancellation_supported: true,
                        subject: Some(v2::OperationSubject {
                            subject: Some(v2::operation_subject::Subject::Import(
                                v2::ImportOperationSubject {
                                    source_url: body.source.expect("source").clone_url,
                                    ..Default::default()
                                },
                            )),
                        }),
                        ..Default::default()
                    },
                });
            write_unary(
                send,
                &v2::MutationResponse {
                    receipt: Some(v2::MutationReceipt {
                        client_operation_id: body.client_operation_id,
                        endpoint: Some(v2::EndpointRef {
                            kind: v2::EndpointKind::Weft as i32,
                            public_key: server_key,
                        }),
                        outcome: Some(v2::mutation_receipt::Outcome::PendingOperation(reference)),
                        ..Default::default()
                    }),
                },
            )
            .await;
        }
        "CancelOperation" => {
            let body = v2::CancelOperationRequest::decode(frame.body).expect("cancel request");
            let result = {
                let mut capture = fixture.captured.lock().expect("cancel admission");
                capture.cancel_requests.push(body.clone());
                let job = capture
                    .import_jobs
                    .iter_mut()
                    .find(|job| job.record.r#ref == body.operation)
                    .expect("operation");
                if caller != job.owner {
                    Err((CallFailureCode::PermissionDenied, "operation unavailable"))
                } else if !job.record.cancellation_supported
                    || matches!(
                        v2::operation_record::State::try_from(job.record.state),
                        Ok(v2::operation_record::State::Completed
                            | v2::operation_record::State::Failed
                            | v2::operation_record::State::Canceled)
                    )
                {
                    Err((
                        CallFailureCode::FailedPrecondition,
                        "operation does not accept cancellation",
                    ))
                } else if body.expected_version != job.record.version {
                    Err((CallFailureCode::Aborted, "operation version changed"))
                } else {
                    assert!(!body.client_operation_id.is_empty());
                    job.record.cancellation_requested = true;
                    job.record.version = vec![2; 32];
                    Ok(v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id,
                            endpoint: Some(v2::EndpointRef {
                                kind: v2::EndpointKind::Weft as i32,
                                public_key: server_key,
                            }),
                            outcome: Some(v2::mutation_receipt::Outcome::Applied(
                                v2::Applied::default(),
                            )),
                            ..Default::default()
                        }),
                    })
                }
            };
            match result {
                Ok(response) => write_unary(send, &response).await,
                Err((code, message)) => {
                    send.write_all(
                        &api::framing::encode_failure_response(&CallFailure {
                            code: code as i32,
                            message: message.into(),
                            ..Default::default()
                        })
                        .expect("failure frame"),
                    )
                    .await
                    .expect("refusal");
                }
            }
        }
        "ObserveOperations" => {
            let body = v2::ObserveOperationsRequest::decode(frame.body).expect("observe request");
            let records = {
                let mut capture = fixture.captured.lock().expect("observe jobs");
                capture
                    .import_jobs
                    .iter_mut()
                    .filter(|job| {
                        job.owner == caller
                            && body
                                .operations
                                .contains(job.record.r#ref.as_ref().expect("ref"))
                    })
                    .map(|job| {
                        // Model the worker acknowledging the durable cancel request.
                        if job.record.cancellation_requested {
                            job.record.state = v2::operation_record::State::Canceled as i32;
                            job.record.cancellation_supported = false;
                            job.record.version = vec![3; 32];
                        }
                        job.record.clone()
                    })
                    .collect::<Vec<_>>()
            };
            assert!(records.len() <= 1);
            let mut sequence = 1;
            for mut frame in snapshot_frames(server_key) {
                if records.is_empty() && matches!(frame.body, Some(v2::stream_frame::Body::Data(_)))
                {
                    continue;
                }
                frame.sequence = sequence;
                sequence += 1;
                let payload = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)))
                    .then(|| {
                        records
                            .first()
                            .cloned()
                            .map(v2::operation_event::Payload::Operation)
                    })
                    .flatten();
                if let Some(v2::stream_frame::Body::Checkpoint(checkpoint)) = frame.body.as_mut() {
                    checkpoint.page = Some(v2::PageInfo {
                        exhausted: true,
                        ..Default::default()
                    });
                }
                write_message(
                    send,
                    &v2::OperationEvent {
                        frame: Some(frame),
                        payload,
                    },
                )
                .await;
            }
        }
        _ => unreachable!("import fixture methods"),
    }
    send.finish().expect("import control FIN");
}
