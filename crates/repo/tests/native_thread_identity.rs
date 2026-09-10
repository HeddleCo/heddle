use repo::Repository;

#[test]
fn local_init_and_fork_keep_original_signed_thread_identity() {
    let directory = tempfile::tempdir().expect("repository directory");
    let repository = Repository::init_default(directory.path()).expect("native init");
    repository.seed_default_thread().expect("main thread");
    let spool = repository.native_spool_id().expect("spool identity");
    let main = repository.native_thread("main").expect("main replica");
    let original = main.signed_genesis().expect("creator proof");
    assert_eq!(main.genesis().expect("genesis").spool, spool.to_string());
    let base = repository.head().expect("head").expect("seed state");
    let child = repository
        .create_native_thread("feature", base, Some("main"), "edit docs")
        .expect("new native Thread");
    assert_eq!(
        child.genesis().expect("child genesis").parent,
        Some(main.thread_id())
    );
    let id = child.thread_id();
    drop(child);
    drop(repository);
    let reopened = Repository::open(directory.path()).expect("reopen");
    assert_eq!(reopened.native_spool_id().expect("same spool"), spool);
    assert_eq!(
        reopened
            .native_thread("main")
            .expect("same main")
            .signed_genesis()
            .expect("proof"),
        original
    );
    assert_eq!(
        reopened
            .native_thread("feature")
            .expect("same child")
            .thread_id(),
        id
    );
    reopened
        .rename_native_thread("feature", "renamed")
        .expect("rename address");
    assert_eq!(
        reopened
            .native_thread("renamed")
            .expect("renamed Thread")
            .thread_id(),
        id
    );
    assert!(reopened.native_thread("feature").is_err());
    assert!(
        reopened
            .install_native_spool_id(uuid::Uuid::now_v7())
            .is_err(),
        "local identity cannot be silently replaced by publish"
    );
    assert!(
        reopened
            .create_native_thread("renamed", base, Some("main"), "different intent")
            .is_err(),
        "name cannot silently bind different genesis"
    );
}

#[test]
fn local_captures_keep_checkout_parentage_and_reuse_the_original_operation() {
    use objects::{
        object::{Attribution, Principal, State, thread_replication::ThreadFacet},
        store::ObjectStore as _,
    };
    let directory = tempfile::tempdir().expect("repository");
    let repository = Repository::init_default(directory.path()).expect("native init");
    let base = repository.head().expect("head").expect("base");
    let tree = repository
        .store()
        .get_state(&base)
        .expect("base state")
        .expect("state")
        .tree;
    let author = Attribution::human(Principal::new("Developer", "developer@example.test"));
    let left = State::new_snapshot(tree, vec![base], author.clone()).with_intent("left checkout");
    let right = State::new_snapshot(tree, vec![base], author.clone()).with_intent("right checkout");
    repository.store().put_state(&left).expect("capture left");
    repository.store().put_state(&right).expect("capture right");
    let left_op = repository
        .record_native_capture("main", left.id())
        .expect("record left");
    repository
        .record_native_capture("main", right.id())
        .expect("record right");
    assert_eq!(
        repository
            .record_native_capture("main", left.id())
            .expect("retry"),
        left_op
    );
    let child = State::new_snapshot(tree, vec![left.id()], author).with_intent("continue left");
    repository.store().put_state(&child).expect("capture child");
    let child_op = repository
        .record_native_capture("main", child.id())
        .expect("record child");
    let replica = repository.native_thread("main").expect("replica");
    let operations = replica
        .accepted_page(ThreadFacet::Source, None, 20)
        .expect("operations");
    assert_eq!(
        operations.len(),
        3,
        "retry cannot produce a duplicate operation"
    );
    let signed = operations
        .iter()
        .find(|(id, _)| *id == child_op)
        .expect("child proof");
    assert_eq!(
        signed.1.verify().expect("original proof").parents,
        [left_op].into()
    );
    let heads = replica.view().expect("view").source_heads;
    assert_eq!(
        heads,
        [right.id(), child.id()].into(),
        "concurrent checkout is not implicitly merged"
    );
}

