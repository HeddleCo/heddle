// SPDX-License-Identifier: Apache-2.0
use objects::store::{
    CompressionConfig, ObjectStore,
    pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId},
};

use crate::{ObjectId, ObjectInfo, ObjectType, ProtocolError, Result, load_object_data};

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

#[derive(Debug, Clone)]
pub struct NativePackBundle {
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
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

pub fn native_pack_excluded_object_types() -> &'static [ObjectType] {
    &[ObjectType::Redaction, ObjectType::StateVisibility]
}

pub fn is_native_packable_object_type(obj_type: ObjectType) -> bool {
    !native_pack_excluded_object_types().contains(&obj_type)
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
        builder.add_id(pack_id, to_pack_object_type(object.obj_type)?, object.data);
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

    buffer.extend_from_slice(data);
    *progress = (next_offset, progress.1 + 1);
    if is_final_chunk || is_complete {
        *complete = true;
    }
    Ok(())
}

fn to_pack_object_id(id: &ObjectId) -> PackObjectId {
    match id {
        ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
        ObjectId::ChangeId(change_id) => PackObjectId::ChangeId(*change_id),
    }
}

fn to_pack_object_type(obj_type: ObjectType) -> Result<PackObjectType> {
    match obj_type {
        ObjectType::Blob => Ok(PackObjectType::Blob),
        ObjectType::Tree => Ok(PackObjectType::Tree),
        ObjectType::State => Ok(PackObjectType::State),
        ObjectType::Action => Ok(PackObjectType::Action),
        ObjectType::Redaction => Err(ProtocolError::InvalidState(
            "Redaction sidecar records cannot be packed into the content-addressed object pack"
                .to_string(),
        )),
        ObjectType::StateVisibility => Err(ProtocolError::InvalidState(
            "StateVisibility sidecar records cannot be packed into the content-addressed object pack"
                .to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use objects::{
        object::Blob,
        store::{FsStore, ObjectStore, pack::PackObjectId},
    };
    use tempfile::TempDir;

    use super::{
        MAX_RECEIVED_PACK_SIZE, ObjectId, ObjectInfo, ObjectType, PackChunkState,
        build_native_pack, install_received_pack, next_pack_chunk, receive_pack_chunk,
        receive_pack_chunk_with_limit,
    };

    fn create_test_store() -> (TempDir, FsStore) {
        let temp = TempDir::new().unwrap();
        let store = FsStore::new(temp.path().join(".heddle"));
        store.init().unwrap();
        (temp, store)
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
}
