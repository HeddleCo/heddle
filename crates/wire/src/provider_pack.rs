// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use objects::store::{
    ObjectStore,
    pack::{PackContainerSpec, PackIndex, PackObjectId, PackReader, verify_container},
};

use crate::{
    MAX_RECEIVED_PACK_SIZE, NativePackBundle, ProtocolError, Result, native_pack::unique_spool_dir,
};

const PACK_HEADER_LEN: usize = 16;
const PACK_TRAILER_LEN: usize = 32;
const PACK_SPEC: PackContainerSpec = PackContainerSpec {
    magic: b"LMPK",
    version: 4,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPackIndexEntry {
    pub id: PackObjectId,
    pub output_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPackExtent {
    pub output_offset: u64,
    pub length: u64,
    pub digest: [u8; 32],
    pub objects: Vec<ProviderPackIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPackManifest {
    pub header: [u8; PACK_HEADER_LEN],
    pub output_pack_length: u64,
    pub extents: Vec<ProviderPackExtent>,
}

#[derive(Debug)]
pub struct ProviderPackBundle {
    pub pack: NativePackBundle,
    pub trailer_digest: [u8; 32],
}

/// A bounded-memory positional writer for one validated provider pack plan.
#[derive(Clone, Debug)]
pub struct ProviderPackWriter {
    file: Arc<File>,
    ranges: Arc<Vec<(u64, u64)>>,
    verified: Arc<Mutex<Vec<bool>>>,
}

/// A pre-sized provider pack spool that owns all partial transfer state.
#[derive(Debug)]
pub struct ProviderPackSpool {
    dir: Option<PathBuf>,
    pack_path: PathBuf,
    index_path: PathBuf,
    file: Arc<File>,
    manifest: ProviderPackManifest,
    verified: Arc<Mutex<Vec<bool>>>,
}

/// A completely covered and validated provider pack ready for atomic install.
#[derive(Debug)]
pub struct CompletedProviderPack {
    dir: PathBuf,
    pack_path: PathBuf,
    index_path: PathBuf,
    pub trailer_digest: [u8; 32],
}

impl ProviderPackSpool {
    /// Create an exact-length spool and write the validated virtual pack header.
    pub fn new_in(root: &Path, manifest: ProviderPackManifest) -> Result<Self> {
        validate_manifest(&manifest)?;
        let base = root.join("transfer-spool");
        fs::create_dir_all(&base)?;
        let dir = unique_spool_dir(&base)?;
        let pack_path = dir.join("provider.pack");
        let index_path = dir.join("provider.idx");
        let create_result = (|| -> Result<File> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&pack_path)?;
            file.set_len(manifest.output_pack_length)?;
            write_all_at(&file, &manifest.header, 0)?;
            Ok(file)
        })();
        let file = match create_result {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(error);
            }
        };
        let verified = Arc::new(Mutex::new(vec![false; manifest.extents.len()]));
        Ok(Self {
            dir: Some(dir),
            pack_path,
            index_path,
            file: Arc::new(file),
            manifest,
            verified,
        })
    }

    /// Obtain a cloneable positional writer for concurrent extent streams.
    pub fn writer(&self) -> ProviderPackWriter {
        ProviderPackWriter {
            file: Arc::clone(&self.file),
            ranges: Arc::new(
                self.manifest
                    .extents
                    .iter()
                    .map(|extent| (extent.output_offset, extent.length))
                    .collect(),
            ),
            verified: Arc::clone(&self.verified),
        }
    }

    /// Finalize the trailer and index after every extent has verified exactly once.
    pub fn finish(mut self) -> Result<CompletedProviderPack> {
        let verified = self.verified.lock().map_err(|_| {
            ProtocolError::InvalidState("provider spool verification lock poisoned".to_string())
        })?;
        if verified.iter().any(|complete| !complete) {
            return Err(ProtocolError::InvalidState(
                "provider spool does not have complete verified coverage".to_string(),
            ));
        }
        drop(verified);

        let body_end = self.manifest.output_pack_length - PACK_TRAILER_LEN as u64;
        let trailer_digest = hash_file_prefix(&self.file, body_end)?;
        write_all_at(&self.file, &trailer_digest, body_end)?;
        objects::fs_atomic::sync_file(&self.file, &self.pack_path)?;
        write_provider_index(&self.index_path, &self.manifest)?;

        let reader = PackReader::open(&self.pack_path, &self.index_path)?;
        let expected_objects = self
            .manifest
            .extents
            .iter()
            .map(|extent| extent.objects.len())
            .sum::<usize>();
        if reader.list_ids()?.len() != expected_objects {
            return Err(ProtocolError::InvalidState(
                "provider pack index does not match its manifest".to_string(),
            ));
        }
        drop(reader);

        let dir = self.dir.take().ok_or_else(|| {
            ProtocolError::InvalidState("provider spool directory is missing".to_string())
        })?;
        Ok(CompletedProviderPack {
            dir,
            pack_path: self.pack_path.clone(),
            index_path: self.index_path.clone(),
            trailer_digest,
        })
    }
}