#[test]
fn cross_thread_merge_records_local_integration_not_capture() {
    use objects::{
        object::{Attribution, Principal, State, ThreadName, thread_replication::ThreadFacet},
        store::ObjectStore as _,
    };
    use refs::Head;
    let directory = tempfile::tempdir().expect("repository");
    let repository = Repository::init_default(directory.path()).expect("native init");
    let base = repository.head().expect("head").expect("base");
    let tree = repository
        .store()
        .get_state(&base)
        .expect("base state")
        .expect("state")
        .tree;
    let author = Attribution::human(Principal::new("Developer", "developer@example.test"));
    let on_main = State::new_snapshot(tree, vec![base], author.clone()).with_intent("main work");
    repository.store().put_state(&on_main).expect("main state");
    repository
        .record_native_capture("main", on_main.id())
        .expect("record main");
    repository
        .set_thread_recorded(&ThreadName::new("main"), &on_main.id())
        .expect("advance main");
    repository
        .write_head_recorded(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .expect("attach main");
    let source = repository
        .create_native_thread("feature", on_main.id(), Some("main"), "fork")
        .expect("feature");
    let on_feature =
        State::new_snapshot(tree, vec![on_main.id()], author.clone()).with_intent("feature work");
    repository
        .store()
        .put_state(&on_feature)
        .expect("feature state");
    let source_operation = repository
        .record_native_capture("feature", on_feature.id())
        .expect("record feature");
    let merged = repository
        .snapshot_merge_with_attribution(
            &on_feature.id(),
            Some("Refresh feature onto main".into()),
            None,
            author,
            Some(on_main.id()),
            false,
        )
        .expect("cross-thread merge snapshot");
    let replica = repository.native_thread("main").expect("main replica");
    let recorded = replica
        .source_operation_page(merged.id(), None, 4)
        .expect("recorded merge");
    assert_eq!(recorded.len(), 1, "merge records one native operation");
    let (signed, _) = replica
        .operation(&recorded[0])
        .expect("load merge operation")
        .expect("merge operation present");
    let operation = signed.verify().expect("merge signature");
    let receipt = operation
        .local_integration()
        .expect("decode")
        .expect("LocalIntegration, not Capture");
    assert_eq!(receipt.source_thread, source.thread_id());
    assert_eq!(receipt.source_operation, source_operation);
    assert_eq!(receipt.source_revision, on_feature.id());
    assert_eq!(receipt.target_thread, replica.thread_id());
    let heads = replica.view().expect("view").source_heads;
    assert_eq!(
        heads,
        [merged.id()].into(),
        "integration replaces the pre-merge target head"
    );
    let source_heads = source.view().expect("source view").source_heads;
    assert_eq!(
        source_heads,
        [on_feature.id()].into(),
        "source Thread is not rewritten by landing into the target"
    );
    let facet = replica
        .accepted_page(ThreadFacet::Source, None, 20)
        .expect("source facet");
    assert!(
        facet.iter().all(|(_, signed)| {
            signed
                .verify()
                .expect("source op")
                .source_state()
                .expect("state")
                .is_some_and(|state| {
                    state.id() != merged.id()
                        || signed
                            .verify()
                            .expect("source op")
                            .local_integration()
                            .expect("kind")
                            .is_some()
                })
        }),
        "merge snapshot must not be admitted as a Capture"
    );
}

#[test]
fn checkout_mutations_share_exclusive_leases_and_release_temporary_writers() {
    use objects::store::{WriterLeaseStatus, WriterLeaseStore};
    use repo::thread_replication::checkout::ThreadCheckout;
    let directory = tempfile::tempdir().expect("repository");
    let source = directory.path().join("source");
    std::fs::create_dir(&source).expect("source directory");
    let repository = Repository::init_default(&source).expect("native init");
    let replica = repository.native_thread("main").expect("Thread");
    let thread = replica.thread_id();
    let base = repository.head().expect("head").expect("base");
    let sibling = ThreadCheckout::create(
        &repository,
        &replica,
        &directory.path().join("sibling"),
        base,
        &repo::AudienceTier::Internal,
    )
    .expect("separate checkout");
    let store = WriterLeaseStore::new(repository.heddle_dir());
    for _ in 0..2 {
        let guard = repository
            .acquire_checkout_writer(thread, "agent-a")
            .expect("root writer");
        assert!(
            repository
                .acquire_checkout_writer(thread, "agent-b")
                .is_err(),
            "same physical checkout must exclude another writer"
        );
        let sibling_guard = sibling
            .repository
            .acquire_checkout_writer(thread, "agent-b")
            .expect("separate checkout may write same Thread");
        sibling_guard.finish().expect("release sibling");
        guard.finish().expect("release root");
        assert!(
            store
                .list_without_reaping()
                .expect("persisted leases")
                .iter()
                .all(|lease| lease.status == WriterLeaseStatus::Complete),
            "teardown persisted after each run"
        );
    }
    let owner = sibling
        .claim_writer("persistent-agent".into(), Some(std::process::id()))
        .expect("persistent owner");
    assert!(
        sibling
            .repository
            .acquire_checkout_writer(thread, "cli")
            .is_err(),
        "CLI cannot bypass persistent agent reservation"
    );
    assert!(
        sibling
            .repository
            .authenticate_checkout_writer(thread, &owner.lease.lease_id, "wrong-token")
            .is_err()
    );
    let guard = sibling
        .repository
        .authenticate_checkout_writer(thread, &owner.lease.lease_id, &owner.token)
        .expect("agent authenticates its lease");
    assert!(
        sibling
            .repository
            .authenticate_checkout_writer(thread, &owner.lease.lease_id, &owner.token)
            .is_err(),
        "same token cannot run simultaneous mutations"
    );
    guard.finish().expect("finish mutation");
    assert!(
        store
            .list()
            .expect("leases")
            .iter()
            .any(|lease| lease.lease_id == owner.lease.lease_id
                && lease.status == WriterLeaseStatus::Active),
        "persistent writer survives between commands"
    );
}
