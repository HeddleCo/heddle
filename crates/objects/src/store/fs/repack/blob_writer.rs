// SPDX-License-Identifier: Apache-2.0

use std::fs::File;

use heddle_object_model::compact::{decode_blob_frame, encode_blob_frame};

use super::{compact::FRAME_LIMIT, staging::BuildError};
use crate::{
    object::ContentHash,
    store::{
        HeddleError, ObjectStore,
        fs::FsStore,
        pack::{
            ObjectType, PackObjectId, RepackContext, StreamingPackBuilder, compress_compact_frame,
        },
    },
};

const FRAME_FIXED_BYTES: usize = 4 + 32;

pub(super) fn add_blob_frames(
    store: &FsStore,
    builder: &mut StreamingPackBuilder<File>,
    order: &[ContentHash],
    context: &RepackContext,
    corrupt_first: &mut bool,
) -> Result<u64, BuildError> {
    let mut ids = Vec::new();
    let mut bodies = Vec::new();
    let mut body_bytes = 0usize;
    let mut length_bytes = 0usize;
    let mut previous_len = None;
    let mut logical_bytes = 0u64;
    for hash in order {
        let body = ObjectStore::get_blob(store, hash)?
            .ok_or_else(|| HeddleError::InvalidObject(format!("repack blob disappeared: {hash}")))?
            .into_content();
        let body_len = body.len();
        let encoded_len = encoded_length_bytes(previous_len, body_len)?;
        let proposed = FRAME_FIXED_BYTES
            .saturating_add(unsigned_varint_len(ids.len() + 1))
            .saturating_add(length_bytes)
            .saturating_add(encoded_len)
            .saturating_add(body_bytes)
            .saturating_add(body_len);
        if !bodies.is_empty() && proposed > FRAME_LIMIT {
            write_blob_frame(builder, &ids, &bodies, corrupt_first)?;
            ids.clear();
            bodies.clear();
            body_bytes = 0;
            length_bytes = 0;
            previous_len = None;
        }
        let encoded_len = encoded_length_bytes(previous_len, body_len)?;
        logical_bytes = logical_bytes.saturating_add(body.len() as u64);
        body_bytes = body_bytes.saturating_add(body_len);
        length_bytes = length_bytes.saturating_add(encoded_len);
        previous_len = Some(body_len);
        ids.push(PackObjectId::Hash(*hash));
        bodies.push(body);
        context
            .checkpoint(body_len as u64)
            .map_err(BuildError::Cancelled)?;
    }
    if !bodies.is_empty() {
        write_blob_frame(builder, &ids, &bodies, corrupt_first)?;
    }
    Ok(logical_bytes)
}

fn encoded_length_bytes(previous: Option<usize>, current: usize) -> Result<usize, BuildError> {
    let current = i64::try_from(current)
        .map_err(|_| HeddleError::InvalidObject("blob length exceeds signed delta range".into()))?;
    let value = match previous {
        Some(previous) => {
            let previous = i64::try_from(previous).map_err(|_| {
                HeddleError::InvalidObject("blob length exceeds signed delta range".into())
            })?;
            ((current - previous) << 1) ^ ((current - previous) >> 63)
        }
        None => current,
    } as u64;
    Ok(unsigned_u64_varint_len(value))
}

fn unsigned_varint_len(value: usize) -> usize {
    unsigned_u64_varint_len(value as u64)
}

fn unsigned_u64_varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn write_blob_frame(
    builder: &mut StreamingPackBuilder<File>,
    ids: &[PackObjectId],
    bodies: &[Vec<u8>],
    corrupt_first: &mut bool,
) -> Result<(), BuildError> {
    if let ([PackObjectId::Hash(hash)], [body]) = (ids, bodies) {
        let mut body = body.clone();
        if *corrupt_first {
            body.push(0xff);
            *corrupt_first = false;
        }
        builder.add(*hash, ObjectType::Blob, body)?;
        return Ok(());
    }
    let slices = bodies.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let mut frame = encode_blob_frame(&slices)
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    verify_blob_frame(ids, &frame)?;
    if *corrupt_first {
        let index = frame.len() / 2;
        frame[index] ^= 0x01;
        *corrupt_first = false;
    }
    let stored = compress_compact_frame(&frame)?;
    builder.add_shared_frame(ids, ObjectType::Blob, frame.len(), &stored)?;
    Ok(())
}

fn verify_blob_frame(ids: &[PackObjectId], frame: &[u8]) -> Result<(), BuildError> {
    let decoded =
        decode_blob_frame(frame).map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    if decoded.len() != ids.len() {
        return Err(
            HeddleError::InvalidObject("compact blob frame changed object count".into()).into(),
        );
    }
    for (id, (hash, _)) in ids.iter().zip(decoded) {
        if *id != PackObjectId::Hash(hash) {
            return Err(
                HeddleError::InvalidObject("compact blob frame changed a typed id".into()).into(),
            );
        }
    }
    Ok(())
}