impl ProviderPackWriter {
    /// Write one chunk at its manifest-assigned extent-relative position.
    pub fn write_extent_chunk(
        &self,
        extent_index: usize,
        relative_offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let (output_offset, extent_len) =
            self.ranges.get(extent_index).copied().ok_or_else(|| {
                ProtocolError::InvalidState("provider extent index is out of range".to_string())
            })?;
        let data_len = u64::try_from(data.len()).map_err(|_| {
            ProtocolError::InvalidState("provider chunk length exceeds u64".to_string())
        })?;
        let relative_end = relative_offset.checked_add(data_len).ok_or_else(|| {
            ProtocolError::InvalidState("provider extent write offset overflows".to_string())
        })?;
        if relative_end > extent_len {
            return Err(ProtocolError::InvalidState(
                "provider extent write exceeds its planned range".to_string(),
            ));
        }
        let absolute_offset = output_offset.checked_add(relative_offset).ok_or_else(|| {
            ProtocolError::InvalidState("provider spool write offset overflows".to_string())
        })?;
        write_all_at(&self.file, data, absolute_offset)
    }

    /// Rehash a retained prefix without retaining the extent body in memory.
    pub fn hash_extent_prefix(
        &self,
        extent_index: usize,
        prefix_len: u64,
        hasher: &mut blake3::Hasher,
    ) -> Result<()> {
        let (output_offset, extent_len) =
            self.ranges.get(extent_index).copied().ok_or_else(|| {
                ProtocolError::InvalidState("provider extent index is out of range".to_string())
            })?;
        if prefix_len > extent_len {
            return Err(ProtocolError::InvalidState(
                "provider retained prefix exceeds its planned extent".to_string(),
            ));
        }
        let mut buffer = [0_u8; 64 * 1024];
        let mut read = 0_u64;
        while read < prefix_len {
            let remaining = prefix_len - read;
            let length = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                ProtocolError::InvalidState("provider prefix length exceeds usize".to_string())
            })?;
            let offset = output_offset.checked_add(read).ok_or_else(|| {
                ProtocolError::InvalidState("provider prefix read offset overflows".to_string())
            })?;
            read_exact_at(&self.file, &mut buffer[..length], offset)?;
            hasher.update(&buffer[..length]);
            read += length as u64;
        }
        Ok(())
    }

    /// Mark one fully length- and digest-verified extent complete exactly once.
    pub fn mark_verified(&self, extent_index: usize) -> Result<()> {
        let mut verified = self.verified.lock().map_err(|_| {
            ProtocolError::InvalidState("provider spool verification lock poisoned".to_string())
        })?;
        let complete = verified.get_mut(extent_index).ok_or_else(|| {
            ProtocolError::InvalidState("provider extent index is out of range".to_string())
        })?;
        if *complete {
            return Err(ProtocolError::InvalidState(
                "provider extent completed more than once".to_string(),
            ));
        }
        *complete = true;
        Ok(())
    }
}

impl CompletedProviderPack {
    /// Atomically install a fully validated provider pack into the object store.
    pub fn install_into(&mut self, store: &impl ObjectStore) -> Result<Vec<PackObjectId>> {
        store
            .install_pack_streaming(&self.pack_path, &self.index_path)
            .map_err(ProtocolError::from)
    }
}

impl Drop for ProviderPackSpool {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.as_ref() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

impl Drop for CompletedProviderPack {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn write_provider_index(path: &Path, manifest: &ProviderPackManifest) -> Result<()> {
    let mut index = PackIndex::new();
    for extent in &manifest.extents {
        for object in &extent.objects {
            index.add(object.id, object.output_offset);
        }
    }
    index.sort();
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&index.to_bytes())?;
    file.flush()?;
    objects::fs_atomic::sync_file(&file, path)?;
    Ok(())
}

fn hash_file_prefix(file: &File, length: u64) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        let remaining = length - offset;
        let read_len = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            ProtocolError::InvalidState("provider pack hash length exceeds usize".to_string())
        })?;
        read_exact_at(file, &mut buffer[..read_len], offset)?;
        hasher.update(&buffer[..read_len]);
        offset += read_len as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(unix)]
fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;

    while !data.is_empty() {
        let written = file.write_at(data, offset)?;
        if written == 0 {
            return Err(ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "provider positional spool write returned zero",
            )));
        }
        data = &data[written..];
        offset = offset.checked_add(written as u64).ok_or_else(|| {
            ProtocolError::InvalidState("provider positional write offset overflows".to_string())
        })?;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;

    while !data.is_empty() {
        let written = file.seek_write(data, offset)?;
        if written == 0 {
            return Err(ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "provider positional spool write returned zero",
            )));
        }
        data = &data[written..];
        offset = offset.checked_add(written as u64).ok_or_else(|| {
            ProtocolError::InvalidState("provider positional write offset overflows".to_string())
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut data: &mut [u8], mut offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;

    while !data.is_empty() {
        let read = file.read_at(data, offset)?;
        if read == 0 {
            return Err(ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "provider positional spool read ended early",
            )));
        }
        data = &mut data[read..];
        offset = offset.checked_add(read as u64).ok_or_else(|| {
            ProtocolError::InvalidState("provider positional read offset overflows".to_string())
        })?;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut data: &mut [u8], mut offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;

    while !data.is_empty() {
        let read = file.seek_read(data, offset)?;
        if read == 0 {
            return Err(ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "provider positional spool read ended early",
            )));
        }
        data = &mut data[read..];
        offset = offset.checked_add(read as u64).ok_or_else(|| {
            ProtocolError::InvalidState("provider positional read offset overflows".to_string())
        })?;
    }
    Ok(())
}

/// Assemble and verify one virtual native pack from provider extent bodies.
///
/// `extent_bodies` uses the same order as `manifest.extents`. The manifest may
/// arrive in any order, but its output layout must cover every byte between the
/// 16-byte header and 32-byte trailer exactly once.
pub fn assemble_provider_pack(
    manifest: &ProviderPackManifest,
    extent_bodies: &[Vec<u8>],
) -> Result<ProviderPackBundle> {
    validate_manifest(manifest)?;
    if extent_bodies.len() != manifest.extents.len() {
        return Err(ProtocolError::InvalidState(format!(
            "provider extent count mismatch: expected {}, got {}",
            manifest.extents.len(),
            extent_bodies.len()
        )));
    }

    let mut order = (0..manifest.extents.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| manifest.extents[*index].output_offset);
    let output_len = usize::try_from(manifest.output_pack_length).map_err(|_| {
        ProtocolError::InvalidState("provider output pack exceeds this platform".to_string())
    })?;
    let mut pack_data = Vec::with_capacity(output_len);
    pack_data.extend_from_slice(&manifest.header);
    for index in order {
        let extent = &manifest.extents[index];
        let body = &extent_bodies[index];
        let expected_len = usize::try_from(extent.length).map_err(|_| {
            ProtocolError::InvalidState("provider extent exceeds this platform".to_string())
        })?;
        if body.len() != expected_len {
            return Err(ProtocolError::InvalidState(format!(
                "provider extent length mismatch: expected {}, got {}",
                extent.length,
                body.len()
            )));
        }
        if blake3::hash(body).as_bytes() != &extent.digest {
            return Err(ProtocolError::InvalidState(
                "provider extent digest mismatch".to_string(),
            ));
        }
        pack_data.extend_from_slice(body);
    }

    let trailer_digest = *blake3::hash(&pack_data).as_bytes();
    pack_data.extend_from_slice(&trailer_digest);
    if pack_data.len() != output_len {
        return Err(ProtocolError::InvalidState(format!(
            "provider output pack length mismatch: expected {}, got {}",
            manifest.output_pack_length,
            pack_data.len()
        )));
    }
    verify_container(&pack_data, PACK_SPEC).map_err(ProtocolError::from)?;

    let mut index = PackIndex::new();
    for extent in &manifest.extents {
        for object in &extent.objects {
            index.add(object.id, object.output_offset);
        }
    }
    index.sort();

    Ok(ProviderPackBundle {
        pack: NativePackBundle {
            pack_data,
            index_data: index.to_bytes(),
        },
        trailer_digest,
    })
}

