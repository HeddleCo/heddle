use super::*;

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
