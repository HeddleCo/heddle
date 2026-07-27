// SPDX-License-Identifier: Apache-2.0
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use objects::store::{
    CompressionConfig, ObjectStore,
    pack::{PackBuilder, PackObjectId, PackReader, StreamingPackBuilder},
};

use crate::{
    ObjectData, ObjectId, ObjectInfo, ObjectType, ProtocolError, Result, load_object_data,
};

/// Maximum hosted native-pack body accepted by the receive primitive.
///
/// Native sync packs are produced from bounded state-closure wants and
/// each decoded pack object is separately capped at 1 GiB in the pack
/// reader. A 2 GiB compressed pack is materially above normal hosted
/// sync use while still preventing an untrusted server from growing the
/// in-memory receive buffer without limit. The receive path can now move
/// to temp-file spooling plus `install_pack_streaming` — that install API
/// reports the installed ids the receiver needs, so only the spooling of
/// the receive buffer itself remains.
pub const MAX_RECEIVED_PACK_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum hosted native-pack index accepted by the receive primitive.
///
/// Pack indexes are proportional to object count, not object payload
/// size. 256 MiB leaves room for millions of entries while bounding the
/// second in-memory buffer controlled by the remote sender.
pub const MAX_RECEIVED_PACK_INDEX_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum hosted Git pack accepted by the Git-lane transfer primitive.
///
/// Git-overlay sync sends Git-shaped data as raw Git packs. The sender and
/// receiver still stream those bytes in bounded chunks, but the declared pack
/// size is untrusted wire input and needs a hard ceiling before buffering or
/// spooling work begins.
pub const MAX_RECEIVED_GIT_PACK_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NativePackBundle {
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
}

#[derive(Debug)]
pub struct NativePackFileBundle {
    dir: PathBuf,
    pub pack_path: PathBuf,
    pub index_path: PathBuf,
    pub pack_len: u64,
    pub index_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReusedNativePackStats {
    pub object_count: usize,
    pub encoded_bytes_copied: u64,
}

/// Build a hosted transport pack by reusing non-delta encoded entries from an
/// authoritative local pack. `Ok(None)` means the caller must use the normal
/// object-loading writer.
pub fn reuse_native_pack_encoded_subset_in(
    root: &Path,
    source_pack_path: &Path,
    objects: &[ObjectInfo],
) -> Result<Option<(NativePackFileBundle, ReusedNativePackStats)>> {
    if objects.is_empty()
        || objects
            .iter()
            .any(|object| !object.obj_type.packable_for_push())
    {
        return Ok(None);
    }
    let source_index_path = source_pack_path.with_extension("idx");
    if !source_pack_path.is_file() || !source_index_path.is_file() {
        return Ok(None);
    }
    let reader = PackReader::open(source_pack_path, &source_index_path)?;
    let expected = objects
        .iter()
        .map(|object| {
            Ok((
                to_pack_object_id(&object.id),
                object.obj_type.pack_object_type()?,
                object.size,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(reused) = reader.copy_hosted_encoded_subset(&expected)? else {
        return Ok(None);
    };

    let base = root.join("transfer-spool");
    fs::create_dir_all(&base)?;
    let dir = unique_spool_dir(&base)?;
    let pack_path = dir.join("pack");
    let index_path = dir.join("idx");
    let write_result = (|| -> Result<(u64, u64)> {
        fs::write(&pack_path, &reused.pack_data)?;
        fs::write(&index_path, &reused.index_data)?;
        Ok((
            u64::try_from(reused.pack_data.len()).map_err(|_| {
                ProtocolError::InvalidState("reused pack length exceeds u64".to_string())
            })?,
            u64::try_from(reused.index_data.len()).map_err(|_| {
                ProtocolError::InvalidState("reused pack index length exceeds u64".to_string())
            })?,
        ))
    })();
    let (pack_len, index_len) = match write_result {
        Ok(lengths) => lengths,
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
    };
    Ok(Some((
        NativePackFileBundle {
            dir,
            pack_path,
            index_path,
            pack_len,
            index_len,
        },
        ReusedNativePackStats {
            object_count: objects.len(),
            encoded_bytes_copied: reused.encoded_bytes_copied,
        },
    )))
}

impl Drop for NativePackFileBundle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug)]
pub struct PackFileChunkReader {
    file: File,
    total_len: u64,
    chunk_size: usize,
    offset: u64,
    chunk_index: u32,
}

pub type NativePackFileChunk = (u64, u32, Vec<u8>, bool);

impl PackFileChunkReader {
    pub fn open(path: &Path, chunk_size: usize) -> Result<Self> {
        let file = File::open(path)?;
        let total_len = file.metadata()?.len();
        Ok(Self {
            file,
            total_len,
            chunk_size: chunk_size.max(1),
            offset: 0,
            chunk_index: 0,
        })
    }

    pub fn next_chunk(&mut self) -> Result<Option<NativePackFileChunk>> {
        if self.offset >= self.total_len {
            return Ok(None);
        }
        let remaining = self.total_len - self.offset;
        let len = remaining.min(self.chunk_size as u64);
        let len = usize::try_from(len).map_err(|_| {
            ProtocolError::InvalidState("native pack file chunk length exceeds usize".to_string())
        })?;
        let mut data = vec![0u8; len];
        self.file.read_exact(&mut data)?;

        let offset = self.offset;
        let chunk_index = self.chunk_index;
        self.offset = self.offset.checked_add(len as u64).ok_or_else(|| {
            ProtocolError::InvalidState("native pack file chunk offset overflow".to_string())
        })?;
        self.chunk_index = self.chunk_index.checked_add(1).ok_or_else(|| {
            ProtocolError::InvalidState("native pack file chunk index overflow".to_string())
        })?;
        Ok(Some((
            offset,
            chunk_index,
            data,
            self.offset == self.total_len,
        )))
    }
}

#[derive(Debug)]
pub struct GrowingPackChunkReader {
    file: File,
    chunk_size: usize,
    offset: u64,
    chunk_index: u32,
}

impl GrowingPackChunkReader {
    pub fn open(path: &Path, chunk_size: usize) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            chunk_size: chunk_size.max(1),
            offset: 0,
            chunk_index: 0,
        })
    }

