// SPDX-License-Identifier: Apache-2.0

use super::*;

pub(super) fn add_state_frames(
    builder: &mut StreamingPackBuilder<File>,
    states: &HashMap<StateId, State>,
    order: &[StateId],
    context: &RepackContext,
    corrupt_first: &mut bool,
) -> Result<u64, BuildError> {
    let mut batch = Vec::new();
    let mut source_bytes = 0usize;
    let mut logical_bytes = 0u64;
    for id in order {
        let state = states[id].clone();
        let bytes = rmp_serde::to_vec_named(&state).map_err(HeddleError::from)?;
        source_bytes = source_bytes.saturating_add(bytes.len());
        logical_bytes = logical_bytes.saturating_add(bytes.len() as u64);
        batch.push((*id, state));
        context
            .checkpoint(bytes.len() as u64)
            .map_err(BuildError::Cancelled)?;
        if source_bytes >= FRAME_LIMIT {
            write_state_batch(builder, &batch, corrupt_first)?;
            batch.clear();
            source_bytes = 0;
        }
    }
    write_state_batch(builder, &batch, corrupt_first)?;
    Ok(logical_bytes)
}

fn write_state_batch(
    builder: &mut StreamingPackBuilder<File>,
    records: &[(StateId, State)],
    corrupt_first: &mut bool,
) -> Result<(), BuildError> {
    if records.is_empty() {
        return Ok(());
    }
    let states = records
        .iter()
        .map(|(_, state)| state.clone())
        .collect::<Vec<_>>();
    let mut frame = encode_state_frame(&states).map_err(compact_error)?;
    if frame.len() > FRAME_LIMIT && records.len() > 1 {
        let middle = records.len() / 2;
        write_state_batch(builder, &records[..middle], corrupt_first)?;
        return write_state_batch(builder, &records[middle..], corrupt_first);
    }
    verify_state_frame(records, &frame)?;
    corrupt_if_requested(&mut frame, corrupt_first);
    let stored = compress_compact_frame(&frame)?;
    let ids = records
        .iter()
        .map(|(id, _)| PackObjectId::StateId(*id))
        .collect::<Vec<_>>();
    builder.add_shared_frame(&ids, ObjectType::State, frame.len(), &stored)?;
    Ok(())
}

fn verify_state_frame(records: &[(StateId, State)], frame: &[u8]) -> Result<(), BuildError> {
    let decoded = decode_state_frame(frame).map_err(compact_error)?;
    if decoded.len() != records.len() {
        return Err(
            HeddleError::InvalidObject("compact state frame changed object count".into()).into(),
        );
    }
    for ((id, expected), actual) in records.iter().zip(decoded) {
        if actual.id() != *id {
            return Err(HeddleError::InvalidObject(
                "compact state frame changed its typed id".into(),
            )
            .into());
        }
        let actual_bytes = rmp_serde::to_vec_named(&actual).map_err(HeddleError::from)?;
        let expected_bytes = rmp_serde::to_vec_named(expected).map_err(HeddleError::from)?;
        if actual_bytes != expected_bytes {
            return Err(HeddleError::InvalidObject(
                "compact state frame changed native bytes".into(),
            )
            .into());
        }
    }
    Ok(())
}
