use api::heddle::api::v2alpha1 as wire;
use prost::Message;
use store::{Catalog, SpoolRecord};

use super::*;

fn registration(home: &Path, id: uuid::Uuid) -> DeviceSpool {
    let root = home.join(id.to_string());
    let heddle_dir = root.join(".heddle");
    std::fs::create_dir_all(&heddle_dir).expect("local binding");
    std::fs::write(heddle_dir.join("spool-id"), id.to_string()).expect("stable ID");
    DeviceSpool {
        id,
        root,
        heddle_dir,
        capability_path: id.to_string(),
    }
}
fn insert(home: &Path, catalog: &mut Catalog) -> SpoolRecord {
    let id = uuid::Uuid::now_v7();
    let registration = registration(home, id);
    catalog
        .register(
            &registration,
            wire::SpoolOverview {
                name: "Local work".into(),
                slug: id.to_string(),
                settings: Some(wire::SpoolSettings::default()),
                ..Default::default()
            },
        )
        .expect("registration");
    catalog.spool(id).expect("lookup").expect("registered")
}
#[test]
fn catalog_receipts_cas_and_state_commit_atomically() {
    let home = tempfile::tempdir().expect("home");
    let mut catalog = Catalog::open(home.path()).expect("catalog");
    let before = insert(home.path(), &mut catalog);
    let account = uuid::Uuid::new_v4().to_string();
    let operation = uuid::Uuid::new_v4().to_string();
    let settings = wire::SpoolSettings {
        description: "atomic settings".into(),
        ..Default::default()
    };
    let generation = catalog.generation().expect("generation");
    let changed: wire::SpoolOverview = catalog
        .mutate(&account, "ReviseSpool", &operation, b"same-request", |tx| {
            mutations::revise(
                tx,
                before.registration.id,
                &before.overview.version,
                "Updated",
                &settings,
            )
        })
        .expect("mutation");
    assert_ne!(changed.version, before.overview.version);
    assert!(catalog.generation().expect("generation") > generation);
    let marker = std::fs::read(store::database_path(home.path()).with_extension("sqlite3.changed"))
        .expect("post-commit wakeup");
    assert_eq!(
        marker,
        catalog
            .generation()
            .expect("durable generation")
            .to_be_bytes()
    );
    let retry: wire::SpoolOverview = catalog
        .mutate(&account, "ReviseSpool", &operation, b"same-request", |_| {
            panic!("exact retry must not execute again")
        })
        .expect("exact durable retry");
    assert_eq!(retry, changed);
    assert!(
        catalog
            .mutate::<wire::SpoolOverview>(
                &account,
                "ReviseSpool",
                &operation,
                b"changed-request",
                |_| panic!("ID mismatch must not execute")
            )
            .is_err()
    );
    let stale = catalog.mutate::<wire::SpoolOverview>(
        &account,
        "ReviseSpool",
        &uuid::Uuid::new_v4().to_string(),
        b"stale",
        |tx| {
            mutations::revise(
                tx,
                before.registration.id,
                &before.overview.version,
                "Lost update",
                &settings,
            )
        },
    );
    assert!(
        stale
            .expect_err("stale property CAS")
            .to_string()
            .contains("version changed")
    );
    let version = changed.version.clone();
    let rollback = uuid::Uuid::new_v4().to_string();
    assert!(
        catalog
            .mutate::<wire::SpoolOverview>(&account, "ReviseSpool", &rollback, b"rollback", |tx| {
                mutations::revise(
                    tx,
                    before.registration.id,
                    &version,
                    "Uncommitted",
                    &settings,
                )?;
                anyhow::bail!("injected transaction failure")
            })
            .is_err()
    );
    drop(catalog);
    let mut reopened = Catalog::open(home.path()).expect("restart");
    assert_eq!(
        reopened
            .spool(before.registration.id)
            .expect("lookup")
            .expect("record")
            .overview,
        changed
    );
    reopened
        .mutate::<wire::SpoolOverview>(&account, "ReviseSpool", &rollback, b"rollback", |tx| {
            mutations::revise(
                tx,
                before.registration.id,
                &version,
                "Committed retry",
                &settings,
            )
        })
        .expect("failed command did not retain receipt");
}
#[test]
fn catalog_mounts_keep_exact_identity_retarget_and_reject_cycles() {
    let home = tempfile::tempdir().expect("home");
    let mut catalog = Catalog::open(home.path()).expect("catalog");
    let a = insert(home.path(), &mut catalog);
    let b = insert(home.path(), &mut catalog);
    let c = insert(home.path(), &mut catalog);
    let account = uuid::Uuid::new_v4().to_string();
    let request = wire::SetSpoolMountRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        expected_version: vec![],
        mount: Some(wire::SpoolMount {
            r#ref: Some(wire::RecordRef {
                id: uuid::Uuid::new_v4().to_string(),
                spool: a.overview.r#ref.clone(),
            }),
            parent: a.overview.r#ref.clone(),
            child: b.overview.r#ref.clone(),
            name: "mounted".into(),
            version: vec![],
        }),
    };
    let first: wire::SpoolMount = catalog
        .mutate(
            &account,
            "SetSpoolMount",
            &request.client_operation_id,
            &request.encode_to_vec(),
            |tx| mutations::mount(tx, &request),
        )
        .expect("mount");
    let cycle = wire::SetSpoolMountRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        expected_version: vec![],
        mount: Some(wire::SpoolMount {
            r#ref: Some(wire::RecordRef {
                id: uuid::Uuid::new_v4().to_string(),
                spool: b.overview.r#ref.clone(),
            }),
            parent: b.overview.r#ref.clone(),
            child: a.overview.r#ref.clone(),
            name: "cycle".into(),
            version: vec![],
        }),
    };
    assert!(
        catalog
            .mutate::<wire::SpoolMount>(
                &account,
                "SetSpoolMount",
                &cycle.client_operation_id,
                &cycle.encode_to_vec(),
                |tx| mutations::mount(tx, &cycle)
            )
            .expect_err("cycle denied")
            .to_string()
            .contains("cycle")
    );
    let mut retarget = request.clone();
    retarget.client_operation_id = uuid::Uuid::new_v4().to_string();
    retarget.expected_version = first.version.clone();
    retarget.mount.as_mut().expect("mount").child = c.overview.r#ref.clone();
    let second: wire::SpoolMount = catalog
        .mutate(
            &account,
            "SetSpoolMount",
            &retarget.client_operation_id,
            &retarget.encode_to_vec(),
            |tx| mutations::mount(tx, &retarget),
        )
        .expect("atomic retarget");
    assert_eq!(second.r#ref, first.r#ref);
    assert_ne!(second.version, first.version);
    assert_eq!(second.child, c.overview.r#ref);
    let deletion = catalog.mutate::<wire::SpoolOverview>(
        &account,
        "DeleteSpool",
        &uuid::Uuid::new_v4().to_string(),
        b"delete-mounted",
        |tx| {
            mutations::delete(tx, c.registration.id, &c.overview.version)?;
            Ok(c.overview.clone())
        },
    );
    assert!(
        deletion
            .expect_err("mount target retained")
            .to_string()
            .contains("children or mounts")
    );
}
#[test]
fn catalog_pages_bound_bytes_and_physical_bindings_survive_restart() {
    let home = tempfile::tempdir().expect("home");
    let mut catalog = Catalog::open(home.path()).expect("catalog");
    let first = insert(home.path(), &mut catalog);
    let second = insert(home.path(), &mut catalog);
    let page = catalog
        .spools("", 1, store::MAX_PAGE_BYTES)
        .expect("bounded page");
    assert_eq!(page.records.len(), 1);
    assert!(page.has_more);
    let next = catalog
        .spools(
            &page.records[0].registration.id.to_string(),
            1,
            store::MAX_PAGE_BYTES,
        )
        .expect("keyset next");
    assert_eq!(next.records.len(), 1);
    assert!(!next.has_more);
    assert!(
        catalog
            .spools("", 1, 1)
            .expect_err("record cannot silently exceed requested byte budget")
            .to_string()
            .contains("byte budget")
    );
    drop(catalog);
    assert_eq!(
        load(home.path(), first.registration.id)
            .expect("durable binding")
            .root,
        first.registration.root
    );
    set_capability_path(home.path(), first.registration.id, "account/spool")
        .expect("authenticated path");
    assert_eq!(
        load(home.path(), first.registration.id)
            .expect("changed path")
            .capability_path,
        "account/spool"
    );
    std::fs::write(
        second.registration.heddle_dir.join("spool-id"),
        uuid::Uuid::new_v4().to_string(),
    )
    .expect("changed repository");
    assert!(
        load(home.path(), second.registration.id)
            .expect_err("physical identity binding")
            .to_string()
            .contains("changed spool identity")
    );
}

#[test]
fn catalog_mount_traversal_rejects_more_than_1024_distinct_descendants() {
    let home = tempfile::tempdir().expect("home");
    let mut catalog = Catalog::open(home.path()).expect("catalog");
    let parent = insert(home.path(), &mut catalog);
    let account = uuid::Uuid::new_v4().to_string();
    let root = uuid::Uuid::now_v7();
    catalog
        .mutate::<wire::SpoolOverview>(
            &account,
            "fixture-tree",
            &uuid::Uuid::new_v4().to_string(),
            b"bounded-tree",
            |tx| {
                let mut previous = None;
                for index in 0..1025 {
                    let id = if index == 0 {
                        root
                    } else {
                        uuid::Uuid::now_v7()
                    };
                    let registration = registration(home.path(), id);
                    let overview = wire::SpoolOverview {
                        parent: previous
                            .map(|id: uuid::Uuid| wire::SpoolRef { id: id.to_string() }),
                        name: "tree".into(),
                        slug: id.to_string(),
                        ..Default::default()
                    };
                    store::insert_spool_in(tx, &registration, &overview)?;
                    previous = Some(id);
                }
                Ok(wire::SpoolOverview::default())
            },
        )
        .expect("valid finite local hierarchy");
    let request = wire::SetSpoolMountRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        expected_version: vec![],
        mount: Some(wire::SpoolMount {
            r#ref: Some(wire::RecordRef {
                id: uuid::Uuid::new_v4().to_string(),
                spool: parent.overview.r#ref.clone(),
            }),
            parent: parent.overview.r#ref.clone(),
            child: Some(wire::SpoolRef {
                id: root.to_string(),
            }),
            name: "oversized".into(),
            version: vec![],
        }),
    };
    assert!(
        catalog
            .mutate::<wire::SpoolMount>(
                &account,
                "SetSpoolMount",
                &request.client_operation_id,
                &request.encode_to_vec(),
                |tx| mutations::mount(tx, &request)
            )
            .expect_err("traversal is bounded even without a cycle")
            .to_string()
            .contains("traversal bound")
    );
}