    pub fn next_available_chunk(
        &mut self,
        final_stream: bool,
    ) -> Result<Option<NativePackFileChunk>> {
        let total_len = self.file.metadata()?.len();
        if self.offset >= total_len {
            return Ok(None);
        }
        let available = total_len - self.offset;
        if !final_stream && available < self.chunk_size as u64 {
            return Ok(None);
        }

        let len = available.min(self.chunk_size as u64);
        let len = usize::try_from(len).map_err(|_| {
            ProtocolError::InvalidState(
                "growing native pack chunk length exceeds usize".to_string(),
            )
        })?;
        let mut data = vec![0u8; len];
        self.file.read_exact(&mut data)?;

        let offset = self.offset;
        let chunk_index = self.chunk_index;
        self.offset = self.offset.checked_add(len as u64).ok_or_else(|| {
            ProtocolError::InvalidState("growing native pack chunk offset overflow".to_string())
        })?;
        self.chunk_index = self.chunk_index.checked_add(1).ok_or_else(|| {
            ProtocolError::InvalidState("growing native pack chunk index overflow".to_string())
        })?;
        Ok(Some((
            offset,
            chunk_index,
            data,
            final_stream && self.offset == total_len,
        )))
    }
}

pub struct NativePackStreamingWriter {
    dir: Option<PathBuf>,
    pack_path: PathBuf,
    index_path: PathBuf,
    builder: Option<StreamingPackBuilder<File>>,
}

impl NativePackStreamingWriter {
    pub fn new_in(root: &Path, object_count: u64) -> Result<Self> {
        let base = root.join("transfer-spool");
        fs::create_dir_all(&base)?;
        let dir = unique_spool_dir(&base)?;
        let pack_path = dir.join("pack");
        let index_path = dir.join("idx");
        let bucket_dir = dir.join("buckets");
        let pack_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&pack_path)?;
        let builder = StreamingPackBuilder::new_with_object_count_ephemeral(
            pack_file,
            index_path.clone(),
            sync_pack_compression(),
            bucket_dir,
            object_count,
        )
        .map_err(ProtocolError::from)?;

        Ok(Self {
            dir: Some(dir),
            pack_path,
            index_path,
            builder: Some(builder),
        })
    }

    pub fn pack_path(&self) -> &Path {
        &self.pack_path
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    pub fn add_object_data(&mut self, object: ObjectData) -> Result<()> {
        if !is_native_packable_object_type(object.obj_type) {
            return Err(ProtocolError::InvalidState(format!(
                "{:?} sidecar records cannot be packed into the content-addressed object pack",
                object.obj_type
            )));
        }
        let builder = self.builder.as_mut().ok_or_else(|| {
            ProtocolError::InvalidState("native pack streaming writer is finalized".to_string())
        })?;
        let pack_id = to_pack_object_id(&object.id);
        builder
            .add_id(pack_id, object.obj_type.pack_object_type()?, object.data)
            .map_err(ProtocolError::from)
    }

    pub fn flush_pack(&mut self) -> Result<()> {
        let builder = self.builder.as_mut().ok_or_else(|| {
            ProtocolError::InvalidState("native pack streaming writer is finalized".to_string())
        })?;
        builder.flush_pack().map_err(ProtocolError::from)
    }

    pub fn finish(mut self) -> Result<NativePackFileBundle> {
        let builder = self.builder.take().ok_or_else(|| {
            ProtocolError::InvalidState("native pack streaming writer is finalized".to_string())
        })?;
        let (mut file, _) = builder.finalize().map_err(ProtocolError::from)?;
        file.flush()?;
        drop(file);
        let pack_len = fs::metadata(&self.pack_path)?.len();
        let index_len = fs::metadata(&self.index_path)?.len();
        let dir = self.dir.take().ok_or_else(|| {
            ProtocolError::InvalidState("native pack streaming writer lost spool dir".to_string())
        })?;
        Ok(NativePackFileBundle {
            dir,
            pack_path: self.pack_path.clone(),
            index_path: self.index_path.clone(),
            pack_len,
            index_len,
        })
    }
}

