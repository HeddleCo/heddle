//! Daemon-owned expiration. Workers belong to registered stores, never streams.
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use anyhow::Result;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub(crate) struct Retention {
    task: JoinHandle<()>,
}
impl Drop for Retention {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Retention {
    pub(crate) fn start(home: PathBuf) -> Self {
        Self {
            task: tokio::spawn(async move {
                // Only failures retry on a timer. Healthy idle stores issue no SQL.
                loop {
                    if let Err(error) = supervise(home.clone()).await {
                        tracing::warn!(%error, "device artifact retention restarting");
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }),
        }
    }
}

async fn supervise(home: PathBuf) -> Result<()> {
    let directory = home.join("state/device-rpc");
    objects::fs_atomic::create_private_dir_all(&directory)?;
    let (changes, mut changed) = tokio::sync::watch::channel(false);
    let _watch = repo::device_watch::watch_directory_filtered(
        &directory,
        |path| {
            path.file_name()
                .is_some_and(|name| name == "catalog.sqlite3.changed")
        },
        move |result| {
            changes.send_modify(|failed| *failed |= result.is_err());
        },
    )?;
    // Abort workers when the daemon or supervisor goes away.
    let mut workers: BTreeMap<PathBuf, (uuid::Uuid, Retention)> = BTreeMap::new();
    loop {
        if *changed.borrow_and_update() {
            anyhow::bail!("catalog watch continuity lost");
        }
        let root = home.clone();
        let stores = tokio::task::spawn_blocking(move || {
            let Some(catalog) = repo::device_catalog::store::Catalog::read(&root)? else {
                return Ok(Vec::new());
            };
            let mut directories = Vec::new();
            for entry in catalog.retention_registrations()? {
                let actual = std::fs::read_to_string(entry.heddle_dir.join("spool-id"));
                if actual.as_deref().is_ok_and(|id| id.trim() == entry.id.to_string()) {
                    directories.push((entry.heddle_dir, entry.id));
                } else {
                    tracing::warn!(spool = %entry.id, "retention skipped unavailable or changed physical Spool binding");
                }
            }
            Ok::<_, anyhow::Error>(directories)
        })
        .await??;
        let stores: BTreeMap<_, _> = stores.into_iter().collect();
        workers.retain(|path, (id, _)| stores.get(path) == Some(id));
        for (directory, spool) in stores {
            workers.entry(directory.clone()).or_insert_with(|| {
                (
                    spool,
                    Retention {
                        task: tokio::spawn(async move {
                            loop {
                                if let Err(error) = expire_store(directory.clone(), spool).await {
                                    tracing::warn!(%error, "artifact store retention retry");
                                }
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                        }),
                    },
                )
            });
        }
        changed.changed().await?;
    }
}

async fn expire_store(directory: PathBuf, spool: uuid::Uuid) -> Result<()> {
    let (changes, mut changed) = tokio::sync::watch::channel(false);
    let _watch = repo::device_watch::watch_directory_filtered(
        &directory,
        |path| {
            path.file_name().is_some_and(|name| {
                name == repo::local_metadata::CHANGE_MARKER_NAME
                    || name == repo::local_metadata::DATABASE_NAME
                    || name == "metadata.sqlite3-wal"
            })
        },
        move |result| {
            changes.send_modify(|failed| *failed |= result.is_err());
        },
    )?;
    let mut held = None;
    loop {
        // Mark the wake before reading committed state: a concurrent write remains pending.
        if *changed.borrow_and_update() {
            anyhow::bail!("artifact watch continuity lost");
        }
        let path = directory.clone();
        let (pin, deadline) = tokio::task::spawn_blocking(move || -> Result<_> {
            if std::fs::read_to_string(path.join("spool-id"))?.trim() != spool.to_string() {
                anyhow::bail!("artifact store physical identity changed");
            }
            if !path
                .join(repo::local_metadata::DATABASE_NAME)
                .try_exists()?
            {
                return Ok((held, None));
            }
            if held.is_none() {
                held = Some(repo::device_watch::hold_replica_database(&path)?);
            }
            let store = repo::device_artifacts::ArtifactStore::open(&path)?;
            let now = chrono::Utc::now().timestamp();
            let purged = store.purge_expired(now, 128)?;
            let next = store.next_expiry()?;
            // An overdue locked batch must not spin; successful full batches continue promptly.
            let next = next.map(|at| {
                if at <= now && purged == 0 {
                    now + 1
                } else {
                    at
                }
            });
            Ok((held, next))
        })
        .await??;
        held = pin;
        if let Some(deadline) = deadline {
            let seconds = deadline
                .saturating_sub(chrono::Utc::now().timestamp())
                .max(0) as u64;
            tokio::select! {
                result = changed.changed() => { result?; }
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
            }
        } else {
            changed.changed().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v2alpha1::*;

    use super::*;

    #[tokio::test]
    async fn expires_without_observers_and_stops_with_daemon() {
        let home = tempfile::tempdir().expect("device home");
        let root = tempfile::tempdir().expect("owned store");
        let directory = root.path().join(".heddle");
        std::fs::create_dir(&directory).expect("metadata directory");
        let spool = uuid::Uuid::new_v4();
        std::fs::write(directory.join("spool-id"), spool.to_string()).expect("physical binding");
        let runs = repo::device_runs::RunStore::open(&directory).expect("runs");
        let run = RecordRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: "retention-test".into(),
        };
        runs.put_run(RunRecord {
            r#ref: Some(run.clone()),
            ..Default::default()
        })
        .expect("Run");
        runs.put_policy(
            &PutRunPolicyRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                policy: Some(RunPolicy {
                    spool: run.spool.clone(),
                    retain_raw: true,
                    raw_retention_seconds: 2,
                    ..Default::default()
                }),
                ..Default::default()
            },
            "owning-human",
        )
        .expect("opt in");
        let artifacts = repo::device_artifacts::ArtifactStore::open(&directory).expect("artifacts");
        let retain = |bytes: &[u8]| {
            let record = artifacts
                .retain(
                    &run,
                    "report",
                    "text/plain",
                    bytes,
                    chrono::Utc::now().timestamp(),
                )
                .expect("retain");
            directory
                .join("retained-artifacts")
                .join(record.r#ref.expect("artifact ref").id)
        };
        let first = retain(b"before daemon starts");
        assert!(first.exists());
        let registration = repo::device_catalog::DeviceSpool {
            id: spool,
            root: root.path().to_owned(),
            heddle_dir: directory.clone(),
            capability_path: spool.to_string(),
        };
        let daemon = Retention::start(home.path().to_owned());
        // Registration arrives after startup; no browser stream or artifact read exists.
        repo::device_catalog::store::Catalog::open(home.path())
            .expect("catalog")
            .register(
                &registration,
                SpoolOverview {
                    name: "owned".into(),
                    slug: "owned".into(),
                    ..Default::default()
                },
            )
            .expect("register");
        removed(&first, &artifacts).await;
        assert_eq!(artifacts.next_expiry().expect("next expiry"), None);
        let second = retain(b"created while daemon idle");
        assert!(second.exists());
        removed(&second, &artifacts).await;
        assert_eq!(
            artifacts
                .purge_expired(chrono::Utc::now().timestamp(), 128)
                .expect("repeat cleanup"),
            0
        );
        drop(daemon);
        // Let cancellation propagate before publishing the next independent item.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let third = retain(b"after shutdown");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            third.exists(),
            "daemon shutdown must stop background deletion"
        );
        let mut catalog = repo::device_catalog::store::Catalog::open(home.path()).expect("catalog");
        let version = catalog
            .spool(spool)
            .expect("lookup")
            .expect("live Spool")
            .overview
            .version;
        catalog
            .mutate(
                &uuid::Uuid::new_v4().to_string(),
                "delete",
                &uuid::Uuid::new_v4().to_string(),
                b"delete",
                |tx| {
                    repo::device_catalog::mutations::delete(tx, spool, &version)?;
                    Ok(SpoolOverview::default())
                },
            )
            .expect("remove from discovery");
        assert!(
            catalog
                .registrations()
                .expect("visible inventory")
                .is_empty()
        );
        let _restarted = Retention::start(home.path().to_owned());
        removed(&third, &artifacts).await;
    }
    async fn removed(path: &std::path::Path, artifacts: &repo::device_artifacts::ArtifactStore) {
        tokio::time::timeout(Duration::from_secs(8), async {
            while path.exists() || artifacts.next_expiry().expect("committed expiry").is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon physically expires artifact without observers");
        assert!(!path.exists());
    }
}
