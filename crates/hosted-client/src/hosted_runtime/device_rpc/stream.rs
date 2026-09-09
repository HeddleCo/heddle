//! Bounded current-state observations over shared post-commit device wakeups.
//! Payloads are committed only by checkpoints; restart/lag/window movement reset
//! explicitly, and unchanged retained streams execute no periodic storage reads.
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use prost::Message;

use super::{DeviceRpc, auth::Session, failure};

/// The view writer needs current admitted authority, not a fabricated Spool.
pub(super) trait ObservationAuthority {
    fn binding(&self) -> Vec<u8>;
    fn expires(&self) -> i64;
    fn check_clock(&self) -> Result<()>;
    fn check_current(&self, home: &std::path::Path) -> Result<()>;
}
impl ObservationAuthority for Session {
    fn binding(&self) -> Vec<u8> {
        [
            self.actor.as_bytes(),
            self.principal.as_bytes(),
            self.spool.capability_path.as_bytes(),
        ]
        .concat()
    }
    fn expires(&self) -> i64 {
        self.expires
    }
    fn check_clock(&self) -> Result<()> {
        Session::check_clock(self)
    }
    fn check_current(&self, home: &std::path::Path) -> Result<()> {
        Session::check_current(self, home)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("view changed while composing its snapshot")]
pub(super) struct SnapshotChanged;

pub(super) trait Event: Message + Default + Clone {
    fn frame(&mut self, frame: StreamFrame);
}
impl Event for ThreadEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}
impl Event for ThreadListEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}

