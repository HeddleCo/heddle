//! Shared actual-artifact validation for hosted and owned-device publication.
//! Original signatures and causal closure are necessary but do not authorize
//! their authors, current audience, hosted executors, or destination policy.
use std::io::Read;

use super::PublicationOriginals;
use crate::{contract::*, fetch::{Error, ValidatedSourceArtifacts}, replication};

/// Call on a bounded disk worker after the complete upload. The directory must
/// contain only the uploaded `source.pack` and `source.idx`; validation never
/// falls back to objects already installed in a shared store. Dropping either
/// the input on failure or the result on cancellation removes its scratch.
pub fn validate_source_artifacts(
    directory: tempfile::TempDir,
    opening: &PublishContentOpen,
    originals: PublicationOriginals,
) -> Result<ValidatedSourceArtifacts, Error> {
    originals.validate_bounds().map_err(preparation)?;
    let thread = opening.thread.as_ref().ok_or(Error::Invalid("publication Thread absent"))?;
    let revision = opening.revision.as_ref().ok_or(Error::Invalid("publication revision absent"))?;
    if opening.packs.len() != 2
        || opening.packs[0].kind != pack_extent::Kind::NativePack as i32
        || opening.packs[1].kind != pack_extent::Kind::NativeIndex as i32
    { return Err(Error::Invalid("ordered native pack and index required")); }
    let mut total = 0_u64;
    for (extent, name) in opening.packs.iter().zip(["source.pack", "source.idx"]) {
        let address = extent.pack.as_ref().ok_or(Error::Invalid("artifact address absent"))?;
        total = total.checked_add(extent.length).ok_or(Error::Invalid("artifact length overflow"))?;
        if address.algorithm != "blake3" || address.digest.len() != 32 || extent.offset != 0
            || extent.length == 0 || extent.extent_digest.as_ref() != Some(address)
            || total > 256 * 1024 * 1024
        { return Err(Error::Invalid("invalid or oversized publication artifact")); }
        let mut file = std::fs::File::open(directory.path().join(name))?;
        if file.metadata()?.len() != extent.length { return Err(Error::Invalid("uploaded artifact length differs")); }
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 { break; }
            hasher.update(&buffer[..read]);
        }
        if hasher.finalize().as_bytes().as_slice() != address.digest {
            return Err(Error::Invalid("uploaded artifact digest differs"));
        }
    }
    let selected = thread.id.as_ref().ok_or(Error::Invalid("Thread identity absent"))?;
    let mut genesis = None;
    let mut dependencies = Vec::new();
    for wrapper in originals.geneses {
        let record = wrapper.genesis.as_ref().ok_or(Error::Invalid("original genesis absent"))?;
        let candidate = heddle_object_model::object::thread_replication::ThreadGenesis::decode(&record.canonical_record).map_err(preparation)?;
        if candidate.id().map_err(preparation)?.as_bytes().as_slice() == selected.value {
            if genesis.replace(wrapper).is_some() { return Err(Error::Invalid("duplicate selected genesis")); }
        } else { dependencies.push(wrapper); }
    }
    let mut operations = Vec::new();
    let mut receipts = Vec::new();
    for batch in originals.operations {
        crate::authority_admission::match_batch(&batch)?;
        operations.extend(batch.operations.into_iter().map(replication::decode_record).collect::<Result<Vec<_>, _>>()?);
        receipts.extend(batch.authority_admissions);
    }
    crate::fetch::validate_artifacts(directory, thread, revision,
        &genesis.ok_or(Error::Invalid("original selected genesis absent"))?, operations, dependencies, receipts)

}
fn preparation(error: impl std::fmt::Display) -> Error { Error::Preparation(error.to_string()) }
