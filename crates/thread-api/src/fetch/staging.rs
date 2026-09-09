//! Source bytes remain temporary until the terminal receipt and exact closure
//! have been checked. Staging never changes a repository or a checkout.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use api::v2::client::MessageReader;
use crypto::thread_operation::SignedOperation;
use heddle_object_model::object::{ContentHash, State, thread_replication::ThreadOperation};
use heddle_pack::store::pack::PackReader;
use prost::Message;
use tokio::io::AsyncWriteExt;

use super::{Download, Error, Item};
use crate::{contract::*, replication, transport};

const METADATA_BYTES: usize = 16 * 1024 * 1024;
const SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const SOURCE_OBJECTS: usize = 100_000;

/// Verified source artifacts and their original proofs. Dropping this value
/// removes its temporary files. Native callers can install it on a disk worker.
pub struct StagedSource {
    pub(super) directory: tempfile::TempDir,
    pub(super) ready: TransferReady,
    pub(super) operations: Vec<SignedOperation>,
    pub(super) state: State,
}
impl StagedSource {
    pub fn artifact_paths(&self) -> [std::path::PathBuf; 2] {
        [
            self.directory.path().join("source.pack"),
            self.directory.path().join("source.idx"),
        ]
    }
    pub fn operations(&self) -> &[SignedOperation] {
        &self.operations
    }
    pub fn ready(&self) -> &TransferReady {
        &self.ready
    }
    pub fn state(&self) -> &State {
        &self.state
    }
}
impl<R: MessageReader<Error = transport::Error>> Download<R> {
    /// Consume one complete source download into bounded temporary files and
    /// validate its original causal ancestry and exact selected source closure.
    pub async fn stage(mut self, scratch: &Path) -> Result<StagedSource, Error> {
        if self.state.facets != [SharedFacet::Source as i32] {
            return Err(Error::Invalid("staging requires the source facet alone"));
        }
        let total = self
            .state
            .ready
            .packs
            .iter()
            .try_fold(0u64, |sum, extent| sum.checked_add(extent.length))
            .ok_or(Error::Invalid("source artifact length overflow"))?;
        if total > SOURCE_BYTES {
            return Err(Error::Invalid("staged source exceeds 256 MiB"));
        }
        self.state.limits.max_operations = self.state.limits.max_operations.min(10_000);
        let directory = tempfile::Builder::new()
            .prefix("thread-download-")
            .tempdir_in(scratch)?;
        let mut files = [
            tokio::fs::File::create(directory.path().join("source.pack")).await?,
            tokio::fs::File::create(directory.path().join("source.idx")).await?,
        ];
        let mut operations = Vec::new();
        let mut metadata_bytes = 0usize;
        let mut complete = false;
        while let Some(item) = self.next().await? {
            match item {
                Item::Pack(chunk) => {
                    let kind = chunk
                        .extent
                        .as_ref()
                        .ok_or(Error::Invalid("chunk extent absent"))?
                        .kind;
                    let index = match pack_extent::Kind::try_from(kind) {
                        Ok(pack_extent::Kind::NativePack) => 0,
                        Ok(pack_extent::Kind::NativeIndex) => 1,
                        _ => return Err(Error::Invalid("native source artifacts required")),
                    };
                    files[index].write_all(&chunk.data).await?;
                }
                Item::Operation(record) => {
                    metadata_bytes = metadata_bytes
                        .checked_add(record.encoded_len())
                        .ok_or(Error::Invalid("source metadata length overflow"))?;
                    if metadata_bytes > METADATA_BYTES {
                        return Err(Error::Invalid("staged source metadata exceeds 16 MiB"));
                    }
                    operations.push(replication::decode_record(record)?);
                }
                Item::Complete(_) => complete = true,
                Item::Sidecar(_) => return Err(Error::Invalid("source staging excludes sidecars")),
            }
        }
        if !complete {
            return Err(Error::Invalid("source staging requires Complete"));
        }
        for file in &mut files {
            file.flush().await?;
            file.sync_all().await?;
        }
        drop(files);
        let ready = self.state.ready;
        tokio::task::spawn_blocking(move || validate(directory, ready, operations))
            .await
            .map_err(|error| Error::Preparation(error.to_string()))?
    }
}
fn validate(
    directory: tempfile::TempDir,
    ready: TransferReady,
    operations: Vec<SignedOperation>,
) -> Result<StagedSource, Error> {
    let thread = ready
        .thread
        .as_ref()
        .ok_or(Error::Invalid("Thread absent"))?;
    let genesis = replication::opening::verify_genesis(
        ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("original genesis absent"))?,
        thread,
    )?;
    let Some(revision_ref::Revision::State(selected)) =
        ready.current.as_ref().and_then(|r| r.revision.as_ref())
    else {
        return Err(Error::Invalid("exact native State required"));
    };
    let mut decoded = BTreeMap::<ContentHash, ThreadOperation>::new();
    let mut selected_operation = None;
    for signed in &operations {
        let operation = signed.verify().map_err(preparation)?;
        let id = operation.id().map_err(preparation)?;
        let state = operation
            .source_state()
            .map_err(preparation)?
            .ok_or(Error::Invalid("non-source operation in source ancestry"))?;
        if state.id().as_bytes().as_slice() == selected.value {
            if selected_operation.replace((id, state)).is_some() {
                return Err(Error::Invalid("ambiguous selected source proof"));
            }
        }
        if decoded.insert(id, operation).is_some() {
            return Err(Error::Invalid("duplicate source proof"));
        }
    }
    let (selected_id, state) =
        selected_operation.ok_or(Error::Invalid("selected source proof absent"))?;
    let mut pending = BTreeSet::from([selected_id]);
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop_first() {
        if !seen.insert(id) {
            continue;
        }
        let operation = decoded
            .get(&id)
            .ok_or(Error::Invalid("incomplete source ancestry"))?;
        let parents = operation
            .parents
            .iter()
            .map(|id| {
                decoded
                    .get(id)
                    .cloned()
                    .ok_or(Error::Invalid("incomplete source ancestry"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        operation
            .validate_parents(&genesis, &parents)
            .map_err(preparation)?;
        pending.extend(
            operation
                .parents
                .iter()
                .filter(|id| !seen.contains(id))
                .copied(),
        );
    }
    if seen.len() != decoded.len() {
        return Err(Error::Invalid("unselected source proofs"));
    }
    let references = decoded
        .values()
        .map(|operation| operation.reference_proof(&genesis).map_err(preparation))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    PackReader::open(
        &directory.path().join("source.pack"),
        &directory.path().join("source.idx"),
    )
    .map_err(preparation)?
    .validate_source_closure_with_references(&state, &references, SOURCE_OBJECTS, SOURCE_BYTES)
    .map_err(preparation)?;
    Ok(StagedSource {
        directory,
        ready,
        operations,
        state,
    })
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}

#[cfg(test)]
#[path = "staging_tests.rs"]
mod tests;