impl Drop for NativePackStreamingWriter {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PackChunkState {
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pack_progress: (u64, u32),
    index_progress: (u64, u32),
    pack_complete: bool,
    index_complete: bool,
}

impl PackChunkState {
    pub fn is_complete(&self) -> bool {
        self.pack_complete && self.index_complete
    }
}

#[derive(Debug, Default, Clone)]
pub struct GitPackChunkState {
    transfer_id: Option<String>,
    pack_size: Option<u64>,
    next_offset: u64,
    next_chunk_index: u32,
    pack_data: Vec<u8>,
}

impl GitPackChunkState {
    pub fn is_idle(&self) -> bool {
        self.transfer_id.is_none()
            && self.pack_size.is_none()
            && self.next_offset == 0
            && self.next_chunk_index == 0
            && self.pack_data.is_empty()
    }

    pub fn ensure_idle(&self) -> Result<()> {
        if self.is_idle() {
            Ok(())
        } else {
            Err(ProtocolError::InvalidState(
                "Git pack transfer ended before final chunk".to_string(),
            ))
        }
    }

    pub fn receive_chunk(
        &mut self,
        transfer_id: &str,
        offset: u64,
        chunk_index: u32,
        is_final_chunk: bool,
        pack_size: u64,
        data: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        if transfer_id.is_empty() {
            return Err(ProtocolError::InvalidState(
                "Git pack transfer_id is required".to_string(),
            ));
        }
        if pack_size > MAX_RECEIVED_GIT_PACK_SIZE {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack exceeds maximum transfer size of {MAX_RECEIVED_GIT_PACK_SIZE} bytes"
            )));
        }
        if data.is_empty() {
            return Err(ProtocolError::InvalidState(
                "Git pack chunk must not be empty".to_string(),
            ));
        }
        match self.transfer_id.as_ref() {
            Some(current) if current != transfer_id => {
                return Err(ProtocolError::InvalidState(format!(
                    "Git pack transfer id changed from {current:?} to {transfer_id:?}"
                )));
            }
            Some(_) => {}
            None => {
                self.transfer_id = Some(transfer_id.to_string());
                self.pack_size = Some(pack_size);
            }
        }
        if self.pack_size != Some(pack_size) {
            return Err(ProtocolError::InvalidState(
                "Git pack size changed during transfer".to_string(),
            ));
        }
        if offset != self.next_offset {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack offset mismatch: expected {}, got {}",
                self.next_offset, offset
            )));
        }
        if chunk_index != self.next_chunk_index {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack chunk index mismatch: expected {}, got {}",
                self.next_chunk_index, chunk_index
            )));
        }
        let chunk_len = u64::try_from(data.len()).map_err(|_| {
            ProtocolError::InvalidState("Git pack chunk length exceeds u64".to_string())
        })?;
        let next_offset = self
            .next_offset
            .checked_add(chunk_len)
            .ok_or_else(|| ProtocolError::InvalidState("Git pack offset overflow".to_string()))?;
        if next_offset > pack_size {
            return Err(ProtocolError::InvalidState(
                "Git pack chunk exceeds declared pack size".to_string(),
            ));
        }
        self.pack_data.extend_from_slice(data);
        self.next_offset = next_offset;
        self.next_chunk_index = self.next_chunk_index.checked_add(1).ok_or_else(|| {
            ProtocolError::InvalidState("Git pack chunk index overflow".to_string())
        })?;
        if is_final_chunk {
            if self.next_offset != pack_size {
                return Err(ProtocolError::InvalidState(format!(
                    "Git pack final size mismatch: declared {}, received {}",
                    pack_size, self.next_offset
                )));
            }
            let pack_data = std::mem::take(&mut self.pack_data);
            self.transfer_id = None;
            self.pack_size = None;
            self.next_offset = 0;
            self.next_chunk_index = 0;
            return Ok(Some(pack_data));
        }
        if self.next_offset == pack_size {
            return Err(ProtocolError::InvalidState(
                "Git pack reached declared size without final chunk marker".to_string(),
            ));
        }
        Ok(None)
    }
}

#[derive(Debug)]
pub struct PackChunkSpool {
    dir: PathBuf,
    pack: PackStreamSpool,
    index: PackStreamSpool,
}

impl PackChunkSpool {
    pub fn new_in(root: &Path) -> Result<Self> {
        let base = root.join("transfer-spool");
        fs::create_dir_all(&base)?;
        let dir = unique_spool_dir(&base)?;
        let pack = PackStreamSpool::new(dir.join("pack"))?;
        let index = PackStreamSpool::new(dir.join("idx"))?;
        Ok(Self { dir, pack, index })
    }

    pub fn is_complete(&self) -> bool {
        self.pack.complete && self.index.complete
    }

