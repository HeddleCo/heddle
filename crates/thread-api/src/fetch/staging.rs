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
    pub(super) dependencies: Vec<ThreadGenesisRecord>,
    pub(super) state: State,
    pub(super) authority_admissions: BTreeMap<ContentHash, crypto::thread_authority_admission::SignedAuthorityAdmission>,
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
    pub fn dependency_geneses(&self) -> &[ThreadGenesisRecord] {
        &self.dependencies
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
        let mut receipt_records = Vec::new();
        let mut dependencies = Vec::new();
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
                Item::Operations(batch) => {
                    metadata_bytes = metadata_bytes.checked_add(batch.encoded_len()).ok_or(Error::Invalid("source metadata length overflow"))?;
                    if metadata_bytes > METADATA_BYTES { return Err(Error::Invalid("staged source metadata exceeds 16 MiB")); }
                    for record in batch.operations { operations.push(replication::decode_record(record)?); }
                    receipt_records.extend(batch.authority_admissions);
                }
                Item::ThreadGenesis(record) => {
                    metadata_bytes = metadata_bytes
                        .checked_add(record.encoded_len())
                        .ok_or(Error::Invalid("source metadata length overflow"))?;
                    if metadata_bytes > METADATA_BYTES || dependencies.len() >= 127 {
                        return Err(Error::Invalid("dependency metadata exceeds bounds"));
                    }
                    dependencies.push(record);
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
        tokio::task::spawn_blocking(move || validate_with_receipts(directory, ready, operations, dependencies, receipt_records))
            .await
            .map_err(|error| Error::Preparation(error.to_string()))?
    }
}
#[cfg(test)]
fn validate(
    directory: tempfile::TempDir,
    ready: TransferReady,
    operations: Vec<SignedOperation>,
    dependencies: Vec<ThreadGenesisRecord>,
) -> Result<StagedSource, Error> {
    validate_with_receipts(directory, ready, operations, dependencies, Vec::new())
}
fn validate_with_receipts(
    directory: tempfile::TempDir, ready: TransferReady, operations: Vec<SignedOperation>,
    dependencies: Vec<ThreadGenesisRecord>, receipt_records: Vec<SignedRecord>,
) -> Result<StagedSource, Error> {
    let value = validate_artifacts(directory,
        ready.thread.as_ref().ok_or(Error::Invalid("Thread absent"))?,
        ready.current.as_ref().ok_or(Error::Invalid("revision absent"))?,
        ready.thread_genesis.as_ref().ok_or(Error::Invalid("original genesis absent"))?,
        operations, dependencies, receipt_records)?;
    Ok(StagedSource { directory: value.directory, ready, operations: value.operations,
        dependencies: value.dependencies, state: value.state, authority_admissions: value.authority_admissions })
}
/// Structurally verified original source and actual artifact closure. This is
/// not an author, audience, executor, or sharing-policy admission decision.
pub struct ValidatedSourceArtifacts {
    directory: tempfile::TempDir,
    operations: Vec<SignedOperation>,
    genesis: ThreadGenesisRecord,
    dependencies: Vec<ThreadGenesisRecord>,
    state: State,
    authority_admissions: BTreeMap<ContentHash, crypto::thread_authority_admission::SignedAuthorityAdmission>,
}
impl ValidatedSourceArtifacts {
    pub fn artifact_paths(&self) -> [std::path::PathBuf; 2] {
        [self.directory.path().join("source.pack"), self.directory.path().join("source.idx")]
    }
    pub fn operations(&self) -> &[SignedOperation] { &self.operations }
    pub fn geneses(&self) -> impl Iterator<Item = &ThreadGenesisRecord> {
        std::iter::once(&self.genesis).chain(&self.dependencies)
    }
    pub fn state(&self) -> &State { &self.state }
    pub fn authority_admissions(&self) -> &BTreeMap<ContentHash, crypto::thread_authority_admission::SignedAuthorityAdmission> { &self.authority_admissions }
}
pub(crate) fn validate_artifacts(
    directory: tempfile::TempDir,
    thread: &ThreadRef,
    revision: &RevisionRef,
    original: &ThreadGenesisRecord,
    operations: Vec<SignedOperation>,
    dependency_records: Vec<ThreadGenesisRecord>,
    receipt_records: Vec<SignedRecord>,
) -> Result<ValidatedSourceArtifacts, Error> {
    if operations.is_empty() || operations.len() > 10_000 || dependency_records.len() >= 128 || receipt_records.len() > operations.len() {
        return Err(Error::Invalid("source original graph exceeds bounds"));
    }
    let mut metadata = original.encoded_len();
    for record in &dependency_records { metadata = metadata.saturating_add(record.encoded_len()); }
    for operation in &operations { metadata = metadata.saturating_add(operation.canonical.len() + operation.signature.len()); }
    for receipt in &receipt_records { metadata = metadata.saturating_add(receipt.encoded_len()); }
    if metadata > METADATA_BYTES { return Err(Error::Invalid("source metadata exceeds 16 MiB")); }
    if revision.spool != thread.spool { return Err(Error::Invalid("source revision crosses Spool")); }
    let genesis = super::verify_origin(original, thread)?;
    let Some(revision_ref::Revision::State(selected)) = revision.revision.as_ref() else {
        return Err(Error::Invalid("exact native State required"));
    };
    let selected_thread = genesis.id().map_err(preparation)?;
    let mut geneses = BTreeMap::from([(selected_thread, genesis)]);
    let mut dependencies = Vec::new();
    for wrapper in dependency_records {
        let record = wrapper
            .genesis
            .as_ref()
            .ok_or(Error::Invalid("dependency signed genesis absent"))?;
        let candidate = heddle_object_model::object::thread_replication::ThreadGenesis::decode(
            &record.canonical_record,
        )
        .map_err(preparation)?;
        let reference = ThreadRef {
            spool: thread.spool.clone(),
            id: Some(ThreadId {
                value: candidate.id().map_err(preparation)?.as_bytes().to_vec(),
            }),
        };
        let candidate = super::verify_origin(&wrapper, &reference)?;
        let id = candidate.id().map_err(preparation)?;
        if geneses.len() >= 128 || geneses.insert(id, candidate).is_some() {
            return Err(Error::Invalid(
                "duplicate or oversized dependency genesis set",
            ));
        }
        dependencies.push(wrapper);
    }
    let mut originals = BTreeMap::new();
    let mut decoded = BTreeMap::<ContentHash, ThreadOperation>::new();
    let mut selected_operation = None;
    for signed in &operations {
        let operation = signed.verify().map_err(preparation)?;
        let id = operation.id().map_err(preparation)?;
        let state = operation
            .source_state()
            .map_err(preparation)?
            .ok_or(Error::Invalid("non-source operation in source ancestry"))?;
        if operation.thread == selected_thread && state.id().as_bytes().as_slice() == selected.value
        {
            if selected_operation.replace((id, state)).is_some() {
                return Err(Error::Invalid("ambiguous selected source proof"));
            }
        }
        originals.insert(id, signed.clone());
        if decoded.insert(id, operation).is_some() {
            return Err(Error::Invalid("duplicate source proof"));
        }
    }
    let mut authority_admissions = BTreeMap::new();
    for record in receipt_records {
        let receipt = crate::authority_admission::decode(&record)?;
        let statement = receipt.verify_signature().map_err(preparation)?;
        let operation_id = statement.subject.operation_id().ok_or(Error::Invalid("source batch cannot carry ownership claim admission"))?;
        let original = decoded.get(&operation_id).ok_or(Error::Invalid("unmatched source authority receipt"))?;
        // Match immutable claims and signatures only. This self-described key
        // is not enrolled here; the receiver must independently pin the issuer.
        statement.authorize(original, &heddle_object_model::object::thread_replication::integration::TrustedHostedExecutor {
            spool: statement.spool, spool_genesis: statement.spool_genesis, executor: statement.executor,
        }).map_err(preparation)?;
        if authority_admissions.insert(operation_id, receipt).is_some() {
            return Err(Error::Invalid("duplicate source authority receipt"));
        }
    }
    let (selected_id, state) =
        selected_operation.ok_or(Error::Invalid("selected source proof absent"))?;
    let mut pending = BTreeSet::from([selected_id]);
    let mut seen = BTreeSet::new();
    let mut used_threads = BTreeSet::new();
    let mut edges = BTreeMap::new();
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
        let genesis = geneses
            .get(&operation.thread)
            .ok_or(Error::Invalid("source dependency genesis absent"))?;
        used_threads.insert(operation.thread);
        operation
            .validate_parents(genesis, &parents)
            .map_err(preparation)?;
        let mut required = operation.parents.clone();
        if let Some(receipt) = operation.local_integration().map_err(preparation)? {
            let source = decoded
                .get(&receipt.source_operation)
                .ok_or(Error::Invalid(
                    "local integration original source proof absent",
                ))?;
            receipt.validate_source(source).map_err(preparation)?;
            required.insert(receipt.source_operation);
            pending.insert(receipt.source_operation);
        }
        if let Some(receipt) = operation.integration().map_err(preparation)? {
            let source = decoded
                .get(&receipt.source_operation)
                .ok_or(Error::Invalid(
                    "hosted integration original source proof absent",
                ))?;
            receipt.validate_source(source).map_err(preparation)?;
            required.insert(receipt.source_operation);
            pending.insert(receipt.source_operation);
        }
        edges.insert(id, required);
        pending.extend(
            operation
                .parents
                .iter()
                .filter(|id| !seen.contains(id))
                .copied(),
        );
    }
    if seen.len() != decoded.len() || used_threads.len() != geneses.len() {
        return Err(Error::Invalid("unselected source proofs"));
    }
    let references = decoded
        .values()
        .map(|operation| {
            operation
                .reference_proof(
                    geneses
                        .get(&operation.thread)
                        .ok_or(Error::Invalid("dependency genesis absent"))?,
                )
                .map_err(preparation)
        })
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
    // Dependency-first installation makes foreign source authority available
    // before admitting a local integration. Cycles cannot settle this graph.
    let mut ready_ids: BTreeSet<_> = edges
        .iter()
        .filter(|(_, parents)| parents.is_empty())
        .map(|(id, _)| *id)
        .collect();
    let mut children: BTreeMap<ContentHash, Vec<ContentHash>> = BTreeMap::new();
    for (child, parents) in &edges {
        for parent in parents {
            children.entry(*parent).or_default().push(*child);
        }
    }
    let mut ordered = Vec::new();
    while let Some(id) = ready_ids.pop_first() {
        ordered.push(
            originals
                .remove(&id)
                .ok_or(Error::Invalid("duplicate source topology identity"))?,
        );
        if let Some(dependants) = children.get(&id) {
            for child in dependants {
                let parents = edges
                    .get_mut(child)
                    .ok_or(Error::Invalid("incomplete source topology"))?;
                parents.remove(&id);
                if parents.is_empty() {
                    ready_ids.insert(*child);
                }
            }
        }
    }
    if !originals.is_empty() {
        return Err(Error::Invalid("source dependency cycle"));
    }
    Ok(ValidatedSourceArtifacts {
        directory,
        genesis: original.clone(),
        operations: ordered,
        authority_admissions,
        dependencies,
        state,
    })
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}

#[cfg(test)]
#[path = "staging_tests.rs"]
mod tests;
