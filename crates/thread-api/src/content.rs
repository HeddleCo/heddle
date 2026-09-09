// SPDX-License-Identifier: Apache-2.0
//! Bounded full-blob reads at one exact revision. Large/ranged transfers use the
//! same generated ContentServiceReadContent stream and a caller-owned sink.
use api::v2::client::RpcTransport;

pub use crate::contract::blob_read::Source as BlobSource;
use crate::{
    Remote,
    contract::*,
    observation::{self, Error},
    rpc, transport,
};

pub struct Blob {
    pub source: BlobSource,
    pub object_hash: Vec<u8>,
    pub bytes: Vec<u8>,
}

impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    /// One request for any mixture of paths and missing object hashes. The
    /// revision pins both content and authorization; no mutable Thread-tip lookup.
    pub async fn read_blobs(
        &self,
        revision: RevisionRef,
        sources: Vec<BlobSource>,
    ) -> Result<Vec<Blob>, Error> {
        let budget = observation::budget(&self.description)?;
        if sources.is_empty() || sources.len() > budget.max_items as usize {
            return Err(Error::Invalid("invalid selection count"));
        }
        for source in &sources {
            match source {
                BlobSource::Path(path) if !path.is_empty() => {}
                BlobSource::ObjectHash(hash) if hash.len() == 32 => {}
                _ => return Err(Error::Invalid("invalid blob source")),
            }
        }
        let selections = sources
            .iter()
            .enumerate()
            .map(|(i, source)| ContentRead {
                selection_id: i.to_string(),
                selection: Some(content_read::Selection::Blob(BlobRead {
                    source: Some(source.clone()),
                    offset: 0,
                    length: 0,
                })),
            })
            .collect();
        let mut messages = self
            .api
            .observe::<rpc::ContentServiceReadContent>(&ReadContentRequest {
                revision: Some(revision.clone()),
                selections,
                budget: Some(budget),
            })
            .await?;
        let mut blobs: Vec<_> = sources
            .into_iter()
            .map(|source| Blob {
                source,
                object_hash: vec![],
                bytes: vec![],
            })
            .collect();
        let mut range_done = vec![false; blobs.len()];
        let mut complete = vec![false; blobs.len()];
        let mut totals = vec![None; blobs.len()];
        let mut received_bytes = 0_u64;
        let mut received_items = 0_u32;
        while let Some(event) = messages.next().await? {
            let size = prost::Message::encoded_len(&event) as u64;
            if size > u64::from(budget.max_frame_bytes)
                || size > budget.max_snapshot_bytes.saturating_sub(received_bytes)
                || received_items >= budget.max_items
            {
                return Err(Error::Invalid("content budget exceeded"));
            }
            received_bytes += size;
            received_items += 1;
            if event.revision.as_ref() != Some(&revision) {
                return Err(Error::Invalid("content revision mismatch"));
            }
            let index = event
                .selection_id
                .parse::<usize>()
                .map_err(|_| Error::Invalid("unknown content selection"))?;
            if event.selection_id != index.to_string() || index >= blobs.len() || complete[index] {
                return Err(Error::Invalid("unknown or completed content selection"));
            }
            let blob = &mut blobs[index];
            match event
                .payload
                .ok_or(Error::Invalid("missing content payload"))?
            {
                content_event::Payload::Blob(chunk) => {
                    if range_done[index]
                        || chunk.offset != blob.bytes.len() as u64
                        || chunk.total_size > budget.max_snapshot_bytes
                        || chunk.data.len() as u64 > chunk.total_size.saturating_sub(chunk.offset)
                        || chunk.object_hash.len() != 32
                        || totals[index].is_some_and(|total| total != chunk.total_size)
                        || (!blob.object_hash.is_empty() && blob.object_hash != chunk.object_hash)
                        || matches!(&blob.source, BlobSource::ObjectHash(hash) if *hash != chunk.object_hash)
                    {
                        return Err(Error::Invalid("inconsistent blob range or identity"));
                    }
                    totals[index] = Some(chunk.total_size);
                    blob.object_hash = chunk.object_hash;
                    blob.bytes.extend(chunk.data);
                    if chunk.range_complete && blob.bytes.len() as u64 != chunk.total_size {
                        return Err(Error::Invalid("truncated complete blob"));
                    }
                    range_done[index] = chunk.range_complete;
                }
                content_event::Payload::SelectionComplete(status) => {
                    if !range_done[index]
                        || status.coverage != Coverage::Complete as i32
                        || status.computed_for.as_ref().is_some_and(|r| r != &revision)
                    {
                        return Err(Error::Invalid("incomplete blob selection"));
                    }
                    complete[index] = true;
                }
                _ => return Err(Error::Invalid("unexpected content payload")),
            }
            if complete.iter().all(|done| *done) {
                messages.cancel();
                return Ok(blobs);
            }
        }
        Err(Error::Interrupted)
    }
}

/// Decode native conflict attachment bytes without inventing lifecycle evidence.
/// Region geometry is immutable; absent retained resolution evidence stays unspecified.

pub fn structured_conflicts(
    bytes: &[u8],
) -> Result<api::heddle::api::v1alpha1::StructuredConflicts, transport::Error> {
    use api::heddle::api::v1alpha1 as shared;
    let native = heddle_object_model::object::StructuredConflict::decode(bytes)
        .map_err(|error| transport::Error::Io(error.to_string()))?;
    let range = |range: heddle_object_model::object::ConflictRange| shared::ConflictRange {
        start_line: range.start_line,
        end_line: range.end_line,
    };
    let side = |side: heddle_object_model::object::ConflictSide| shared::ConflictSide {
        source_state: Some(shared::StateId {
            value: side.source_state.as_bytes().to_vec(),
        }),
        blob_id: side.blob_id.map(|hash| hash.as_bytes().to_vec()),
        range: Some(range(side.range)),
        hunk_hash: side.hunk_hash.as_bytes().to_vec(),
    };
    Ok(shared::StructuredConflicts {
        conflicts: native
            .conflicts
            .into_iter()
            .map(|record| shared::StructuredConflict {
                id: record.id,
                path: record.path,
                symbol: record.symbol,
                occurrence: record.occurrence,
                merged_range: Some(range(record.merged_range)),
                base: Some(side(record.base)),
                ours: Some(side(record.ours)),
                theirs: Some(side(record.theirs)),
                ..Default::default()
            })
            .collect(),
    })
}
