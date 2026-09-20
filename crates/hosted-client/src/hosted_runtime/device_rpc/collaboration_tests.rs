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
    let base = replica.genesis().expect("genesis").base;
    let local_spool = repo::device_catalog::DeviceSpool {
        id: spool,
        root: repository.root().to_owned(),
        heddle_dir: repository.heddle_dir().to_owned(),
        capability_path: spool.to_string(),
    };
    let mut initial_anchor = Anchor::State { state_id: base };
    let (coverage, _) = super::collaboration_targets::project_for(
        &local_spool,
        metadata.actor.principal_id,
        None,
        replica,
        &metadata.scope,
        &mut initial_anchor,
        &mut [],
    )
    .expect("project source reference");
    assert_eq!(
        coverage,
        Coverage::Complete,
        "the exact canonical empty root is a readable system base"
    );
    let mut unknown_anchor = Anchor::State {
        state_id: objects::object::StateId::from_bytes([97; 32]),
    };
    let (coverage, _) = super::collaboration_targets::project_for(
        &local_spool,
        metadata.actor.principal_id,
        None,
        replica,
        &metadata.scope,
        &mut unknown_anchor,
        &mut [],
    )
    .expect("project unknown source reference");
    assert_eq!(
        coverage,
        Coverage::Unavailable,
        "arbitrary State hash is not an admitted source"
    );
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
            method: "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion",
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
        provenance: None,
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
                assert_eq!(
                    hit.thread
                        .as_ref()
                        .and_then(|thread| thread.id.as_ref())
                        .map(|id| id.value.as_slice()),
                    Some(replica.thread_id().as_bytes().as_slice())
                );
                assert_eq!(
                    hit.causal_id.len(),
                    32,
                    "context hit identifies one accepted revision"
                );
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
    let selected_thread = ThreadRef {
        spool: Some(SpoolRef {
            id: metadata.scope.spool.to_string(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    let exact_review = AnnotationQuery {
        all: vec![AnnotationTagPredicate {
            predicate: Some(annotation_tag_predicate::Predicate::Exact(
                thread_api::collaboration::annotation_tag_ref(&context.tags[0]),
            )),
        }],
        ..Default::default()
    };
    let mut filtered = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            threads: vec![selected_thread.clone()],
            domains: vec![SearchDomain::Context as i32],
            annotations: Some(exact_review),
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("typed local Search without text");
    let mut filtered_hits = 0;
    while let Some(event) = filtered.next().await.expect("filtered Search frame") {
        if let Some(search_event::Payload::Hit(hit)) = event.payload {
            assert_eq!(hit.thread.as_ref(), Some(&selected_thread));
            assert_eq!(hit.causal_id.len(), 32);
            filtered_hits += 1;
        }
    }
    assert_eq!(
        filtered_hits, 1,
        "exact tags select the current signed revision"
    );
    let mut wrong_thread = selected_thread.clone();
    wrong_thread.id = Some(ThreadId {
        value: vec![99; 32],
    });
    let mut unrelated = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            threads: vec![wrong_thread],
            domains: vec![SearchDomain::Context as i32],
            text: "Decision rationale".into(),
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("unrelated Thread Search");
    while let Some(event) = unrelated.next().await.expect("unrelated Search frame") {
        assert!(
            !matches!(event.payload, Some(search_event::Payload::Hit(_))),
            "Thread selector cannot search a sibling Thread"
        );
    }
    let name = replica.genesis().expect("Thread genesis").name;
    let mut threads = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            threads: vec![selected_thread.clone()],
            domains: vec![SearchDomain::Thread as i32],
            text: name,
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("local Thread domain Search");
    let mut thread_hits = 0;
    while let Some(event) = threads.next().await.expect("Thread Search frame") {
        if let Some(search_event::Payload::Hit(hit)) = event.payload {
            assert!(
                matches!(hit.subject.and_then(|subject| subject.entity), Some(entity_ref::Entity::Thread(reference)) if reference == selected_thread)
            );
            thread_hits += 1;
        }
    }
    assert_eq!(thread_hits, 1);
    let embargoed = DiscussionRecordId::generate();
    let embargo_operation = uuid::Uuid::new_v4();
    let embargo_open = thread_api::collaboration::Command {
        discussion: embargoed,
        operation_id: CollaborationIdempotencyKey::new(embargo_operation.to_string())
            .expect("embargo operation"),
        metadata: metadata.clone(),
        author: Attribution::human(Principal::new("Owner", "")),
        occurred_at_ms: chrono::Utc::now().timestamp_millis(),
        body: Body::Open {
            blocking: false,
            title: "Embargoed search needle".into(),
            anchor: Anchor::Repository,
            visibility: VisibilityTier::Private {
                scope_label: "legal-hold".into(),
            },
            turn: DiscussionTurnV1::new("Private discussion text").expect("embargo turn"),
            thread_ref: None,
        },
    }
    .sign(&[], &signer)
    .expect("embargo signature");
    remote
        .api
        .call::<thread_api::rpc::CollaborationServiceOpenDiscussion>(&OpenDiscussionRequest {
            client_operation_id: embargo_operation.to_string(),
            spool: Some(SpoolRef {
                id: metadata.scope.spool.to_string(),
            }),
            anchor: Some(
                thread_api::collaboration::anchor_ref(&Anchor::Repository, &metadata.scope)
                    .expect("embargo anchor"),
            ),
            title: "Embargoed search needle".into(),
            initial_body: "Private discussion text".into(),
            signed_operation: Some(embargo_open),
            audience: Audience::Private as i32,
            audience_label: "legal-hold".into(),
            ..Default::default()
        })
        .await
        .expect("author may store embargoed original");
    let mut embargo_search = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            threads: vec![selected_thread.clone()],
            domains: vec![SearchDomain::Discussion as i32],
            text: "Embargoed search needle".into(),
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("embargo Search observation");
    while let Some(event) = embargo_search.next().await.expect("embargo Search frame") {
        match event.payload {
            Some(search_event::Payload::Hit(_)) => {
                panic!("without an explicit label grant even owner Internal must not see Private")
            }
            Some(search_event::Payload::DomainStatus(status)) => {
                assert_eq!(status.domain, SearchDomain::Discussion as i32);
                assert_eq!(status.coverage, Coverage::Complete as i32);
                assert_eq!(
                    status.supported_modes,
                    vec![search_request::Mode::Lexical as i32]
                );
            }
            Some(search_event::Payload::Complete(status)) => {
                assert_eq!(
                    status.coverage,
                    Coverage::Complete as i32,
                    "hidden rows do not create partial coverage"
                );
                let page = status.page.expect("hidden-only search page");
                assert!(page.exhausted);
                assert!(
                    page.next_page.is_empty(),
                    "hidden-only results do not issue a cursor"
                );
            }
            _ => {}
        }
    }
    let mut public_matches = Vec::new();
    for _ in 0..2 {
        let discussion = DiscussionRecordId::generate();
        let operation_id = uuid::Uuid::new_v4();
        let signed = thread_api::collaboration::Command {
            discussion,
            operation_id: CollaborationIdempotencyKey::new(operation_id.to_string())
                .expect("public search operation"),
            metadata: metadata.clone(),
            author: Attribution::human(Principal::new("Owner", "")),
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            body: Body::Open {
                blocking: false,
                title: "Embargoed search needle".into(),
                anchor: Anchor::Repository,
                visibility: VisibilityTier::Public,
                turn: DiscussionTurnV1::new("Visible search text").expect("public turn"),
                thread_ref: None,
            },
        }
        .sign(&[], &signer)
        .expect("public search signature");
        remote
            .api
            .call::<thread_api::rpc::CollaborationServiceOpenDiscussion>(&OpenDiscussionRequest {
                client_operation_id: operation_id.to_string(),
                spool: Some(SpoolRef {
                    id: metadata.scope.spool.to_string(),
                }),
                anchor: Some(
                    thread_api::collaboration::anchor_ref(&Anchor::Repository, &metadata.scope)
                        .expect("public search anchor"),
                ),
                title: "Embargoed search needle".into(),
                initial_body: "Visible search text".into(),
                signed_operation: Some(signed),
                audience: Audience::Public as i32,
                ..Default::default()
            })
            .await
            .expect("public search discussion");
        public_matches.push(discussion.to_string());
    }
    let page_request = SearchRequest {
        threads: vec![selected_thread.clone()],
        domains: vec![SearchDomain::Discussion as i32],
        text: "Embargoed search needle".into(),
        mode: search_request::Mode::Lexical as i32,
        page: Some(PageRequest {
            size: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    let read_page = |request: SearchRequest| async move {
        let mut stream = remote
            .api
            .observe::<thread_api::rpc::SearchServiceSearch>(&request)
            .await
            .expect("paged device Search");
        let mut ids = Vec::new();
        let mut next = Vec::new();
        while let Some(event) = stream.next().await.expect("paged Search frame") {
            match event.payload {
                Some(search_event::Payload::Hit(hit)) => {
                    if let Some(entity_ref::Entity::Discussion(record)) =
                        hit.subject.and_then(|subject| subject.entity)
                    {
                        ids.push(record.id);
                    }
                }
                Some(search_event::Payload::Complete(status)) => {
                    assert_eq!(status.coverage, Coverage::Complete as i32);
                    next = status.page.expect("paged Search status").next_page;
                }
                _ => {}
            }
        }
        (ids, next)
    };
    let (first, cursor) = read_page(page_request.clone()).await;
    assert_eq!(
        first.len(),
        1,
        "hidden candidates do not fill a visible page"
    );
    assert_eq!(cursor.len(), 32, "continuation is an opaque random token");
    assert_ne!(cursor, embargoed.to_string().into_bytes());
    let mut second_request = page_request;
    second_request.page.as_mut().expect("page").after_page = cursor;
    let (second, exhausted) = read_page(second_request.clone()).await;
    let (retry, retry_exhausted) = read_page(second_request).await;
    assert_eq!(second, retry, "a resumed page is retryable");
    assert_eq!(exhausted, retry_exhausted);
    assert!(exhausted.is_empty(), "two visible matches exhaust the view");
    assert_ne!(first, second, "lookahead does not duplicate the served row");
    assert_eq!(
        first
            .into_iter()
            .chain(second)
            .collect::<std::collections::BTreeSet<_>>(),
        public_matches.into_iter().collect(),
        "hidden match cannot skip either visible discussion"
    );
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
                        value
                            .r#ref
                            .as_ref()
                            .is_none_or(|reference| reference.id != embargoed.to_string()),
                        "Thread aggregate must hide embargoed discussion"
                    );
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