fn validate_manifest(manifest: &ProviderPackManifest) -> Result<()> {
    if manifest.output_pack_length > MAX_RECEIVED_PACK_SIZE
        || manifest.output_pack_length < (PACK_HEADER_LEN + PACK_TRAILER_LEN) as u64
    {
        return Err(ProtocolError::InvalidState(
            "provider output pack length is invalid".to_string(),
        ));
    }
    if &manifest.header[..4] != PACK_SPEC.magic
        || u32::from_be_bytes(manifest.header[4..8].try_into().map_err(|_| {
            ProtocolError::InvalidState("provider pack header is truncated".to_string())
        })?) != PACK_SPEC.version
    {
        return Err(ProtocolError::InvalidState(
            "provider pack header has invalid magic or version".to_string(),
        ));
    }
    let object_count = u64::from_be_bytes(manifest.header[8..16].try_into().map_err(|_| {
        ProtocolError::InvalidState("provider pack header is truncated".to_string())
    })?);
    let expected_body_end = manifest.output_pack_length - PACK_TRAILER_LEN as u64;
    let mut order = manifest.extents.iter().collect::<Vec<_>>();
    order.sort_unstable_by_key(|extent| extent.output_offset);
    let mut next_offset = PACK_HEADER_LEN as u64;
    let mut ids = HashSet::new();
    let mut object_offsets = HashSet::new();
    let mut actual_object_count = 0_u64;

    for extent in order {
        if extent.length == 0 || extent.output_offset != next_offset {
            return Err(ProtocolError::InvalidState(
                "provider extents do not exactly cover the virtual pack body".to_string(),
            ));
        }
        next_offset = extent
            .output_offset
            .checked_add(extent.length)
            .ok_or_else(|| {
                ProtocolError::InvalidState("provider extent output range overflows".to_string())
            })?;
        if next_offset > expected_body_end {
            return Err(ProtocolError::InvalidState(
                "provider extent exceeds the virtual pack body".to_string(),
            ));
        }
        for object in &extent.objects {
            if object.output_offset < extent.output_offset
                || object.output_offset >= next_offset
                || !ids.insert(object.id)
                || !object_offsets.insert(object.output_offset)
            {
                return Err(ProtocolError::InvalidState(
                    "provider object index is outside its extent or duplicated".to_string(),
                ));
            }
            actual_object_count = actual_object_count.checked_add(1).ok_or_else(|| {
                ProtocolError::InvalidState("provider object count overflows".to_string())
            })?;
        }
    }
    if next_offset != expected_body_end || actual_object_count != object_count {
        return Err(ProtocolError::InvalidState(
            "provider manifest body or object count is incomplete".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::{
        object::ContentHash,
        store::{
            CompressionConfig,
            pack::{ObjectType, PackBuilder, PackIndex, PackObjectId},
        },
    };

    use super::*;

    fn source_pack() -> (Vec<u8>, Vec<u8>, Vec<PackObjectId>) {
        let ids = vec![
            PackObjectId::Hash(ContentHash::from_bytes([1; 32])),
            PackObjectId::Hash(ContentHash::from_bytes([2; 32])),
        ];
        let mut builder = PackBuilder::new(CompressionConfig {
            enabled: false,
            ..CompressionConfig::default()
        });
        builder.add_id(ids[0], ObjectType::Blob, b"provider-one".to_vec());
        builder.add_id(ids[1], ObjectType::Blob, b"provider-two".to_vec());
        let (pack, index, _) = builder.build().unwrap();
        (pack, index, ids)
    }

    fn split_manifest() -> (ProviderPackManifest, Vec<Vec<u8>>, Vec<u8>, Vec<u8>) {
        let (pack, index, ids) = source_pack();
        let parsed_index = PackIndex::from_bytes(&index).unwrap();
        let first = parsed_index
            .find(&ids[0])
            .unwrap()
            .expect("first fixture object indexed");
        let second = parsed_index
            .find(&ids[1])
            .unwrap()
            .expect("second fixture object indexed");
        let (first_id, first_offset, second_id, second_offset) = if first < second {
            (ids[0], first, ids[1], second)
        } else {
            (ids[1], second, ids[0], first)
        };
        let body_end = pack.len() - PACK_TRAILER_LEN;
        let first_body = pack[first_offset as usize..second_offset as usize].to_vec();
        let second_body = pack[second_offset as usize..body_end].to_vec();
        let manifest = ProviderPackManifest {
            header: pack[..PACK_HEADER_LEN].try_into().unwrap(),
            output_pack_length: pack.len() as u64,
            extents: vec![
                ProviderPackExtent {
                    output_offset: first_offset,
                    length: first_body.len() as u64,
                    digest: *blake3::hash(&first_body).as_bytes(),
                    objects: vec![ProviderPackIndexEntry {
                        id: first_id,
                        output_offset: first_offset,
                    }],
                },
                ProviderPackExtent {
                    output_offset: second_offset,
                    length: second_body.len() as u64,
                    digest: *blake3::hash(&second_body).as_bytes(),
                    objects: vec![ProviderPackIndexEntry {
                        id: second_id,
                        output_offset: second_offset,
                    }],
                },
            ],
        };
        (manifest, vec![first_body, second_body], pack, index)
    }

    #[test]
    fn provider_and_ordinary_pack_results_are_byte_identical() {
        let (manifest, bodies, source_pack, source_index) = split_manifest();

        let assembled = assemble_provider_pack(&manifest, &bodies).unwrap();

        assert_eq!(assembled.pack.pack_data, source_pack);
        assert_eq!(assembled.pack.index_data, source_index);
        let source_digest = blake3::Hash::from_bytes(
            source_pack[source_pack.len() - PACK_TRAILER_LEN..]
                .try_into()
                .unwrap(),
        );
        println!(
            "byte_identical provider_digest={} weft_digest={} pack_bytes={} index_bytes={} identical=true",
            blake3::Hash::from_bytes(assembled.trailer_digest),
            source_digest,
            source_pack.len(),
            source_index.len(),
        );
    }

    #[test]
    fn digest_mismatch_never_produces_an_installable_pack() {
        let (manifest, mut bodies, _, _) = split_manifest();
        bodies[1][0] ^= 0xff;

        let error = assemble_provider_pack(&manifest, &bodies).unwrap_err();

        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn manifest_gaps_and_invalid_or_duplicate_index_entries_fail_closed() {
        let (mut manifest, bodies, _, _) = split_manifest();
        manifest.extents[1].output_offset += 1;
        assert!(assemble_provider_pack(&manifest, &bodies).is_err());

        let (mut manifest, bodies, _, _) = split_manifest();
        manifest.extents[1].objects[0].output_offset = manifest.extents[0].output_offset;
        assert!(assemble_provider_pack(&manifest, &bodies).is_err());

        let (mut manifest, bodies, _, _) = split_manifest();
        manifest.extents[1].objects[0].id = manifest.extents[0].objects[0].id;
        assert!(assemble_provider_pack(&manifest, &bodies).is_err());
    }

    #[test]
    fn positional_spool_accepts_out_of_order_extents_and_is_byte_identical() {
        let (manifest, bodies, source_pack, source_index) = split_manifest();
        let root = tempfile::tempdir().unwrap();
        let spool = ProviderPackSpool::new_in(root.path(), manifest).unwrap();
        assert_eq!(
            spool.file.metadata().unwrap().len(),
            source_pack.len() as u64,
            "the sparse spool must be pre-sized to the exact virtual pack length"
        );
        let writer = spool.writer();

        writer.write_extent_chunk(1, 0, &bodies[1]).unwrap();
        writer.mark_verified(1).unwrap();
        writer.write_extent_chunk(0, 0, &bodies[0]).unwrap();
        writer.mark_verified(0).unwrap();
        drop(writer);

        let completed = spool.finish().unwrap();
        assert_eq!(fs::read(&completed.pack_path).unwrap(), source_pack);
        assert_eq!(fs::read(&completed.index_path).unwrap(), source_index);
    }

    #[test]
    fn positional_spool_rejects_range_overrun_duplicate_and_incomplete_coverage() {
        let (manifest, bodies, _, _) = split_manifest();
        let root = tempfile::tempdir().unwrap();
        let spool = ProviderPackSpool::new_in(root.path(), manifest).unwrap();
        let spool_dir = spool.dir.clone().unwrap();
        let writer = spool.writer();

        assert!(
            writer
                .write_extent_chunk(0, bodies[0].len() as u64, &[1])
                .is_err()
        );
        writer.write_extent_chunk(0, 0, &bodies[0]).unwrap();
        writer.mark_verified(0).unwrap();
        assert!(writer.mark_verified(0).is_err());
        drop(writer);

        assert!(spool.finish().is_err());
        assert!(
            !spool_dir.exists(),
            "failed finalization must remove partial spool state"
        );
    }

    #[test]
    fn dropping_partial_spool_removes_all_transfer_state() {
        let (manifest, bodies, _, _) = split_manifest();
        let root = tempfile::tempdir().unwrap();
        let spool = ProviderPackSpool::new_in(root.path(), manifest).unwrap();
        let spool_dir = spool.dir.clone().unwrap();
        let writer = spool.writer();
        writer
            .write_extent_chunk(0, 0, &bodies[0][..bodies[0].len() / 2])
            .unwrap();

        drop(writer);
        drop(spool);

        assert!(!spool_dir.exists());
    }
}
