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
    let child = repository.create_native_thread("feature", base, Some("main"), "edit docs").expect("new native Thread");
    assert_eq!(child.genesis().expect("child genesis").parent, Some(main.thread_id()));
    let id = child.thread_id();
    drop(child);
    drop(repository);
    let reopened = Repository::open(directory.path()).expect("reopen");
    assert_eq!(reopened.native_spool_id().expect("same spool"), spool);
    assert_eq!(reopened.native_thread("main").expect("same main").signed_genesis().expect("proof"), original);
    assert_eq!(reopened.native_thread("feature").expect("same child").thread_id(), id);
    assert!(reopened.install_native_spool_id(uuid::Uuid::now_v7()).is_err(), "local identity cannot be silently replaced by publish");
    assert!(reopened.create_native_thread("feature", base, Some("main"), "different intent").is_err(), "name cannot silently bind different genesis");
}
