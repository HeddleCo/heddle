use std::{cell::Cell, fs, process::Command};

use super::*;
thread_local! {
    static FAILURE: Cell<Option<(usize, &'static str)>> = const { Cell::new(None) };
    static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static CRASH_COUNT: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn checkpoint(step: &str) {
    if step == "file-flush" {
        HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
    }
    if std::env::var("HEDDLE_INSTALL_CRASH_STEP").is_ok_and(|s| s == step) {
        CRASH_COUNT.with(|count| {
            count.set(count.get() + 1);
            let occurrence: usize = std::env::var("HEDDLE_INSTALL_CRASH_OCCURRENCE")
                .expect("occurrence")
                .parse()
                .expect("number");
            if count.get() == occurrence {
                std::process::exit(86);
            }
        });
    }
}
pub(super) fn rollback_fault(index: usize, operation: &'static str) -> Result<()> {
    if FAILURE.with(|failure| failure.get() == Some((index, operation))) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("injected rollback {operation} at {index}"),
        )
        .into());
    }
    Ok(())
}
pub(in crate::thread_replication) fn fail_rollback(index: usize, operation: &'static str) {
    FAILURE.with(|failure| failure.set(Some((index, operation))));
}
pub(in crate::thread_replication) fn clear_failure() {
    FAILURE.with(|failure| failure.set(None));
}

