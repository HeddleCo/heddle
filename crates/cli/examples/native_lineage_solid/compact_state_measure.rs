// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path};

use anyhow::{Result, bail};
use objects::{
    object::{State, StateId},
    store::FsStore,
};
use serde::Serialize;

use crate::{
    compact_measure::{CommonMeasurement, RoundTripStats, report_progress, verify_frames},
    compact_state::{StateBreakdown, encode_state_frame},
    compact_state_decode::decode_state_frame,
    measure::compress_frame,
    model::ObjectRef,
};

struct StateRecord {
    id: StateId,
    state: State,
    source: Vec<u8>,
}

#[derive(Serialize)]
pub struct StateMeasurement {
    #[serde(flatten)]
    pub common: CommonMeasurement,
    pub breakdown: StateBreakdown,
}

pub fn measure_states(
    store: &FsStore,
    order: &[StateId],
    output_dir: &Path,
    frame_limit: usize,
) -> Result<StateMeasurement> {
    let frames_dir = output_dir.join("compact-state.frames");
    fs::create_dir(&frames_dir)?;
    let mut common = CommonMeasurement::default();
    let mut breakdown = StateBreakdown::default();
    let mut batch = Vec::new();
    let mut batch_source_bytes = 0usize;
    for (index, id) in order.iter().enumerate() {
        let source = ObjectRef::State(*id).load(store)?;
        let mut state: State = rmp_serde::from_slice(&source)?;
        state.state_id = *id;
        common.source_msgpack_bytes += source.len() as u64;
        common.source_positional_msgpack_bytes += rmp_serde::to_vec(&state)?.len() as u64;
        batch_source_bytes += source.len();
        batch.push(StateRecord {
            id: *id,
            state,
            source,
        });
        if batch_source_bytes >= frame_limit {
            write_batch(
                &batch,
                &frames_dir,
                frame_limit,
                &mut common,
                &mut breakdown,
            )?;
            batch.clear();
            batch_source_bytes = 0;
        }
        report_progress("compact states", index + 1, order.len());
    }
    write_batch(
        &batch,
        &frames_dir,
        frame_limit,
        &mut common,
        &mut breakdown,
    )?;
    common.frame_integrity_checks = verify_frames(&frames_dir)?;
    Ok(StateMeasurement { common, breakdown })
}

fn write_batch(
    records: &[StateRecord],
    frames_dir: &Path,
    frame_limit: usize,
    common: &mut CommonMeasurement,
    breakdown: &mut StateBreakdown,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let states = records
        .iter()
        .map(|record| record.state.clone())
        .collect::<Vec<_>>();
    let (compact, frame_breakdown) = encode_state_frame(&states)?;
    if compact.len() > frame_limit && records.len() > 1 {
        let middle = records.len() / 2;
        write_batch(
            &records[..middle],
            frames_dir,
            frame_limit,
            common,
            breakdown,
        )?;
        write_batch(
            &records[middle..],
            frames_dir,
            frame_limit,
            common,
            breakdown,
        )?;
        return Ok(());
    }
    verify_states(records, &compact, &mut common.roundtrip)?;
    common.compact_raw_bytes += compact.len() as u64;
    common.compressed_bytes += compress_frame(frames_dir, common.frames, &compact)?;
    common.frames += 1;
    common.objects += records.len();
    breakdown.add(&frame_breakdown);
    Ok(())
}

fn verify_states(
    records: &[StateRecord],
    compact: &[u8],
    stats: &mut RoundTripStats,
) -> Result<()> {
    let decoded = decode_state_frame(compact)?;
    if decoded.len() != records.len() {
        bail!(
            "compact state frame decoded {} objects, expected {}",
            decoded.len(),
            records.len()
        );
    }
    for (record, decoded) in records.iter().zip(decoded) {
        let id = record.id.to_string_full();
        if decoded != record.state {
            stats.value_mismatches += 1;
            bail!("compact state value mismatch for {id}");
        }
        if decoded.id() != record.id {
            stats.typed_hash_mismatches += 1;
            bail!("compact state typed-hash mismatch for {id}");
        }
        if rmp_serde::to_vec_named(&decoded)? != record.source {
            stats.native_payload_mismatches += 1;
            bail!("compact state native-payload mismatch for {id}");
        }
        stats.checked(id);
    }
    Ok(())
}