    #[allow(clippy::too_many_arguments)]
    pub fn receive_chunk(
        &mut self,
        is_index: bool,
        resume_offset: u64,
        chunk_index: u32,
        is_complete: bool,
        data: &[u8],
        is_final_chunk: bool,
    ) -> Result<()> {
        let max_bytes = if is_index {
            MAX_RECEIVED_PACK_INDEX_SIZE
        } else {
            MAX_RECEIVED_PACK_SIZE
        };
        let stream = if is_index {
            &mut self.index
        } else {
            &mut self.pack
        };
        receive_pack_chunk_to_spool(
            stream,
            is_index,
            resume_offset,
            chunk_index,
            is_complete,
            data,
            is_final_chunk,
            max_bytes,
        )
    }

    pub fn install_into(&mut self, store: &impl ObjectStore) -> Result<Vec<PackObjectId>> {
        if !self.is_complete() {
            return Err(ProtocolError::InvalidState(
                "native pack spool is incomplete".to_string(),
            ));
        }
        self.pack.close()?;
        self.index.close()?;
        store
            .install_pack_streaming(&self.pack.path, &self.index.path)
            .map_err(ProtocolError::from)
    }
}

impl Drop for PackChunkSpool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug)]
struct PackStreamSpool {
    path: PathBuf,
    file: Option<File>,
    progress: (u64, u32),
    complete: bool,
}

impl PackStreamSpool {
    fn new(path: PathBuf) -> Result<Self> {
        let file = File::create(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            progress: (0, 0),
            complete: false,
        })
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(ProtocolError::InvalidState(
                "native pack spool stream is already closed".to_string(),
            ));
        };
        file.write_all(data)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        Ok(())
    }
}

pub fn native_pack_excluded_object_types() -> &'static [ObjectType] {
    &[ObjectType::Redaction, ObjectType::StateVisibility]
}

pub fn is_native_packable_object_type(obj_type: ObjectType) -> bool {
    obj_type.packable()
}

pub fn build_native_pack(
    store: &impl ObjectStore,
    objects: &[ObjectInfo],
) -> Result<NativePackBundle> {
    let mut builder = PackBuilder::new(sync_pack_compression());

    for info in objects {
        // Sidecar records (redaction + state-visibility) live outside
        // `.heddle/objects/` so GC cannot touch them, and must not be
        // folded into the content-addressed pack. They ship via the
        // per-object transfer path instead; callers split them out before
        // packing.
        if !is_native_packable_object_type(info.obj_type) {
            continue;
        }
        let object = load_object_data(store, &info.id, info.obj_type)?;
        let pack_id = to_pack_object_id(&object.id);
        builder.add_id(pack_id, object.obj_type.pack_object_type()?, object.data);
    }

    let (pack_data, index_data, _) = builder.build()?;
    Ok(NativePackBundle {
        pack_data,
        index_data,
    })
}

fn sync_pack_compression() -> CompressionConfig {
    CompressionConfig {
        level: 1,
        min_size: 1024,
        max_delta_size: 0,
        ..CompressionConfig::default()
    }
}

pub fn install_received_pack(
    store: &impl ObjectStore,
    pack_data: &[u8],
    index_data: &[u8],
) -> Result<Vec<PackObjectId>> {
    store
        .install_pack(pack_data, index_data)
        .map_err(ProtocolError::from)
}

pub fn next_pack_chunk(
    data: &[u8],
    chunk_size: usize,
    chunk_index: usize,
) -> Option<(usize, Vec<u8>, bool)> {
    let (start, len) = crate::chunk_bounds(data.len(), chunk_size.max(1), chunk_index)?;
    let is_final = start + len == data.len();
    Some((start, data[start..start + len].to_vec(), is_final))
}

pub fn receive_pack_chunk(
    state: &mut PackChunkState,
    is_index: bool,
    resume_offset: u64,
    chunk_index: u32,
    is_complete: bool,
    data: &[u8],
    is_final_chunk: bool,
) -> Result<()> {
    let max_bytes = if is_index {
        MAX_RECEIVED_PACK_INDEX_SIZE
    } else {
        MAX_RECEIVED_PACK_SIZE
    };
    receive_pack_chunk_with_limit(
        state,
        is_index,
        resume_offset,
        chunk_index,
        is_complete,
        data,
        is_final_chunk,
        max_bytes,
    )
}