#[test]
fn relative_destinations_reject_traversal() {
    let directory = tempfile::tempdir().expect("root");
    let repository = directory.path().join("repo");
    fs::create_dir(&repository).expect("repo");
    let serialization = InstallationLock::acquire(&repository).expect("exclusive");
    let mut journal = Installation::begin(&serialization).expect("journal");
    for path in [
        directory.path().join("escape"),
        PathBuf::from("../escape"),
        PathBuf::from("nested/../../escape"),
    ] {
        assert!(
            journal.writer().write_file(&path, b"escaped").is_err(),
            "accepted {}",
            path.display()
        );
    }
    assert!(!directory.path().join("escape").exists());
    assert!(
        journal
            .writer()
            .write_file(Path::new("metadata.sqlite3"), b"db")
            .is_err()
    );
    assert!(
        journal
            .writer()
            .write_file(Path::new("locks/repo.lock"), b"lock")
            .is_err()
    );
}
#[cfg(unix)]
#[test]
fn parent_symlink_substitution_cannot_escape_repository() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().expect("root");
    let repository = directory.path().join("repo");
    let outside = directory.path().join("outside");
    fs::create_dir(&repository).expect("repo");
    fs::create_dir(&outside).expect("outside");
    fs::create_dir(repository.join("parent")).expect("parent");
    let serialization = InstallationLock::acquire(&repository).expect("exclusive");
    let mut journal = Installation::begin(&serialization).expect("journal");
    fs::rename(repository.join("parent"), repository.join("held")).expect("substitute parent");
    symlink(&outside, repository.join("parent")).expect("symlink");
    assert!(
        journal
            .writer()
            .write_file(Path::new("parent/pin"), b"escaped")
            .is_err()
    );
    assert!(!outside.join("pin").exists());
}
#[cfg(unix)]
#[test]
fn repository_root_replacement_cannot_redirect_journal_begin() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().expect("root");
    let repository = directory.path().join("repo");
    let outside = directory.path().join("outside");
    fs::create_dir(&repository).expect("repo");
    fs::create_dir(&outside).expect("outside");
    fs::write(repository.join("pin"), b"old").expect("old pin");
    fs::write(outside.join("pin"), b"outside").expect("outside pin");
    let serialization = InstallationLock::acquire(&repository).expect("selected repository");
    fs::rename(&repository, directory.path().join("held")).expect("replace repository name");
    symlink(&outside, &repository).expect("substitution");
    let mut journal = Installation::begin(&serialization).expect("clone selected capability");
    journal
        .writer()
        .write_file(Path::new("pin"), b"new")
        .expect("held publication");
    assert_eq!(fs::read(outside.join("pin")).expect("outside"), b"outside");
    let mut db = Connection::open_in_memory().expect("sql");
    let tx = db.transaction().expect("tx");
    assert!(journal.mark(&tx).is_err(), "replaced root cannot commit");
    tx.rollback().expect("rollback sql");
    journal
        .rollback()
        .expect("rollback through selected root handle");
    assert_eq!(
        fs::read(directory.path().join("held/pin")).expect("restored"),
        b"old"
    );
    assert!(!outside.join(INTENT).exists());
}
#[cfg(unix)]
#[test]
fn destination_parent_replacement_uses_held_directory() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().expect("root");
    let repository = directory.path().join("repo");
    let outside = directory.path().join("outside");
    fs::create_dir(&repository).expect("repo");
    fs::create_dir(&outside).expect("outside");
    fs::create_dir(repository.join("parent")).expect("parent");
    fs::write(repository.join("parent/pin"), b"old").expect("pin");
    fs::write(outside.join("pin"), b"outside").expect("outside pin");
    let mut db = crate::local_metadata::open(&repository).expect("db");
    let serialization = InstallationLock::acquire(&repository).expect("exclusive");
    let mut journal = Installation::begin(&serialization).expect("journal");
    let repo_for_hook = repository.clone();
    let outside_for_hook = outside.clone();
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::rename(repo_for_hook.join("parent"), repo_for_hook.join("held"))
                .expect("replace parent");
            symlink(&outside_for_hook, repo_for_hook.join("parent")).expect("symlink substitution");
        }))
    });
    journal
        .writer()
        .write_file(Path::new("parent/pin"), b"new")
        .expect("publish through held handle");
    let tx = db.transaction().expect("tx");
    assert!(
        journal.mark(&tx).is_err(),
        "commit must reject changed parent binding"
    );
    tx.rollback().expect("sql rollback");
    journal.rollback().expect("restore through held handle");
    assert_eq!(fs::read(outside.join("pin")).expect("outside"), b"outside");
    assert_eq!(
        fs::read(repository.join("held/pin")).expect("restored old parent"),
        b"old"
    );
}
#[test]
fn failed_rollback_keeps_independent_entries_and_durable_undo() {
    for failed in 0..4 {
        let dir = tempfile::tempdir().expect("repository");
        let _db = crate::local_metadata::open(dir.path()).expect("metadata");
        for index in [0, 2] {
            fs::write(dir.path().join(format!("pin{index}")), b"old").expect("old");
        }
        let serialization = InstallationLock::acquire(dir.path()).expect("exclusive");
        let mut journal = Installation::begin(&serialization).expect("journal");
        for index in 0..4 {
            journal
                .writer()
                .write_file(Path::new(&format!("pin{index}")), b"new")
                .expect("install");
        }
        fail_rollback(failed, if failed % 2 == 0 { "rename" } else { "unlink" });
        assert!(journal.rollback().is_err());
        clear_failure();
        for index in 0..4 {
            let pin = dir.path().join(format!("pin{index}"));
            if index == failed {
                assert_eq!(fs::read(pin).expect("retained failed entry"), b"new");
            } else if index % 2 == 0 {
                assert_eq!(fs::read(pin).expect("independent restore"), b"old");
            } else {
                assert!(!pin.exists(), "independent unlink {index}");
            }
        }
        assert!(dir.path().join(INTENT).exists());
        assert!(
            dir.path()
                .join(artifact_name(journal.intent.id, 0, "backup"))
                .exists()
        );
        drop(journal);
        crate::local_metadata::open(dir.path()).expect("open retries undo");
        crate::local_metadata::open(dir.path()).expect("repeat recovery");
        for index in 0..4 {
            let pin = dir.path().join(format!("pin{index}"));
            if index % 2 == 0 {
                assert_eq!(fs::read(pin).expect("recovered"), b"old");
            } else {
                assert!(!pin.exists());
            }
        }
        assert!(!dir.path().join(INTENT).exists());
    }
}
#[test]
fn process_exit_recovery_is_repeatable_at_every_journal_step() {
    let install_steps = [
        ("intent", 3),
        ("backup", 2),
        ("prepared", 3),
        ("file-flush", 4),
        ("publish", 4),
        ("marker", 1),
        ("before-commit", 1),
        ("commit", 1),
        ("done", 1),
        ("cleanup", 12),
        ("retired", 1),
    ];
    for (step, occurrences) in install_steps {
        for occurrence in 1..=occurrences {
            let directory = tempfile::tempdir().expect("repo");
            setup_crash_repo(directory.path());
            crash_child(directory.path(), "install", step, occurrence);
            let committed = matches!(step, "commit" | "done" | "cleanup" | "retired");
            crate::local_metadata::open(directory.path()).expect("reopen recovery");
            assert_reconciled(directory.path(), committed);
            crate::local_metadata::open(directory.path()).expect("idempotent reopen");
            assert_reconciled(directory.path(), committed);
        }
    }
    // Crash the recovery itself after every undo/cleanup position, then reopen.
    for (step, occurrences) in [
        ("rollback", 3),
        ("done", 1),
        ("cleanup", 12),
        ("retired", 1),
    ] {
        for occurrence in 1..=occurrences {
            let directory = tempfile::tempdir().expect("repo");
            setup_crash_repo(directory.path());
            crash_child(directory.path(), "install", "before-commit", 1);
            crash_child(directory.path(), "recover", step, occurrence);
            crate::local_metadata::open(directory.path())
                .expect("repeated recovery after second exit");
            assert_reconciled(directory.path(), false);
        }
    }
}
fn setup_crash_repo(dir: &Path) {
    let db = crate::local_metadata::open(dir).expect("metadata");
    db.execute_batch("CREATE TABLE crash_admitted(id INTEGER)")
        .expect("test state");
    fs::write(dir.join("pin0"), b"old").expect("old");
    fs::write(dir.join("pin2"), b"old").expect("old");
}
fn assert_reconciled(dir: &Path, committed: bool) {
    let db = crate::local_metadata::open(dir).expect("recovered metadata");
    let admitted: i64 = db
        .query_row("SELECT count(*) FROM crash_admitted", [], |r| r.get(0))
        .expect("admission");
    assert_eq!(admitted, i64::from(committed));
    for index in [0, 2] {
        assert_eq!(
            fs::read(dir.join(format!("pin{index}"))).expect("pin"),
            if committed {
                b"new".as_slice()
            } else {
                b"old".as_slice()
            }
        );
    }
    assert_eq!(dir.join("pin1").exists(), committed);
    assert!(
        !dir.join(INTENT).exists(),
        "undo record cleaned only after reconciliation"
    );
}
fn crash_child(dir: &Path, mode: &str, step: &str, occurrence: usize) {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "thread_replication::install_artifacts::tests::installation_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("HEDDLE_INSTALL_CRASH_DIR", dir)
        .env("HEDDLE_INSTALL_CRASH_MODE", mode)
        .env("HEDDLE_INSTALL_CRASH_STEP", step)
        .env("HEDDLE_INSTALL_CRASH_OCCURRENCE", occurrence.to_string())
        .output()
        .expect("child");
    assert_eq!(
        output.status.code(),
        Some(86),
        "{mode} {step}:{occurrence}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
#[ignore = "entry point for the process-exit recovery test"]
fn installation_crash_child() {
    let directory =
        PathBuf::from(std::env::var_os("HEDDLE_INSTALL_CRASH_DIR").expect("child directory"));
    if std::env::var("HEDDLE_INSTALL_CRASH_MODE").expect("mode") == "recover" {
        crate::local_metadata::open(&directory).expect("recover");
    } else {
        let serialization = InstallationLock::acquire(&directory).expect("exclusive");
        let mut db = crate::local_metadata::open(&directory).expect("db");
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("tx");
        tx.execute("INSERT INTO crash_admitted VALUES(1)", [])
            .expect("admit");
        let mut journal = Installation::begin(&serialization).expect("journal");
        for index in 0..3 {
            journal
                .writer()
                .write_file(Path::new(&format!("pin{index}")), b"new")
                .expect("install");
        }
        journal
            .writer()
            .write_file(Path::new("pin2"), b"new")
            .expect("repeat destination");
        journal.mark(&tx).expect("same-transaction marker");
        checkpoint("before-commit");
        tx.commit().expect("commit");
        checkpoint("commit");
        journal.finish().expect("cleanup");
    }
    panic!("crash point was not reached");
}
