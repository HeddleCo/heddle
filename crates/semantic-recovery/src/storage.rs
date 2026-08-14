// SPDX-License-Identifier: Apache-2.0
//! Checksummed, atomic sidecar persistence and compact code packing.

use std::{fs, path::Path};

use heddle_fs_prims::fs_atomic::write_file_atomic;

use crate::{INDEX_FORMAT_VERSION, RecoveryError, Result, quantizer::VectorCode};

const MAGIC: &[u8; 8] = b"HDSREC01";
const HEADER_BYTES: usize = MAGIC.len() + 4 + 8;
const CHECKSUM_BYTES: usize = 32;
const MAX_SIDECAR_BYTES: usize = 1 << 30;

pub(crate) fn write_sidecar<T: serde::Serialize>(path: &Path, value: &T) -> Result<u64> {
    let body =
        rmp_serde::to_vec_named(value).map_err(|error| RecoveryError::Codec(error.to_string()))?;
    let body_len = u64::try_from(body.len())
        .map_err(|_| RecoveryError::InvalidInput("sidecar is too large".to_string()))?;
    let mut bytes = Vec::with_capacity(HEADER_BYTES + body.len() + CHECKSUM_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&INDEX_FORMAT_VERSION.to_be_bytes());
    bytes.extend_from_slice(&body_len.to_be_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(blake3::hash(&bytes).as_bytes());
    write_file_atomic(path, &bytes)?;
    Ok(bytes.len() as u64)
}

pub(crate) fn read_sidecar<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_SIDECAR_BYTES as u64 {
        return Err(RecoveryError::InvalidSidecar(format!(
            "sidecar exceeds {MAX_SIDECAR_BYTES} bytes"
        )));
    }
    let bytes = fs::read(path)?;
    if bytes.len() < HEADER_BYTES + CHECKSUM_BYTES || &bytes[..MAGIC.len()] != MAGIC {
        return Err(RecoveryError::InvalidSidecar(
            "missing recovery sidecar header".to_string(),
        ));
    }
    let version = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed header"));
    if version != INDEX_FORMAT_VERSION {
        return Err(RecoveryError::InvalidSidecar(format!(
            "format {version} is unsupported; expected {INDEX_FORMAT_VERSION}"
        )));
    }
    let body_len = u64::from_be_bytes(bytes[12..20].try_into().expect("fixed header")) as usize;
    let expected_len = HEADER_BYTES
        .checked_add(body_len)
        .and_then(|length| length.checked_add(CHECKSUM_BYTES))
        .ok_or_else(|| RecoveryError::InvalidSidecar("sidecar length overflow".to_string()))?;
    if bytes.len() != expected_len {
        return Err(RecoveryError::InvalidSidecar(
            "sidecar length does not match its header".to_string(),
        ));
    }
    let checksum_at = bytes.len() - CHECKSUM_BYTES;
    if blake3::hash(&bytes[..checksum_at]).as_bytes() != &bytes[checksum_at..] {
        return Err(RecoveryError::InvalidSidecar(
            "sidecar checksum mismatch".to_string(),
        ));
    }
    rmp_serde::from_slice(&bytes[HEADER_BYTES..checksum_at])
        .map_err(|error| RecoveryError::Codec(error.to_string()))
}

pub(crate) fn pack_codes(
    codes: &[VectorCode],
    coarse_bits: usize,
    residual_bits: usize,
) -> Vec<u8> {
    let total_bits = codes.len() * (coarse_bits + residual_bits);
    let mut output = vec![0_u8; total_bits.div_ceil(8)];
    let mut offset = 0;
    for code in codes {
        write_bits(&mut output, &mut offset, code.coarse, coarse_bits);
        write_bits(&mut output, &mut offset, code.residual, residual_bits);
    }
    output
}

pub(crate) fn unpack_codes(
    bytes: &[u8],
    count: usize,
    coarse_bits: usize,
    residual_bits: usize,
) -> Result<Vec<VectorCode>> {
    let expected = count
        .checked_mul(coarse_bits + residual_bits)
        .map(|bits| bits.div_ceil(8))
        .ok_or_else(|| RecoveryError::InvalidSidecar("code length overflow".to_string()))?;
    if bytes.len() != expected {
        return Err(RecoveryError::InvalidSidecar(
            "packed code length does not match the entry count".to_string(),
        ));
    }
    let mut offset = 0;
    Ok((0..count)
        .map(|_| VectorCode {
            coarse: read_bits(bytes, &mut offset, coarse_bits),
            residual: read_bits(bytes, &mut offset, residual_bits),
        })
        .collect())
}

fn write_bits(output: &mut [u8], offset: &mut usize, value: usize, width: usize) {
    for shift in 0..width {
        if value & (1 << shift) != 0 {
            output[*offset / 8] |= 1 << (*offset % 8);
        }
        *offset += 1;
    }
}

fn read_bits(input: &[u8], offset: &mut usize, width: usize) -> usize {
    let mut value = 0;
    for shift in 0..width {
        if input[*offset / 8] & (1 << (*offset % 8)) != 0 {
            value |= 1 << shift;
        }
        *offset += 1;
    }
    value
}