#[allow(clippy::too_many_arguments)]
fn receive_pack_chunk_with_limit(
    state: &mut PackChunkState,
    is_index: bool,
    resume_offset: u64,
    chunk_index: u32,
    is_complete: bool,
    data: &[u8],
    is_final_chunk: bool,
    max_bytes: u64,
) -> Result<()> {
    let (buffer, progress, complete) = if is_index {
        (
            &mut state.index_data,
            &mut state.index_progress,
            &mut state.index_complete,
        )
    } else {
        (
            &mut state.pack_data,
            &mut state.pack_progress,
            &mut state.pack_complete,
        )
    };

    let next_progress = validate_pack_chunk(
        *progress,
        is_index,
        resume_offset,
        chunk_index,
        data,
        max_bytes,
    )?;

    buffer.extend_from_slice(data);
    *progress = next_progress;
    if is_final_chunk || is_complete {
        *complete = true;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receive_pack_chunk_to_spool(
    stream: &mut PackStreamSpool,
    is_index: bool,
    resume_offset: u64,
    chunk_index: u32,
    is_complete: bool,
    data: &[u8],
    is_final_chunk: bool,
    max_bytes: u64,
) -> Result<()> {
    let next_progress = validate_pack_chunk(
        stream.progress,
        is_index,
        resume_offset,
        chunk_index,
        data,
        max_bytes,
    )?;
    stream.write_all(data)?;
    stream.progress = next_progress;
    if is_final_chunk || is_complete {
        stream.complete = true;
    }
    Ok(())
}

fn validate_pack_chunk(
    progress: (u64, u32),
    is_index: bool,
    resume_offset: u64,
    chunk_index: u32,
    data: &[u8],
    max_bytes: u64,
) -> Result<(u64, u32)> {
    if resume_offset != progress.0 {
        return Err(ProtocolError::InvalidState(format!(
            "native pack chunk resume offset mismatch: expected {}, got {}",
            progress.0, resume_offset
        )));
    }
    if chunk_index != progress.1 {
        return Err(ProtocolError::InvalidState(format!(
            "native pack chunk index mismatch: expected {}, got {}",
            progress.1, chunk_index
        )));
    }

    let data_len = u64::try_from(data.len()).map_err(|_| {
        ProtocolError::InvalidState("native pack chunk length does not fit in u64".to_string())
    })?;
    let next_offset = progress.0.checked_add(data_len).ok_or_else(|| {
        ProtocolError::InvalidState("native pack chunk offset overflow".to_string())
    })?;
    if next_offset > max_bytes {
        let stream_name = if is_index { "index" } else { "body" };
        return Err(ProtocolError::InvalidState(format!(
            "native pack {stream_name} exceeds receive size limit: {next_offset} bytes (max {max_bytes})"
        )));
    }
    let next_chunk = progress.1.checked_add(1).ok_or_else(|| {
        ProtocolError::InvalidState("native pack chunk index overflow".to_string())
    })?;

    Ok((next_offset, next_chunk))
}

fn unique_spool_dir(base: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| {
            ProtocolError::InvalidState(format!("system clock before UNIX epoch: {err}"))
        })?
        .as_nanos();
    for attempt in 0..100u32 {
        let dir = base.join(format!("pack-{}-{stamp}-{attempt}", std::process::id()));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(ProtocolError::Io(err)),
        }
    }
    Err(ProtocolError::InvalidState(
        "failed to allocate native pack spool directory".to_string(),
    ))
}

