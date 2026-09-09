//! Finite reads of explicitly retained private Run artifacts.
use std::{
    io::{Read, Seek, SeekFrom},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use prost::Message;

use super::{DeviceRpc, auth::Session, checkout, failure};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ITEMS: u32 = 4096;
const MAX_FRAME: u32 = 256 * 1024;

impl DeviceRpc {
    pub(super) async fn read_artifact(
        &self,
        session: Session,
        body: &[u8],
        mut send: SendStream,
    ) -> Result<()> {
        let session = Arc::new(session);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            self.artifact_response(&session, body, &mut send),
        )
        .await;
        let result = result
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
        if let Err(error) = result {
            let bytes = api::framing::encode_stream_failure(&failure(
                CallFailureCode::FailedPrecondition,
                error,
            ))?;
            tokio::time::timeout(Duration::from_secs(5), send.write_all(&bytes)).await??;
        }
        send.finish()?;
        Ok(())
    }

    async fn artifact_response(
        &self,
        session: &Arc<Session>,
        body: &[u8],
        send: &mut SendStream,
    ) -> Result<()> {
        let request = ReadArtifactRequest::decode(body)?;
        let reference = request.artifact.context("artifact required")?;
        checkout::same_spool(session, reference.spool.as_ref())?;
        let requested = request.budget.unwrap_or_default();
        let limits = ReadBudget {
            max_items: if requested.max_items == 0 {
                1024
            } else {
                requested.max_items
            },
            max_frame_bytes: if requested.max_frame_bytes == 0 {
                65536
            } else {
                requested.max_frame_bytes
            },
            max_snapshot_bytes: if requested.max_snapshot_bytes == 0 {
                MAX_BYTES
            } else {
                requested.max_snapshot_bytes
            },
        };
        if limits.max_items > MAX_ITEMS
            || !(1024..=MAX_FRAME).contains(&limits.max_frame_bytes)
            || limits.max_snapshot_bytes > MAX_BYTES
        {
            bail!("artifact budget exceeds endpoint limits");
        }
        // A slow consumer cannot create unbounded queued blocking workers.
        let slot = self
            .content_work
            .clone()
            .try_acquire_owned()
            .context("device content readers at capacity")?;
        let home = self.home.clone();
        let current = session.clone();
        let selected = reference.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ArtifactEvent>(1);
        let worker = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            current.check_current(&home)?;
            let store = repo::device_artifacts::ArtifactStore::open(&current.spool.heddle_dir)?;
            let mut read = store.read(&selected, chrono::Utc::now().timestamp())?;
            let size = read.record.size;
            if request.offset > size {
                bail!("artifact offset exceeds size");
            }
            let end = if request.length == 0 {
                size
            } else {
                size.min(request.offset.saturating_add(request.length))
            };
            if end - request.offset > limits.max_snapshot_bytes {
                bail!("artifact range exceeds read budget");
            }
            let mut offset = request.offset;
            read.file.seek(SeekFrom::Start(offset))?;
            let mut buffer = vec![0; limits.max_frame_bytes as usize / 2];
            let mut count = 0;
            let mut bytes = 0;
            let mut emit = |payload| -> Result<()> {
                current.check_current(&home)?;
                if store.current(&selected, chrono::Utc::now().timestamp())? != read.record {
                    bail!("artifact disclosure changed");
                }
                let event = ArtifactEvent {
                    artifact: Some(selected.clone()),
                    payload: Some(payload),
                };
                let length = event.encoded_len() as u64;
                if count >= limits.max_items
                    || length > u64::from(limits.max_frame_bytes)
                    || length > limits.max_snapshot_bytes.saturating_sub(bytes)
                {
                    bail!("artifact read budget exhausted");
                }
                count += 1;
                bytes += length;
                sender
                    .blocking_send(event)
                    .map_err(|_| anyhow::anyhow!("artifact read cancelled"))
            };
            loop {
                let length = (end - offset).min(buffer.len() as u64) as usize;
                read.file.read_exact(&mut buffer[..length])?;
                emit(artifact_event::Payload::Chunk(BlobChunk {
                    offset,
                    data: buffer[..length].to_vec(),
                    total_size: size,
                    object_hash: read.record.content_hash.clone(),
                    range_complete: offset + length as u64 == end,
                }))?;
                offset += length as u64;
                if offset == end {
                    break;
                }
            }
            emit(artifact_event::Payload::Complete(SectionStatus {
                section: "artifact".into(),
                coverage: Coverage::Complete as i32,
                ..Default::default()
            }))?;
            Ok::<(), anyhow::Error>(())
        });
        while let Some(event) = receiver.recv().await {
            // Recheck after queueing/backpressure, immediately before disclosure.
            let current = session.clone();
            let home = self.home.clone();
            let selected = reference.clone();
            tokio::task::spawn_blocking(move || {
                current.check_current(&home)?;
                let store = repo::device_artifacts::ArtifactStore::open(&current.spool.heddle_dir)?;
                store.current(&selected, chrono::Utc::now().timestamp())?;
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            send.write_all(&api::framing::encode_stream_message(
                &event.encode_to_vec(),
            )?)
            .await?;
        }
        worker.await??;
        Ok(())
    }
}
