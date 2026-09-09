use objects::object::{
    Attribution, CollaborationActor, CollaborationAnchor as Anchor, CollaborationIdempotencyKey,
    CollaborationMetadata, CollaborationOperationBodyV1 as Body, CollaborationResolution,
    CollaborationScope, DiscussionRecordId, DiscussionTurnV1, Principal, VisibilityTier,
};

use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    spool: uuid::Uuid,
) {
    let signer = crypto::Ed25519Signer::from_seed(&[71; 32]).expect("root");
    let discussion = DiscussionRecordId::generate();
    let metadata = CollaborationMetadata {
        scope: CollaborationScope {
            spool,
            thread: Some(replica.thread_id()),
        },
        actor: CollaborationActor {
            principal_id: uuid::Uuid::from_bytes([9; 16]),
            agent_id: None,
        },
        mentions: vec![],
    };
    let command = |id: uuid::Uuid, body| thread_api::collaboration::Command {
        discussion,
        operation_id: CollaborationIdempotencyKey::new(id.to_string()).expect("command ID"),
        metadata: metadata.clone(),
        author: Attribution::human(Principal::new("Owner", "")),
        occurred_at_ms: chrono::Utc::now().timestamp_millis(),
        body,
    };
    let id = uuid::Uuid::new_v4();
    let open = command(
        id,
        Body::Open {
            blocking: true,
            title: "Review source".into(),
            anchor: Anchor::Repository,
            visibility: VisibilityTier::Public,
            turn: DiscussionTurnV1::new("Explain the change").expect("turn"),
            thread_ref: None,
        },
    )
    .sign(&[], &signer)
    .expect("open signature");
    let signed = crypto::thread_operation::SignedOperation {
        canonical: open.canonical_record.clone(),
        signature: open.signatures[0].signature.clone(),
    };
    let record = repo::thread_replication::collaboration::Record {
        kind: 1,
        id: discussion.to_string(),
    };
    let failed = replica.collaboration_command(
        &signed,
        repository.store(),
        repo::thread_replication::collaboration::Command {
            namespace: "rollback-fixture".into(),
            id,
            method: "/heddle.api.v2alpha1.CollaborationService/OpenDiscussion",
            request_hash: [0; 32],
            record: record.clone(),
            precondition: repo::thread_replication::collaboration::Precondition::New,
        },
        |_| Ok(()),
        |_| {
            Err(repo::thread_replication::Error::Invalid(
                "response construction failed".into(),
            ))
        },
    );
    assert!(failed.is_err());
    assert!(
        replica
            .collaboration_heads(&record)
            .expect("rollback heads")
            .is_empty(),
        "failed response rolls back signed acceptance and indexes"
    );
    assert!(
        repo::thread_replication::collaboration_search::search(
            repository.heddle_dir(),
            "Explain the change",
            0,
            10
        )
        .expect("rolled back search")
        .is_empty(),
        "query projection shares acceptance transaction"
    );
    let spool = SpoolRef {
        id: spool.to_string(),
    };
    let anchor = thread_api::collaboration::anchor_ref(&Anchor::Repository, &metadata.scope)
        .expect("wire anchor");
    let request = OpenDiscussionRequest {
        client_operation_id: id.to_string(),
        spool: Some(spool.clone()),
        anchor: Some(anchor),
        title: "Review source".into(),
        initial_body: "Explain the change".into(),
        blocking: true,
        signed_operation: Some(open.clone()),
        audience: Audience::Public as i32,
        ..Default::default()
    };
    let result = remote
        .api
        .call::<thread_api::rpc::CollaborationServiceOpenDiscussion>(&request)
        .await
        .expect("open");
    assert_eq!(
        remote
            .api
            .call::<thread_api::rpc::CollaborationServiceOpenDiscussion>(&request)
            .await
            .expect("exact replay"),
        result
    );
    let mut tampered = request.clone();
    tampered.title = "unsigned replacement".into();
    assert!(
        remote
            .api
            .call::<thread_api::rpc::CollaborationServiceOpenDiscussion>(&tampered)
            .await
            .is_err(),
        "request cannot replace signed title"
    );
    let id = uuid::Uuid::new_v4();
    let append = command(
        id,
        Body::AppendTurn {
            turn: DiscussionTurnV1::new("Evidence attached").expect("turn"),
        },
    )
    .sign(std::slice::from_ref(&open), &signer)
    .expect("append");
    let reference = RecordRef {
        spool: Some(spool.clone()),
        id: discussion.to_string(),
    };
    let appended = remote
        .api
        .call::<thread_api::rpc::CollaborationServiceAppendTurn>(&AppendDiscussionRequest {
            client_operation_id: id.to_string(),
            discussion: Some(reference.clone()),
            causal_parents: vec![
                thread_api::collaboration::operation_id(&open)
                    .expect("parent")
                    .as_bytes()
                    .to_vec(),
            ],
            body: "Evidence attached".into(),
            signed_operation: Some(append.clone()),
            ..Default::default()
        })
        .await
        .expect("append");
    let Some(mutation_receipt::Outcome::Applied(applied)) =
        appended.receipt.expect("receipt").outcome
    else {
        panic!("applied")
    };
    let version = applied.resulting_versions[0].version.clone();
    let id = uuid::Uuid::new_v4();
    let resolve = command(
        id,
        Body::Resolve {
            resolution: CollaborationResolution::Dismissed {
                reason: "Reviewed".into(),
            },
        },
    )
    .sign(std::slice::from_ref(&append), &signer)
    .expect("resolve");
    let mut request = ResolveDiscussionRequest {
        client_operation_id: id.to_string(),
        discussion: Some(reference),
        expected_version: vec![0; 32],
        resolution: Some(resolve_discussion_request::Resolution::DismissalReason(
            "Reviewed".into(),
        )),
        signed_operation: Some(resolve),
    };
    assert!(
        remote
            .api
            .call::<thread_api::rpc::CollaborationServiceResolveDiscussion>(&request)
            .await
            .is_err(),
        "stale heads require refresh"
    );
    request.expected_version = version;
    remote
        .api
        .call::<thread_api::rpc::CollaborationServiceResolveDiscussion>(&request)
        .await
        .expect("resolve current heads");
    assert_eq!(
        replica
            .view()
            .expect("replica view")
            .collaboration
            .discussions[&discussion]
            .turns
            .len(),
        2
    );
    let mut observation = remote
        .api
        .observe::<thread_api::rpc::CollaborationServiceObserveCollaboration>(
            &ObserveCollaborationRequest {
                spool: Some(spool.clone()),
                include_history: true,
                include_operations: true,
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("collaboration view");
    let mut records = 0;
    let mut turns = 0;
    let mut proofs = 0;
    let mut current_version = vec![];
    let mut committed = false;
    while let Some(event) = observation.next().await.expect("view event") {
        if matches!(
            event.frame.and_then(|frame| frame.body),
            Some(stream_frame::Body::Checkpoint(_))
        ) {
            committed = true;
        }
        match event.payload {
            Some(collaboration_event::Payload::Discussion(record)) => {
                assert_eq!(record.status, discussion_record::Status::Resolved as i32);
                assert_eq!(record.turn_count, 2);
                current_version = record.version;
                records += 1;
            }
            Some(collaboration_event::Payload::Turn(_)) => turns += 1,
            Some(collaboration_event::Payload::Operation(_)) => proofs += 1,
            _ => {}
        }
    }
    assert_eq!((records, turns, proofs), (1, 2, 3));
    assert!(committed);
    let id = uuid::Uuid::new_v4();
    let signed = command(
        id,
        Body::Reopen {
            reason: "More evidence needed".into(),
        },
    )
    .sign(
        &[request.signed_operation.expect("resolve record")],
        &signer,
    )
    .expect("reopen signature");
    remote
        .api
        .call::<thread_api::rpc::CollaborationServiceReopenDiscussion>(&ReopenDiscussionRequest {
            client_operation_id: id.to_string(),
            discussion: Some(RecordRef {
                spool: Some(spool.clone()),
                id: discussion.to_string(),
            }),
            expected_version: current_version,
            reason: "More evidence needed".into(),
            signed_operation: Some(signed),
        })
        .await
        .expect("reopen");
    let context = objects::object::ContextRevision {
        version: 2,
        id: uuid::Uuid::new_v4(),
        parents: vec![],
        metadata: metadata.clone(),
        anchor: Anchor::Repository,
        content: "Decision rationale".into(),
        tags: vec!["review".into()],
        supersedes: None,
        extracted_from: None,
        occurred_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    let signed = thread_api::collaboration::sign_context(context.clone(), &[], &signer)
        .expect("context signature");
    let context_command = uuid::Uuid::new_v4().to_string();
    remote
        .api
        .call::<thread_api::rpc::CollaborationServicePutContext>(&PutContextRequest {
            client_operation_id: context_command.clone(),
            context: Some(ContextDraft {
                r#ref: Some(RecordRef {
                    spool: Some(spool),
                    id: context.id.to_string(),
                }),
                anchor: Some(
                    thread_api::collaboration::anchor_ref(&context.anchor, &metadata.scope)
                        .expect("anchor"),
                ),
                content: context.content,
                tags: context
                    .tags
                    .iter()
                    .map(thread_api::collaboration::annotation_tag_ref)
                    .collect(),
                ..Default::default()
            }),
            signed_operation: Some(signed),
            ..Default::default()
        })
        .await
        .expect("context");
    let mut operations = remote
        .api
        .observe::<thread_api::rpc::OperationServiceObserveOperations>(&ObserveOperationsRequest {
            spools: vec![SpoolRef {
                id: metadata.scope.spool.to_string(),
            }],
            client_operation_ids: vec![context_command.clone()],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("recover context command receipt");
    let mut receipts = 0;
    while let Some(event) = operations.next().await.expect("operation frame") {
        if let Some(operation_event::Payload::Operation(record)) = event.payload {
            assert_eq!(record.client_operation_id, context_command);
            assert_eq!(record.state, operation_record::State::Completed as i32);
            assert!(
                !record.cancellation_supported,
                "completed synchronous command is not cancelable"
            );
            receipts += 1;
        }
    }
    assert_eq!(
        receipts, 1,
        "operation-ID recovery selects exact committed caller receipt"
    );
    let mut search = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            spools: vec![SpoolRef {
                id: metadata.scope.spool.to_string(),
            }],
            text: "Decision rationale".into(),
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("indexed search RPC");
    let mut search_hits = 0;
    let mut search_complete = false;
    while let Some(event) = search.next().await.expect("search frame") {
        match event.payload {
            Some(search_event::Payload::Hit(hit)) => {
                assert!(
                    matches!(hit.subject.and_then(|subject|subject.entity),Some(entity_ref::Entity::Context(reference)) if reference.id==context.id.to_string())
                );
                search_hits += 1;
            }
            Some(search_event::Payload::Complete(status)) => {
                assert!(status.page.expect("search page").exhausted);
                search_complete = true;
            }
            _ => {}
        }
    }
    assert_eq!(search_hits, 1);
    assert!(search_complete);
    let hits = repo::thread_replication::collaboration_search::search(
        repository.heddle_dir(),
        "Decision rationale",
        0,
        10,
    )
    .expect("indexed context full text");
    assert_eq!(
        hits.len(),
        1,
        "accepted context indexed once despite replay"
    );
    assert_eq!(hits[0].record, context.id.to_string());
    assert_eq!(hits[0].kind, 2);
    assert!(
        repo::thread_replication::collaboration_search::search(
            repository.heddle_dir(),
            "Decision OR unrelated",
            0,
            10
        )
        .expect("literal phrase query")
        .is_empty(),
        "caller text must not become FTS syntax"
    );
    let empty = repository
        .create_native_thread(
            "empty-collaboration",
            repository.head().expect("head").expect("source"),
            None,
            "no collaboration records",
        )
        .expect("empty selected Thread");
    for (thread, expected) in [(replica.thread_id(), true), (empty.thread_id(), false)] {
        let mut aggregate = remote
            .api
            .observe::<thread_api::rpc::ThreadServiceObserveThread>(&ObserveThreadRequest {
                thread: Some(ThreadRef {
                    spool: Some(SpoolRef {
                        id: metadata.scope.spool.to_string(),
                    }),
                    id: Some(ThreadId {
                        value: thread.as_bytes().to_vec(),
                    }),
                }),
                sections: vec![ThreadSection::Collaboration as i32],
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("selected Thread collaboration aggregate");
        let mut found_context = false;
        let mut found_discussion = false;
        let mut complete = false;
        while let Some(event) = aggregate.next().await.expect("aggregate frame") {
            match event.payload {
                Some(thread_event::Payload::Context(value)) => {
                    assert!(
                        expected,
                        "other Thread context must not enter selected aggregate"
                    );
                    found_context |= value
                        .r#ref
                        .as_ref()
                        .is_some_and(|reference| reference.id == context.id.to_string());
                }
                Some(thread_event::Payload::Discussion(value)) => {
                    assert!(
                        expected,
                        "other Thread discussion must not enter selected aggregate"
                    );
                    found_discussion |= value
                        .r#ref
                        .as_ref()
                        .is_some_and(|reference| reference.id == discussion.to_string());
                }
                Some(thread_event::Payload::Status(status))
                    if status.section == "collaboration" =>
                {
                    complete = status.coverage == Coverage::Complete as i32
                }
                _ => {}
            }
        }
        assert_eq!(
            found_context, expected,
            "selected Thread context is composed"
        );
        assert_eq!(
            found_discussion, expected,
            "selected Thread discussion is composed"
        );
        assert!(complete, "collaboration reports actual complete coverage");
    }
}
