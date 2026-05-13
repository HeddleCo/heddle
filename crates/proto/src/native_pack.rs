// SPDX-License-Identifier: Apache-2.0
use objects::store::{
    CompressionConfig, ObjectStore,
    pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId},
};

use crate::{ObjectId, ObjectInfo, ObjectType, ProtocolError, Result, load_object_data};

#[derive(Debug, Clone)]
pub struct NativePackBundle {
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pub ids: Vec<PackObjectId>,
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

pub fn build_native_pack(
    store: &dyn ObjectStore,
    objects: &[ObjectInfo],
) -> Result<NativePackBundle> {
    let mut builder = PackBuilder::new(sync_pack_compression());
    let mut ids = Vec::with_capacity(objects.len());

    for info in objects {
        let object = load_object_data(store, &info.id, info.obj_type)?;
        let pack_id = to_pack_object_id(&object.id);
        ids.push(pack_id);
        builder.add_id(pack_id, to_pack_object_type(object.obj_type), object.data);
    }

    let (pack_data, index_data, _) = builder.build()?;
    Ok(NativePackBundle {
        pack_data,
        index_data,
        ids,
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
    store: &dyn ObjectStore,
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

    buffer.extend_from_slice(data);
    *progress = (progress.0 + data.len() as u64, progress.1 + 1);
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

fn to_pack_object_type(obj_type: ObjectType) -> PackObjectType {
    match obj_type {
        ObjectType::Blob => PackObjectType::Blob,
        ObjectType::Tree => PackObjectType::Tree,
        ObjectType::State => PackObjectType::State,
        ObjectType::Action => PackObjectType::Action,
    }
}