use super::*;

type DeviceRemote =
    thread_api::Remote<thread_api::transport::IrohTransport<thread_api::credentials::Credentials>>;

async fn run_snapshot_batch(
    remote: &DeviceRemote,
    spool: &SpoolRef,
    runs: Vec<RecordRef>,
) -> thread_api::observation::CommittedBatch<run_event::Payload> {
    let mut observed = remote
        .observe::<thread_api::rpc::RunServiceObserveRuns>(
            ObserveRunsRequest {
                spool: Some(spool.clone()),
                runs,
                include_timeline: true,
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("run snapshot opens");
    observed
        .next_commit()
        .await
        .expect("snapshot protocol")
        .expect("snapshot")
}

async fn run_snapshot(
    remote: &DeviceRemote,
    spool: &SpoolRef,
    runs: Vec<RecordRef>,
) -> Vec<run_event::Payload> {
    run_snapshot_batch(remote, spool, runs).await.changes
}

pub(super) async fn principal_visibility(
    owner: &DeviceRemote,
    browser: &iroh::Endpoint,
    address: iroh::EndpointAddr,
    endpoint: [u8; 32],
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    use crypto::Signer as _;

    use crate::hosted_runtime::{
        device_flow::{AgentAttenuation, attenuate_for_agent},
        hosted::claim_protocol::NATIVE_ALPN,
        root_mint::mint_agent_root,
    };

    let store = repo::device_runs::RunStore::open(repository.heddle_dir()).expect("run store");
    let spool_ref = SpoolRef {
        id: spool.to_string(),
    };
    let owner_id = uuid::Uuid::from_bytes([9; 16]).to_string();
    let mut refs = Vec::new();
    for (id, agent) in [
        ("run-agent-a", "agent-a"),
        ("run-agent-b", "agent-b"),
        ("run-owner", ""),
    ] {
        let reference = RecordRef {
            spool: Some(spool_ref.clone()),
            id: id.into(),
        };
        store
            .put_run(RunRecord {
                r#ref: Some(reference.clone()),
                principal_id: owner_id.clone(),
                agent_id: agent.into(),
                ..Default::default()
            })
            .expect("run");
        store
            .put_timeline(&TimelineRecord {
                r#ref: Some(RecordRef {
                    spool: Some(spool_ref.clone()),
                    id: format!("{id}:0"),
                }),
                run: Some(reference.clone()),
                kind: "PostToolUse".into(),
                summary: format!("private {id}"),
                ..Default::default()
            })
            .expect("timeline");
        refs.push(reference);
    }
    let owner_events = run_snapshot(owner, &spool_ref, refs.clone()).await;
    assert_eq!(
        owner_events
            .iter()
            .filter(|event| matches!(event, run_event::Payload::Run(_)))
            .count(),
        3,
        "owner sees every run"
    );
    assert_eq!(
        owner_events
            .iter()
            .filter(|event| matches!(event, run_event::Payload::Timeline(_)))
            .count(),
        3,
        "owner sees every timeline"
    );

    let root = crypto::Ed25519Signer::from_seed(&[71; 32]).expect("owner root");
    let root_token = mint_agent_root(&[71; 32]).expect("root token").token;
    let mut agents = Vec::new();
    for (agent, seed) in [("agent-a", 81), ("agent-b", 82)] {
        let signer = crypto::Ed25519Signer::from_seed(&[seed; 32]).expect("agent proof key");
        let token = attenuate_for_agent(
            &root_token,
            AgentAttenuation::time_bounded(agent, chrono::Utc::now() + chrono::Duration::hours(1)),
            &root,
            signer.public_key(),
        )
        .expect("delegated capability");
        let credentials = thread_api::credentials::Credentials::Signed {
            signer: Arc::new(signer),
            biscuit: token.into_bytes(),
            grant_envelope: Vec::new(),
        };
        let transport = thread_api::transport::IrohTransport::new(
            browser
                .connect(address.clone(), NATIVE_ALPN)
                .await
                .expect("agent connection"),
            credentials,
            256 * 1024,
            std::time::Duration::from_secs(10),
        )
        .expect("agent transport");
        agents.push(
            thread_api::Remote::discover(transport, endpoint, EndpointKind::Device)
                .await
                .expect("agent remote"),
        );
    }
    let own = run_snapshot(&agents[0], &spool_ref, vec![refs[0].clone()]).await;
    assert!(own.iter().any(|event| matches!(event, run_event::Payload::Run(run) if run.r#ref.as_ref() == Some(&refs[0]))), "agent sees own run");
    assert!(own.iter().any(|event| matches!(event, run_event::Payload::Timeline(timeline) if timeline.run.as_ref() == Some(&refs[0]))), "agent sees own timeline");
    let list = run_snapshot(&agents[0], &spool_ref, Vec::new()).await;
    assert_eq!(
        list.iter()
            .filter(|event| matches!(event, run_event::Payload::Run(_)))
            .count(),
        1,
        "sibling and owner runs absent from list"
    );
    assert_eq!(
        list.iter()
            .filter(|event| matches!(event, run_event::Payload::Timeline(_)))
            .count(),
        1,
        "sibling and owner timelines absent from list"
    );
    for forbidden in [&refs[1], &refs[2]] {
        let denied = run_snapshot_batch(&agents[0], &spool_ref, vec![forbidden.clone()]).await;
        let absent = run_snapshot_batch(
            &agents[0],
            &spool_ref,
            vec![RecordRef {
                spool: Some(spool_ref.clone()),
                id: "no-such-run".into(),
            }],
        )
        .await;
        assert_eq!(
            (denied.changes, denied.page),
            (absent.changes, absent.page),
            "forbidden and nonexistent snapshots are indistinguishable"
        );
    }
    let sibling = run_snapshot(&agents[1], &spool_ref, vec![refs[0].clone()]).await;
    assert!(
        !sibling.iter().any(|event| matches!(
            event,
            run_event::Payload::Run(_) | run_event::Payload::Timeline(_)
        )),
        "sibling cannot observe another agent's run"
    );
}

pub(super) async fn latest_then_follow(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    let store = repo::device_runs::RunStore::open(repository.heddle_dir()).expect("run store");
    let spool_ref = SpoolRef {
        id: spool.to_string(),
    };
    let run = RecordRef {
        spool: Some(spool_ref.clone()),
        id: "latest-follow-run".into(),
    };
    store
        .put_run(RunRecord {
            r#ref: Some(run.clone()),
            harness: "claude-code".into(),
            ..Default::default()
        })
        .expect("run");
    let append = |position| {
        store
            .put_timeline(&TimelineRecord {
                r#ref: Some(RecordRef {
                    spool: Some(spool_ref.clone()),
                    id: format!("latest-follow-run:{position}"),
                }),
                run: Some(run.clone()),
                position,
                kind: "PostToolUse".into(),
                summary: format!("tool {position}"),
                ..Default::default()
            })
            .expect("timeline");
    };
    for position in 0..5 {
        append(position);
    }
    let mut observed = remote
        .observe::<thread_api::rpc::RunServiceObserveRuns>(
            ObserveRunsRequest {
                spool: Some(spool_ref.clone()),
                runs: vec![run.clone()],
                include_timeline: true,
                timeline_start: TimelineStart::Latest as i32,
                timeline_limit: 2,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("latest stream");
    let initial = tokio::time::timeout(std::time::Duration::from_secs(10), observed.next_commit())
        .await
        .expect("snapshot deadline")
        .expect("snapshot protocol")
        .expect("snapshot");
    assert!(initial.replace);
    assert_eq!(initial.page.as_ref().map(|page| page.exhausted), Some(true));
    let positions: Vec<_> = initial
        .changes
        .iter()
        .filter_map(|change| match change {
            run_event::Payload::Timeline(record) => Some(record.position),
            _ => None,
        })
        .collect();
    assert_eq!(positions, vec![3, 4]);
    append(5);
    let follow = tokio::time::timeout(std::time::Duration::from_secs(10), observed.next_commit())
        .await
        .expect("follow deadline")
        .expect("follow protocol")
        .expect("follow");
    assert!(!follow.replace);
    let positions: Vec<_> = follow
        .changes
        .iter()
        .filter_map(|change| match change {
            run_event::Payload::Timeline(record) => Some(record.position),
            _ => None,
        })
        .collect();
    assert_eq!(positions, vec![5]);
}