fn to_pack_object_id(id: &ObjectId) -> PackObjectId {
    match id {
        ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
        ObjectId::StateId(state_id) => PackObjectId::StateId(*state_id),
        ObjectId::StateAttachment { id, .. } => PackObjectId::Hash(*id.as_hash()),
    }
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Blob, ContentHash, StateId},
        store::{
            CompressionConfig, FsStore, ObjectStore,
            pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId, PackReader},
        },
    };
    use tempfile::TempDir;

    use super::{
        GitPackChunkState, GrowingPackChunkReader, MAX_RECEIVED_PACK_SIZE,
        NativePackStreamingWriter, ObjectData, ObjectId, ObjectInfo, ObjectType, PackChunkSpool,
        PackChunkState, PackFileChunkReader, build_native_pack, install_received_pack,
        next_pack_chunk, receive_pack_chunk, receive_pack_chunk_with_limit,
        reuse_native_pack_encoded_subset_in,
    };

    fn create_test_store() -> (TempDir, FsStore) {
        let temp = TempDir::new().unwrap();
        let store = FsStore::new(temp.path().join(".heddle"));
        store.init().unwrap();
        (temp, store)
    }

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    #[test]
    fn encoded_snapshot_subset_is_wire_equivalent_without_local_artifacts_or_attachments() {
        let source = TempDir::new().unwrap();
        let spool = TempDir::new().unwrap();
        let source_pack = source.path().join("snapshot.pack");
        let source_index = source.path().join("snapshot.idx");
        let blob = (
            PackObjectId::Hash(hash(1)),
            PackObjectType::Blob,
            b"blob body".to_vec(),
        );
        let tree = (
            PackObjectId::Hash(hash(2)),
            PackObjectType::Tree,
            b"tree body".to_vec(),
        );
        let state_id = StateId::from_bytes([3; 32]);
        let state = (
            PackObjectId::StateId(state_id),
            PackObjectType::State,
            b"state body".to_vec(),
        );
        let attachment_id = PackObjectId::Hash(hash(4));
        let artifact_id = PackObjectId::Hash(hash(5));
        let mut builder = PackBuilder::new(CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        });
        for (id, kind, body) in [
            blob.clone(),
            tree.clone(),
            state.clone(),
            (
                attachment_id,
                PackObjectType::StateAttachment,
                b"local attachment".to_vec(),
            ),
            (
                artifact_id,
                PackObjectType::SnapshotCommit,
                b"local commit artifact".to_vec(),
            ),
        ] {
            builder.add_id(id, kind, body);
        }
        let (pack, index, _) = builder.build().unwrap();
        std::fs::write(&source_pack, pack).unwrap();
        std::fs::write(&source_index, index).unwrap();

        let wanted = vec![
            ObjectInfo {
                id: ObjectId::Hash(hash(1)),
                obj_type: ObjectType::Blob,
                size: blob.2.len() as u64,
                delta_base: None,
            },
            ObjectInfo {
                id: ObjectId::Hash(hash(2)),
                obj_type: ObjectType::Tree,
                size: tree.2.len() as u64,
                delta_base: None,
            },
            ObjectInfo {
                id: ObjectId::StateId(state_id),
                obj_type: ObjectType::State,
                size: state.2.len() as u64,
                delta_base: None,
            },
        ];
        let (bundle, stats) =
            reuse_native_pack_encoded_subset_in(spool.path(), &source_pack, &wanted)
                .unwrap()
                .expect("authoritative non-delta subset must be reusable");

        assert_eq!(stats.object_count, wanted.len());
        assert!(stats.encoded_bytes_copied > 0);
        let reused = PackReader::open(&bundle.pack_path, &bundle.index_path).unwrap();
        let mut reused_ids = reused.list_ids();
        reused_ids.sort();
        let mut wanted_ids = vec![blob.0, tree.0, state.0];
        wanted_ids.sort();
        assert_eq!(reused_ids, wanted_ids);
        assert!(!reused.has_object(&attachment_id));
        assert!(!reused.has_object(&artifact_id));

        for path in [&bundle.pack_path, &bundle.index_path] {
            let expected_wire_bytes = std::fs::read(path).unwrap();
            let mut chunk_reader = PackFileChunkReader::open(path, 7).unwrap();
            let mut wire_bytes = Vec::new();
            while let Some((offset, chunk_index, data, is_final)) =
                chunk_reader.next_chunk().unwrap()
            {
                assert_eq!(offset as usize, wire_bytes.len());
                assert_eq!(chunk_index as usize, wire_bytes.len() / 7);
                wire_bytes.extend_from_slice(&data);
                assert_eq!(is_final, wire_bytes.len() == expected_wire_bytes.len());
            }
            assert_eq!(wire_bytes, expected_wire_bytes);
        }
        for (id, _, expected) in [blob, tree, state] {
            assert_eq!(reused.get_object(&id).unwrap().unwrap().1, expected);
        }
    }

    #[test]
    fn encoded_snapshot_subset_falls_back_for_mismatch_delta_or_attachment_request() {
        let source = TempDir::new().unwrap();
        let spool = TempDir::new().unwrap();
        let source_pack = source.path().join("snapshot.pack");
        let source_index = source.path().join("snapshot.idx");
        let first = b"This is the base content. ".repeat(100);
        let second = b"This is modified content. ".repeat(100);
        let mut builder = PackBuilder::new(CompressionConfig::default());
        builder.add(hash(10), PackObjectType::Blob, first.clone());
        builder.add(hash(11), PackObjectType::Blob, second.clone());
        let (pack, index, stats) = builder.build().unwrap();
        assert!(stats.delta_count > 0, "fixture must contain a delta");
        std::fs::write(&source_pack, pack).unwrap();
        std::fs::write(&source_index, index).unwrap();
        let delta_wants = [ObjectInfo {
            id: ObjectId::Hash(hash(11)),
            obj_type: ObjectType::Blob,
            size: second.len() as u64,
            delta_base: None,
        }];
        assert!(
            reuse_native_pack_encoded_subset_in(spool.path(), &source_pack, &delta_wants)
                .unwrap()
                .is_none()
        );

        let missing_wants = [ObjectInfo {
            id: ObjectId::Hash(hash(12)),
            obj_type: ObjectType::Blob,
            size: 1,
            delta_base: None,
        }];
        assert!(
            reuse_native_pack_encoded_subset_in(spool.path(), &source_pack, &missing_wants)
                .unwrap()
                .is_none()
        );

        let attachment_wants = [ObjectInfo {
            id: ObjectId::StateAttachment {
                state: StateId::from_bytes([13; 32]),
                id: objects::object::StateAttachmentId::from_hash(hash(14)),
                kind: objects::object::StateAttachmentKind::SemanticIndex,
            },
            obj_type: ObjectType::StateAttachment,
            size: 1,
            delta_base: None,
        }];
        assert!(
            reuse_native_pack_encoded_subset_in(spool.path(), &source_pack, &attachment_wants)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn receive_pack_chunk_rejects_cumulative_size_over_limit_before_buffering() {
        let mut state = PackChunkState::default();

        receive_pack_chunk_with_limit(&mut state, false, 0, 0, false, b"abcd", false, 8).unwrap();
        receive_pack_chunk_with_limit(&mut state, false, 4, 1, false, b"efgh", false, 8).unwrap();

        let error = receive_pack_chunk_with_limit(&mut state, false, 8, 2, false, b"i", false, 8)
            .unwrap_err();

        assert_eq!(state.pack_data, b"abcdefgh");
        assert!(
            error
                .to_string()
                .contains("native pack body exceeds receive size limit")
        );
        assert!(error.to_string().contains("9 bytes (max 8)"));
    }

    #[test]
    fn receive_pack_chunk_checks_production_limit_before_extending_buffer() {
        let mut state = PackChunkState {
            pack_progress: (MAX_RECEIVED_PACK_SIZE - 1, 0),
            ..PackChunkState::default()
        };

        let error = receive_pack_chunk(
            &mut state,
            false,
            MAX_RECEIVED_PACK_SIZE - 1,
            0,
            false,
            b"xx",
            false,
        )
        .unwrap_err();

        assert!(state.pack_data.is_empty());
        assert!(
            error
                .to_string()
                .contains("native pack body exceeds receive size limit")
        );
    }

    #[test]
    fn receive_pack_chunk_rejects_resume_offset_mismatch_before_buffering() {
        let mut state = PackChunkState::default();

        let error =
            receive_pack_chunk(&mut state, false, 1, 0, false, b"late chunk", false).unwrap_err();

        assert!(state.pack_data.is_empty());
        assert!(
            error
                .to_string()
                .contains("native pack chunk resume offset mismatch: expected 0, got 1")
        );
    }

    #[test]
    fn receive_pack_chunk_rejects_chunk_index_mismatch_before_buffering() {
        let mut state = PackChunkState::default();

        receive_pack_chunk(&mut state, false, 0, 0, false, b"abc", false).unwrap();
        let error = receive_pack_chunk(&mut state, false, 3, 2, false, b"def", false).unwrap_err();

        assert_eq!(state.pack_data, b"abc");
        assert!(
            error
                .to_string()
                .contains("native pack chunk index mismatch: expected 1, got 2")
        );
    }

    #[test]
    fn git_pack_chunk_state_requires_ordered_chunks_and_final_size() {
        let mut state = GitPackChunkState::default();

        assert!(
            state
                .receive_chunk("git-pack:test", 0, 0, false, 8, b"abcd")
                .unwrap()
                .is_none()
        );
        let error = state
            .receive_chunk("git-pack:test", 4, 2, true, 8, b"efgh")
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Git pack chunk index mismatch: expected 1, got 2")
        );
        assert!(state.ensure_idle().is_err());

        let mut state = GitPackChunkState::default();
        state
            .receive_chunk("git-pack:test", 0, 0, false, 8, b"abcd")
            .unwrap();
        let complete = state
            .receive_chunk("git-pack:test", 4, 1, true, 8, b"efgh")
            .unwrap()
            .unwrap();

        assert_eq!(complete, b"abcdefgh");
        assert!(state.ensure_idle().is_ok());
    }

    #[test]
    fn receive_pack_chunk_accepts_completion_flags_for_pack_and_index() {
        let mut state = PackChunkState::default();

        receive_pack_chunk(&mut state, false, 0, 0, true, b"pack-body", false).unwrap();
        assert!(!state.is_complete());
        receive_pack_chunk(&mut state, true, 0, 0, false, b"pack-index", true).unwrap();

        assert!(state.is_complete());
        assert_eq!(state.pack_data, b"pack-body");
        assert_eq!(state.index_data, b"pack-index");
    }

    #[test]
    fn normal_size_native_pack_receives_and_installs() {
        let (_source_temp, source_store) = create_test_store();
        let (_dest_temp, dest_store) = create_test_store();
        let blob = Blob::from("native pack receive regression");
        let hash = source_store.put_blob(&blob).unwrap();
        let bundle = build_native_pack(
            &source_store,
            &[ObjectInfo {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Blob,
                size: blob.size() as u64,
                delta_base: None,
            }],
        )
        .unwrap();

        let mut state = PackChunkState::default();
        let mut chunk_index = 0usize;
        while let Some((start, data, is_final)) = next_pack_chunk(&bundle.pack_data, 7, chunk_index)
        {
            receive_pack_chunk(
                &mut state,
                false,
                start as u64,
                chunk_index as u32,
                is_final,
                &data,
                is_final,
            )
            .unwrap();
            chunk_index += 1;
        }

        let mut index_chunk = 0usize;
        while let Some((start, data, is_final)) =
            next_pack_chunk(&bundle.index_data, 5, index_chunk)
        {
            receive_pack_chunk(
                &mut state,
                true,
                start as u64,
                index_chunk as u32,
                is_final,
                &data,
                is_final,
            )
            .unwrap();
            index_chunk += 1;
        }

        assert!(state.is_complete());
        assert_eq!(state.pack_data, bundle.pack_data);
        assert_eq!(state.index_data, bundle.index_data);

        let installed_ids =
            install_received_pack(&dest_store, &state.pack_data, &state.index_data).unwrap();

        assert_eq!(installed_ids, vec![PackObjectId::Hash(hash)]);
        let installed_blob = dest_store.get_blob(&hash).unwrap().unwrap();
        assert_eq!(installed_blob.content(), blob.content());
    }

    #[test]
    fn normal_size_native_pack_spools_and_installs() {
        let (_source_temp, source_store) = create_test_store();
        let (dest_temp, dest_store) = create_test_store();
        let blob = Blob::from("native pack spooled receive regression");
        let hash = source_store.put_blob(&blob).unwrap();
        let bundle = build_native_pack(
            &source_store,
            &[ObjectInfo {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Blob,
                size: blob.size() as u64,
                delta_base: None,
            }],
        )
        .unwrap();

        let mut spool = PackChunkSpool::new_in(dest_temp.path()).unwrap();
        let mut chunk_index = 0usize;
        while let Some((start, data, is_final)) = next_pack_chunk(&bundle.pack_data, 7, chunk_index)
        {
            spool
                .receive_chunk(
                    false,
                    start as u64,
                    chunk_index as u32,
                    is_final,
                    &data,
                    is_final,
                )
                .unwrap();
            chunk_index += 1;
        }

        let mut index_chunk = 0usize;
        while let Some((start, data, is_final)) =
            next_pack_chunk(&bundle.index_data, 5, index_chunk)
        {
            spool
                .receive_chunk(
                    true,
                    start as u64,
                    index_chunk as u32,
                    is_final,
                    &data,
                    is_final,
                )
                .unwrap();
            index_chunk += 1;
        }

        assert!(spool.is_complete());
        let installed_ids = spool.install_into(&dest_store).unwrap();

        assert_eq!(installed_ids, vec![PackObjectId::Hash(hash)]);
        let installed_blob = dest_store.get_blob(&hash).unwrap().unwrap();
        assert_eq!(installed_blob.content(), blob.content());
    }

    #[test]
    fn native_pack_streaming_writer_drains_growing_pack_and_installs() {
        let (source_temp, source_store) = create_test_store();
        let (dest_temp, dest_store) = create_test_store();
        let blob = Blob::from("native pack growing stream regression");
        let hash = source_store.put_blob(&blob).unwrap();
        let large_blob = Blob::from_slice(&vec![b'z'; 4096]);
        let large_hash = source_store.put_blob(&large_blob).unwrap();

        let mut writer = NativePackStreamingWriter::new_in(source_temp.path(), 2).unwrap();
        let mut pack_reader = GrowingPackChunkReader::open(writer.pack_path(), 31).unwrap();
        let mut spool = PackChunkSpool::new_in(dest_temp.path()).unwrap();
        let mut saw_interleaved_pack_chunk = false;

        for (id, obj_type, data) in [
            (
                ObjectId::Hash(hash),
                ObjectType::Blob,
                blob.content().to_vec(),
            ),
            (
                ObjectId::Hash(large_hash),
                ObjectType::Blob,
                large_blob.content().to_vec(),
            ),
        ] {
            writer
                .add_object_data(ObjectData {
                    id,
                    obj_type,
                    data,
                    is_delta: false,
                })
                .unwrap();
            writer.flush_pack().unwrap();
            while let Some((offset, chunk_index, data, is_final)) =
                pack_reader.next_available_chunk(false).unwrap()
            {
                assert!(
                    !is_final,
                    "pre-final growing pack drain must not mark chunks final"
                );
                saw_interleaved_pack_chunk = true;
                spool
                    .receive_chunk(false, offset, chunk_index, false, &data, false)
                    .unwrap();
            }
        }

        let bundle = writer.finish().unwrap();
        let mut saw_final_pack_chunk = false;
        while let Some((offset, chunk_index, data, is_final)) =
            pack_reader.next_available_chunk(true).unwrap()
        {
            saw_final_pack_chunk |= is_final;
            spool
                .receive_chunk(false, offset, chunk_index, is_final, &data, is_final)
                .unwrap();
        }

        let mut index_reader = PackFileChunkReader::open(&bundle.index_path, 17).unwrap();
        while let Some((offset, chunk_index, data, is_final)) = index_reader.next_chunk().unwrap() {
            spool
                .receive_chunk(true, offset, chunk_index, is_final, &data, is_final)
                .unwrap();
        }

        assert!(
            saw_interleaved_pack_chunk,
            "expected at least one pack chunk before finalize"
        );
        assert!(
            saw_final_pack_chunk,
            "expected final pack chunk after finish"
        );
        assert!(spool.is_complete());
        let mut installed_ids = spool.install_into(&dest_store).unwrap();
        let mut expected_ids = vec![PackObjectId::Hash(hash), PackObjectId::Hash(large_hash)];
        installed_ids.sort();
        expected_ids.sort();

        assert_eq!(installed_ids, expected_ids);
        let installed_blob = dest_store.get_blob(&hash).unwrap().unwrap();
        assert_eq!(installed_blob.content(), blob.content());
        let installed_large_blob = dest_store.get_blob(&large_hash).unwrap().unwrap();
        assert_eq!(installed_large_blob.content(), large_blob.content());
    }
}