pub(super) fn budget(requested: Option<ReadBudget>) -> ReadBudget {
    let requested = requested.unwrap_or_default();
    ReadBudget {
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
    }
}
impl DeviceRpc {
    pub(super) async fn observe_view<E: Event>(
        &self,
        session: &Session,
        method: &str,
        normalized_query: &[u8],
        options: ObserveOptions,
        send: SendStream,
        snapshot: impl Fn(&ReadBudget, &[u8]) -> Result<(Vec<(String, E)>, PageInfo, Vec<u8>)>,
        current_version: impl Fn() -> Result<Vec<u8>>,
    ) -> Result<()> {
        let feed = self.feed(session)?;
        self.observe_authorized_view(
            session,
            method,
            normalized_query,
            options,
            send,
            feed.changes.subscribe(),
            snapshot,
            current_version,
        )
        .await
    }
    pub(super) async fn observe_authorized_view<E: Event>(
        &self,
        session: &impl ObservationAuthority,
        method: &str,
        normalized_query: &[u8],
        options: ObserveOptions,
        mut send: SendStream,
        mut changes: tokio::sync::watch::Receiver<u64>,
        snapshot: impl Fn(&ReadBudget, &[u8]) -> Result<(Vec<(String, E)>, PageInfo, Vec<u8>)>,
        current_version: impl Fn() -> Result<Vec<u8>>,
    ) -> Result<()> {
        let result:Result<()> = async {
        if !matches!(
            ObservationMode::try_from(options.mode),
            Ok(ObservationMode::Unspecified | ObservationMode::Once | ObservationMode::Follow)
        ) {
            bail!("unknown observation mode");
        }
        let budget = budget(options.budget);
        let binding = blake3::hash(
            &[
                method.as_bytes(),
                session.binding().as_slice(),
                self.endpoint.as_slice(),
                normalized_query,
            ]
            .concat(),
        )
        .as_bytes()
        .to_vec();
        let mut sequence = 0u64;
        let mut open = StreamOpen {
            source: Some(self.endpoint()),
            binding_digest: binding.clone(),
            accepted_budget: Some(budget),
            ..Default::default()
        };
        if session.expires() != 0 {
            open.authority_valid_until = Some(prost_types::Timestamp {
                seconds: session.expires(),
                nanos: 0,
            });
        }
        write::<E>(
            &mut send,
            &mut sequence,
            stream_frame::Body::Open(open),
            None,
            budget.max_frame_bytes,
        )
        .await?;
        if !options.after_cursor.is_empty() {
            reset::<E>(
                &mut send,
                &mut sequence,
                StreamResetReason::SourceRestarted,
                budget.max_frame_bytes,
            )
            .await?;
            return Ok(());
        }
        let mut cursor = Vec::new();
        let mut previous = BTreeMap::<String, Vec<u8>>::new();
        let mut clock = self.authority_clock.subscribe()?;
        let mut retries = 0u8;
        loop {
            let generation = *changes.borrow_and_update();
            if generation == u64::MAX {
                bail!("device change feed lost continuity");
            }
            session.check_current(&self.home)?;
            let (events, page, revision) = match snapshot(&budget, &binding) {
                Ok(snapshot) => snapshot,
                Err(error) if error.is::<SnapshotChanged>() => {
                    retries += 1;
                    if retries >= 8 {
                        reset::<E>(
                            &mut send,
                            &mut sequence,
                            StreamResetReason::WindowChanged,
                            budget.max_frame_bytes,
                        )
                        .await?;
                        return Ok(());
                    }
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if events.len() > budget.max_items as usize {
                bail!("view exceeds accepted item budget");
            }
            if current_version()? != revision {
                retries += 1;
                if retries >= 8 {
                    reset::<E>(
                        &mut send,
                        &mut sequence,
                        StreamResetReason::WindowChanged,
                        budget.max_frame_bytes,
                    )
                    .await?;
                    return Ok(());
                }
                tokio::task::yield_now().await;
                continue;
            }
            retries = 0;
            let mut next = BTreeMap::new();
            for (id, event) in &events {
                if next.insert(id.clone(), event.encode_to_vec()).is_some() {
                    bail!("duplicate observation entity");
                }
            }
            let initial = cursor.is_empty();
            if initial || next != previous {
                if !initial && (previous.keys().any(|id| !next.contains_key(id)) || !page.exhausted)
                {
                    reset::<E>(
                        &mut send,
                        &mut sequence,
                        StreamResetReason::WindowChanged,
                        budget.max_frame_bytes,
                    )
                    .await?;
                    return Ok(());
                }
                let mut bytes = 0usize;
                for (id, event) in events {
                    if !initial && previous.get(&id) == next.get(&id) {
                        continue;
                    }
                    bytes = bytes
                        .checked_add(event.encoded_len())
                        .context("snapshot size overflow")?;
                    if bytes > budget.max_snapshot_bytes as usize {
                        bail!("view exceeds accepted snapshot budget");
                    }
                    if current_version()? != revision {
                        reset::<E>(&mut send, &mut sequence, StreamResetReason::WindowChanged, budget.max_frame_bytes).await?;
                        return Ok(());
                    }
                    session.check_current(&self.home)?;
                    write(
                        &mut send,
                        &mut sequence,
                        stream_frame::Body::Data(StreamData {
                            kind: if initial {
                                StreamDataKind::Snapshot
                            } else {
                                StreamDataKind::Upsert
                            } as i32,
                        }),
                        Some(event),
                        budget.max_frame_bytes,
                    )
                    .await?;
                }
                if current_version()? != revision {
                    reset::<E>(
                        &mut send,
                        &mut sequence,
                        StreamResetReason::WindowChanged,
                        budget.max_frame_bytes,
                    )
                    .await?;
                    return Ok(());
                }
                session.check_current(&self.home)?;
                let next_cursor = blake3::hash(
                    &[
                        binding.as_slice(),
                        &generation.to_be_bytes(),
                        &sequence.to_be_bytes(),
                    ]
                    .concat(),
                )
                .as_bytes()
                .to_vec();
                write::<E>(
                    &mut send,
                    &mut sequence,
                    stream_frame::Body::Checkpoint(StreamCheckpoint {
                        cursor: next_cursor.clone(),
                        previous_cursor: cursor,
                        snapshot_complete: initial,
                        page: Some(page),
                    }),
                    None,
                    budget.max_frame_bytes,
                )
                .await?;
                cursor = next_cursor;
                previous = next;
            }
            if options.mode == ObservationMode::Once as i32 {
                write::<E>(
                    &mut send,
                    &mut sequence,
                    stream_frame::Body::Complete(StreamComplete { cursor }),
                    None,
                    budget.max_frame_bytes,
                )
                .await?;
                send.finish()?;
                return Ok(());
            }
            loop {
                tokio::select! {
                    _ = send.stopped() => return Ok(()),
                    change = changes.changed() => {
                        change.context("device view feed closed")?;
                        // One committed Spool change can wake thousands of views.
                        // Yield before synchronous verification so ready commands
                        // are not queued behind every observer on this worker.
                        tokio::task::yield_now().await;
                        session.check_current(&self.home)?;
                        let can_skip = method == "/heddle.api.v2alpha1.ThreadService/ObserveThread"
                            && api::heddle::api::v2alpha1::ObserveThreadRequest::decode(normalized_query)?
                                .sections.iter().all(|section| *section == api::heddle::api::v2alpha1::ThreadSection::Overview as i32);
                        if !can_skip || current_version()? != revision { break; }
                        // A sibling Thread's mutation does not rebuild this view.
                    },
                    ended = clock.expired(|| session.check_clock()) => if let Err(error) = ended {
                        send.write_all(&api::framing::encode_stream_failure(&failure(CallFailureCode::Unauthenticated, error))?).await?;
                        send.finish()?; return Ok(());
                    },
                }
            }
        }
        }.await;
        if let Err(error) = result {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                send.write_all(&api::framing::encode_stream_failure(&failure(
                    CallFailureCode::FailedPrecondition,
                    error,
                ))?),
            )
            .await??;
            send.finish()?;
        }
        Ok(())
    }
}
async fn write<E: Event>(
    send: &mut SendStream,
    sequence: &mut u64,
    body: stream_frame::Body,
    event: Option<E>,
    limit: u32,
) -> Result<()> {
    *sequence = sequence
        .checked_add(1)
        .context("stream sequence exhausted")?;
    let mut event = event.unwrap_or_default();
    event.frame(StreamFrame {
        sequence: *sequence,
        body: Some(body),
    });
    if event.encoded_len() > limit as usize {
        bail!("view frame exceeds accepted byte budget");
    }
    let bytes = api::framing::encode_stream_message(&event.encode_to_vec())?;
    tokio::time::timeout(std::time::Duration::from_secs(30), send.write_all(&bytes)).await??;
    Ok(())
}
async fn reset<E: Event>(
    send: &mut SendStream,
    sequence: &mut u64,
    reason: StreamResetReason,
    limit: u32,
) -> Result<()> {
    write::<E>(
        send,
        sequence,
        stream_frame::Body::Reset(StreamReset {
            reason: reason as i32,
        }),
        None,
        limit,
    )
    .await?;
    send.finish()?;
    Ok(())
}

impl Event for WorkspaceEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}

impl Event for SpoolEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}

impl Event for IdentityEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}

impl Event for OwnershipEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}
