use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use anyhow::{Context, Result, bail};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use prost::Message;

use super::{DeviceRpc, auth::Session, checkout::same_spool, failure};

#[derive(Debug)]
pub(super) struct Feed {
    pub(super) changes: tokio::sync::watch::Sender<u64>,
    _watchers: std::sync::Mutex<Vec<repo::device_watch::DeviceWatch>>,
    checkout_watches: std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>>,
    runs: repo::device_runs::RunStore,
    artifacts: repo::device_artifacts::ArtifactStore,
    _replica: std::sync::Mutex<Option<repo::device_watch::DeviceDatabaseWatch>>,
}
#[derive(Clone)]
enum Payload {
    Checkout(CheckoutOverview),
    Run(RunRecord),
    Policy(RunPolicy),
    Timeline(TimelineRecord),
}
impl Payload {
    fn id(&self) -> String {
        match self {
            Self::Checkout(v) => format!(
                "checkout:{}",
                v.r#ref.as_ref().map(|r| r.id.as_str()).unwrap_or_default()
            ),
            Self::Run(v) => format!(
                "run:{}",
                v.r#ref.as_ref().map(|r| r.id.as_str()).unwrap_or_default()
            ),
            Self::Policy(_) => "policy".into(),
            Self::Timeline(v) => format!(
                "timeline:{}",
                v.r#ref.as_ref().map(|r| r.id.as_str()).unwrap_or_default()
            ),
        }
    }
}
impl DeviceRpc {
    pub(super) fn feed(&self, session: &Session) -> Result<Arc<Feed>> {
        let mut feeds = self
            .feeds
            .lock()
            .map_err(|_| anyhow::anyhow!("device feed lock poisoned"))?;
        if let Some(feed) = feeds.get(&session.spool.id).and_then(Weak::upgrade) {
            return Ok(feed);
        }
        let runs = repo::device_runs::RunStore::open(&session.spool.heddle_dir)?;
        let artifacts = repo::device_artifacts::ArtifactStore::open(&session.spool.heddle_dir)?;
        let replica = if session
            .spool
            .heddle_dir
            .join(repo::local_metadata::DATABASE_NAME)
            .exists()
        {
            Some(repo::device_watch::hold_replica_database(
                &session.spool.heddle_dir,
            )?)
        } else {
            None
        };
        let (changes, _) = tokio::sync::watch::channel(0u64);
        let metadata = session.spool.heddle_dir.clone();
        let data_sender = changes.clone();
        let data = repo::device_watch::watch_filtered(
            &session.spool.root,
            move |path| {
                if let Ok(relative) = path.strip_prefix(&metadata) {
                    let first = relative
                        .components()
                        .next()
                        .map(|part| part.as_os_str().to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if first == "device-checkouts" {
                        let parts = relative
                            .components()
                            .map(|part| part.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>();
                        if let Some(index) = parts.iter().position(|part| part == ".heddle") {
                            return parts.get(index + 1).is_some_and(|part| {
                                matches!(part.as_str(), "HEAD" | "refs" | "thread-checkout.json")
                            });
                        }
                        return true;
                    }
                    matches!(
                        first.as_str(),
                        "HEAD"
                            | "refs"
                            | "config.toml"
                            | repo::local_metadata::DATABASE_NAME
                            | repo::local_metadata::CHANGE_MARKER_NAME
                            | "native-checkouts"
                            | "device-checkouts"
                            | "writer-leases"
                    )
                } else {
                    let parts = path
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>();
                    if let Some(index) = parts.iter().rposition(|part| part == ".heddle") {
                        return parts.get(index + 1).is_some_and(|part| {
                            matches!(
                                part.as_str(),
                                "HEAD" | "refs" | "thread-checkout.json" | "config.toml"
                            )
                        });
                    }
                    true
                }
            },
            move |result| {
                data_sender.send_modify(|version| {
                    *version = if result.is_err() {
                        u64::MAX
                    } else {
                        version.saturating_add(1)
                    }
                });
            },
        )?;
        let authority_sender = changes.clone();
        let authority = repo::device_watch::watch_filtered(
            &self.home.join("state/device-rpc"),
            |path| {
                path.file_name().is_some_and(|name| {
                    name == "authority.bin" || name == "catalog.sqlite3.changed"
                })
            },
            move |result| {
                authority_sender.send_modify(|version| {
                    *version = if result.is_err() {
                        u64::MAX
                    } else {
                        version.saturating_add(1)
                    }
                });
            },
        )?;
        let feed = Arc::new(Feed {
            changes,
            _watchers: std::sync::Mutex::new(vec![data, authority]),
            checkout_watches: std::sync::Mutex::new(Default::default()),
            runs,
            artifacts,
            _replica: std::sync::Mutex::new(replica),
        });
        feeds.insert(session.spool.id, Arc::downgrade(&feed));
        Ok(feed)
    }
    pub(super) async fn observe(
        &self,
        session: &Session,
        method: &str,
        body: &[u8],
        mut send: SendStream,
    ) -> Result<()> {
        let is_checkout = method.ends_with("/ObserveCheckouts");
        let (options, page) = if is_checkout {
            let request = ObserveCheckoutsRequest::decode(body)?;
            (
                request.observe.unwrap_or_default(),
                request.page.unwrap_or_default(),
            )
        } else {
            let request = ObserveRunsRequest::decode(body)?;
            (
                request.observe.unwrap_or_default(),
                request.page.unwrap_or_default(),
            )
        };
        let requested = options.budget.unwrap_or_default();
        let budget = ReadBudget {
            max_items: if requested.max_items == 0 {
                100
            } else {
                requested.max_items.min(1024)
            },
            max_frame_bytes: if requested.max_frame_bytes == 0 {
                256 * 1024
            } else {
                requested.max_frame_bytes.min(256 * 1024)
            },
            max_snapshot_bytes: if requested.max_snapshot_bytes == 0 {
                1024 * 1024
            } else {
                requested.max_snapshot_bytes.min(4 * 1024 * 1024)
            },
        };
        let page_size = if page.size == 0 {
            budget.max_items
        } else {
            page.size.min(budget.max_items)
        } as usize;
        let mut binding_bytes = [
            method.as_bytes(),
            session.actor.as_bytes(),
            session.principal.as_bytes(),
            session.spool.capability_path.as_bytes(),
            self.endpoint.as_slice(),
        ]
        .concat();
        // Resume is always explicit reset until durable replay is implemented;
        // a snapshot cursor is never mistaken for collection pagination.
        let normalized;
        if is_checkout {
            let mut request = ObserveCheckoutsRequest::decode(body)?;
            request.observe = None;
            if let Some(page) = request.page.as_mut() {
                page.after_page.clear();
            }
            normalized = request.encode_to_vec();
        } else {
            let mut request = ObserveRunsRequest::decode(body)?;
            request.observe = None;
            if let Some(page) = request.page.as_mut() {
                page.after_page.clear();
            }
            normalized = request.encode_to_vec();
        }
        binding_bytes.extend_from_slice(&normalized);
        let binding = blake3::hash(&binding_bytes).as_bytes().to_vec();
        let feed = self.feed(session)?;
        let mut changes = feed.changes.subscribe();
        let mut sequence = 0;
        write(
            &mut send,
            is_checkout,
            &mut sequence,
            stream_frame::Body::Open(StreamOpen {
                source: Some(self.endpoint()),
                binding_digest: binding.clone(),
                accepted_budget: Some(budget),
                authority_valid_until: if session.expires == 0 {
                    None
                } else {
                    Some(prost_types::Timestamp {
                        seconds: session.expires,
                        nanos: 0,
                    })
                },
                ..Default::default()
            }),
            None,
        )
        .await?;
        if !options.after_cursor.is_empty() {
            write(
                &mut send,
                is_checkout,
                &mut sequence,
                stream_frame::Body::Reset(StreamReset {
                    reason: StreamResetReason::SourceRestarted as i32,
                }),
                None,
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
        let mut previous = Vec::new();
        let mut observed = BTreeMap::<String, Vec<u8>>::new();
        let mut clock = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut projection_retries = 0u8;
        loop {
            let generation = *changes.borrow_and_update();
            if generation == u64::MAX {
                bail!("device change feed lost continuity");
            }
            session.check_current(&self.home)?;
            let (payloads, page_info) = self.snapshot(
                session,
                &feed,
                is_checkout,
                body,
                &page,
                page_size,
                &binding,
            )?;
            if *changes.borrow() != generation {
                projection_retries += 1;
                if projection_retries >= 8 {
                    write(
                        &mut send,
                        is_checkout,
                        &mut sequence,
                        stream_frame::Body::Reset(StreamReset {
                            reason: StreamResetReason::WindowChanged as i32,
                        }),
                        None,
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                tokio::task::yield_now().await;
                continue;
            }
            projection_retries = 0;
            let permission_deadline = payloads
                .iter()
                .filter_map(|payload| {
                    if let Payload::Run(run) = payload {
                        run.pending_permissions
                            .iter()
                            .filter_map(|permission| {
                                permission
                                    .expires_at
                                    .as_ref()
                                    .map(|deadline| deadline.seconds)
                            })
                            .chain(run.artifacts.iter().filter_map(|artifact| {
                                artifact
                                    .retained_until
                                    .as_ref()
                                    .map(|expiry| expiry.seconds)
                            }))
                            .min()
                    } else {
                        None
                    }
                })
                .min();
            let snapshot = previous.is_empty();
            let next: BTreeMap<_, _> = payloads
                .iter()
                .map(|payload| Ok((payload.id(), payload_bytes(payload))))
                .collect::<Result<_>>()?;
            if !snapshot && next == observed { /* no visible change */
            } else {
                if !snapshot
                    && (observed.keys().any(|id| !next.contains_key(id)) || !page_info.exhausted)
                {
                    write(
                        &mut send,
                        is_checkout,
                        &mut sequence,
                        stream_frame::Body::Reset(StreamReset {
                            reason: StreamResetReason::WindowChanged as i32,
                        }),
                        None,
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                let mut total = 0usize;
                for payload in payloads {
                    let encoded = payload_bytes(&payload);
                    if !snapshot && observed.get(&payload.id()) == Some(&encoded) {
                        continue;
                    }
                    total = total
                        .checked_add(encoded.len())
                        .context("snapshot size overflow")?;
                    if encoded.len() + 256 > budget.max_frame_bytes as usize
                        || total > budget.max_snapshot_bytes as usize
                    {
                        bail!("device observation exceeds accepted byte budget; reduce page size");
                    }
                    session.check_current(&self.home)?;
                    if let Payload::Run(run) = &payload {
                        let reference = run.r#ref.as_ref().context("Run reference absent")?;
                        if feed
                            .artifacts
                            .for_run(reference, chrono::Utc::now().timestamp())?
                            != run.artifacts
                        {
                            bail!("artifact disclosure changed before Run output");
                        }
                    }
                    write(
                        &mut send,
                        is_checkout,
                        &mut sequence,
                        stream_frame::Body::Data(StreamData {
                            kind: if snapshot {
                                StreamDataKind::Snapshot as i32
                            } else {
                                StreamDataKind::Upsert as i32
                            },
                        }),
                        Some(payload),
                    )
                    .await?;
                }
                if *changes.borrow() != generation {
                    write(
                        &mut send,
                        is_checkout,
                        &mut sequence,
                        stream_frame::Body::Reset(StreamReset {
                            reason: StreamResetReason::WindowChanged as i32,
                        }),
                        None,
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                session.check_current(&self.home)?;
                let cursor = blake3::hash(
                    &[
                        binding.as_slice(),
                        &generation.to_be_bytes(),
                        &sequence.to_be_bytes(),
                    ]
                    .concat(),
                )
                .as_bytes()
                .to_vec();
                write(
                    &mut send,
                    is_checkout,
                    &mut sequence,
                    stream_frame::Body::Checkpoint(StreamCheckpoint {
                        cursor: cursor.clone(),
                        previous_cursor: previous,
                        snapshot_complete: snapshot,
                        page: Some(page_info),
                    }),
                    None,
                )
                .await?;
                previous = cursor;
                observed = next;
            }
            if options.mode == ObservationMode::Once as i32 {
                write(
                    &mut send,
                    is_checkout,
                    &mut sequence,
                    stream_frame::Body::Complete(StreamComplete { cursor: previous }),
                    None,
                )
                .await?;
                send.finish()?;
                return Ok(());
            }
            loop {
                tokio::select! {
                    _=send.stopped()=>return Ok(()),
                    changed=changes.changed()=>{changed.context("device feed closed")?;break;},
                    _=clock.tick()=>{
                        if let Err(error)=session.check_clock(){send.write_all(&api::framing::encode_stream_failure(&failure(CallFailureCode::Unauthenticated,error))?).await?;send.finish()?;return Ok(());}
                        if permission_deadline.is_some_and(|deadline|chrono::Utc::now().timestamp()>=deadline){break;}
                    },
                }
            }
        }
    }
    fn snapshot(
        &self,
        session: &Session,
        feed: &Feed,
        checkouts: bool,
        body: &[u8],
        page: &PageRequest,
        size: usize,
        binding: &[u8],
    ) -> Result<(Vec<Payload>, PageInfo)> {
        {
            let mut anchor = feed
                ._replica
                .lock()
                .map_err(|_| anyhow::anyhow!("replica anchor lock poisoned"))?;
            if anchor.is_none()
                && session
                    .spool
                    .heddle_dir
                    .join(repo::local_metadata::DATABASE_NAME)
                    .exists()
            {
                *anchor = Some(repo::device_watch::hold_replica_database(
                    &session.spool.heddle_dir,
                )?);
            }
        }
        let after = if page.after_page.is_empty() {
            String::new()
        } else {
            if page.after_page.len() < 32 || &page.after_page[..32] != binding {
                bail!("page token belongs to another observation");
            }
            std::str::from_utf8(&page.after_page[32..])?.to_owned()
        };
        let mut payloads = Vec::new();
        let mut last = if after.is_empty() {
            "!".to_owned()
        } else {
            after.clone()
        };
        let exhausted;
        if checkouts {
            let request = ObserveCheckoutsRequest::decode(body)?;
            for reference in &request.checkouts {
                same_spool(session, reference.spool.as_ref())?;
                if reference.device.as_ref() != Some(&self.endpoint()) {
                    bail!("checkout filter targets another device");
                }
            }
            for reference in &request.threads {
                same_spool(session, reference.spool.as_ref())?;
            }
            let directory = session.spool.heddle_dir.join("native-checkouts");
            let mut ids = if directory.exists() {
                std::fs::read_dir(directory)?
                    .map(|entry| {
                        Ok(entry?
                            .file_name()
                            .to_string_lossy()
                            .trim_end_matches(".json")
                            .to_owned())
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            ids.sort();
            for id in ids.into_iter().filter(|id| id > &after) {
                if !request.checkouts.is_empty() && !request.checkouts.iter().any(|r| r.id == id) {
                    continue;
                }
                let checkout = repo::device_catalog::checkout(&session.spool, &id)?;
                if !checkout.repository.root().starts_with(&session.spool.root) {
                    let mut paths = feed
                        .checkout_watches
                        .lock()
                        .map_err(|_| anyhow::anyhow!("checkout watcher lock poisoned"))?;
                    if !paths.contains(checkout.repository.root()) {
                        feed._watchers
                            .lock()
                            .map_err(|_| anyhow::anyhow!("watcher lock poisoned"))?
                            .first_mut()
                            .context("data watcher missing")?
                            .add(checkout.repository.root())?;
                        paths.insert(checkout.repository.root().to_owned());
                    }
                }
                if !request.threads.is_empty()
                    && !request.threads.iter().any(|r| {
                        r.id.as_ref()
                            .is_some_and(|r| r.value == checkout.binding.thread.as_bytes())
                    })
                {
                    continue;
                }
                payloads.push(Payload::Checkout(
                    self.checkout_overview(session, &checkout)?,
                ));
                last = id;
                if payloads.len() > size {
                    break;
                }
            }
            exhausted = payloads.len() <= size;
            if !exhausted {
                payloads.pop();
                last = payloads
                    .last()
                    .and_then(|p| {
                        if let Payload::Checkout(c) = p {
                            c.r#ref.as_ref().map(|r| r.id.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
            }
        } else {
            let request = ObserveRunsRequest::decode(body)?;
            for reference in &request.runs {
                same_spool(session, reference.spool.as_ref())?;
            }
            let threads = request
                .threads
                .iter()
                .map(|reference| {
                    same_spool(session, reference.spool.as_ref())?;
                    let id = reference.id.as_ref().context("thread id required")?;
                    if id.value.len() != 32 {
                        bail!("invalid Thread id");
                    }
                    Ok(id.value.clone())
                })
                .collect::<Result<Vec<_>>>()?;
            let runs = request
                .runs
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            let policy = if after.is_empty() {
                feed.runs.policy(&session.spool.id.to_string())?
            } else {
                None
            };
            let remaining = size.saturating_sub(usize::from(policy.is_some()));
            let candidates = feed.runs.observation_page(
                &after,
                remaining + 1,
                &runs,
                &threads,
                request.include_timeline,
            )?;
            exhausted = candidates.len() <= remaining;
            for (key, value) in candidates.into_iter().take(remaining) {
                last = key;
                payloads.push(match value {
                    repo::device_runs::RunObservation::Run(mut run) => {
                        run.artifacts = feed.artifacts.for_run(
                            run.r#ref.as_ref().context("Run reference absent")?,
                            chrono::Utc::now().timestamp(),
                        )?;
                        let method = "/heddle.api.v2alpha1.RunService/ControlRun";
                        run.actions = if run.supported_controls.is_empty() {
                            Vec::new()
                        } else {
                            vec![ActionAvailability {
                                method: method.into(),
                                endpoint: Some(self.endpoint()),
                                implemented: true,
                                authorized: session.permits(method),
                                ..Default::default()
                            }]
                        };
                        Payload::Run(run)
                    }
                    repo::device_runs::RunObservation::Timeline(timeline) => {
                        Payload::Timeline(timeline)
                    }
                });
            }
            if let Some(policy) = policy {
                payloads.push(Payload::Policy(policy));
            }
        }
        let next_page = if exhausted {
            Vec::new()
        } else {
            [binding, last.as_bytes()].concat()
        };
        Ok((
            payloads,
            PageInfo {
                next_page,
                exhausted,
                matching_count: None,
            },
        ))
    }
}
fn payload_bytes(payload: &Payload) -> Vec<u8> {
    match payload {
        Payload::Checkout(v) => v.encode_to_vec(),
        Payload::Run(v) => v.encode_to_vec(),
        Payload::Policy(v) => v.encode_to_vec(),
        Payload::Timeline(v) => v.encode_to_vec(),
    }
}
async fn write(
    send: &mut SendStream,
    checkouts: bool,
    sequence: &mut u64,
    body: stream_frame::Body,
    payload: Option<Payload>,
) -> Result<()> {
    *sequence += 1;
    let frame = Some(StreamFrame {
        sequence: *sequence,
        body: Some(body),
    });
    let bytes = if checkouts {
        CheckoutEvent {
            frame,
            payload: match payload {
                Some(Payload::Checkout(value)) => Some(checkout_event::Payload::Checkout(value)),
                None => None,
                _ => bail!("wrong checkout payload"),
            },
        }
        .encode_to_vec()
    } else {
        RunEvent {
            frame,
            payload: match payload {
                Some(Payload::Run(value)) => Some(run_event::Payload::Run(value)),
                Some(Payload::Policy(value)) => Some(run_event::Payload::Policy(value)),
                Some(Payload::Timeline(value)) => Some(run_event::Payload::Timeline(value)),
                None => None,
                _ => bail!("wrong run payload"),
            },
        }
        .encode_to_vec()
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        send.write_all(&api::framing::encode_stream_message(&bytes)?),
    )
    .await??;
    Ok(())
}
