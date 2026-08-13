// SPDX-License-Identifier: Apache-2.0
//! Bounded zstd encoding and decoding primitives.

#[cfg(feature = "zstd")]
use std::io::{BufReader, Read};

use super::CompressionError;

const MAX_DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;

#[cfg(feature = "zstd")]
pub(super) fn compress(
    data: &[u8],
    level: i32,
    dictionary: Option<&[u8]>,
) -> Result<Vec<u8>, CompressionError> {
    match dictionary {
        None => zstd::encode_all(data, level),
        Some(dictionary) => zstd::bulk::Compressor::with_dictionary(level, dictionary)
            .and_then(|mut compressor| compressor.compress(data)),
    }
    .map_err(|error| CompressionError::CompressionFailed(error.to_string()))
}

#[cfg(not(feature = "zstd"))]
pub(super) fn compress(
    _data: &[u8],
    _level: i32,
    _dictionary: Option<&[u8]>,
) -> Result<Vec<u8>, CompressionError> {
    Err(CompressionError::InvalidOperation(
        "zstd compression support not compiled into this build".to_string(),
    ))
}

#[cfg(feature = "zstd")]
pub(super) fn decompress(
    data: &[u8],
    expected_size: u64,
    dictionary: Option<&[u8]>,
) -> Result<Vec<u8>, CompressionError> {
    validate_size(expected_size)?;
    let expected_capacity = usize::try_from(expected_size).map_err(|_| {
        CompressionError::CorruptedData("zstd expected size exceeds platform limits".to_string())
    })?;
    let mut decoder = zstd::stream::read::Decoder::with_dictionary(
        BufReader::new(data),
        dictionary.unwrap_or_default(),
    )
    .map_err(|error| CompressionError::DecompressionFailed(error.to_string()))?;
    let mut decompressed = Vec::with_capacity(expected_capacity);
    let mut buffer = [0_u8; 8192];

    loop {
        let bytes_read = decoder
            .read(&mut buffer)
            .map_err(|error| CompressionError::DecompressionFailed(error.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        let next_size = decompressed.len().checked_add(bytes_read).ok_or_else(|| {
            CompressionError::CorruptedData("decompressed size overflows".to_string())
        })?;
        let next_size = u64::try_from(next_size).map_err(|_| {
            CompressionError::CorruptedData("decompressed size exceeds platform limits".to_string())
        })?;
        if next_size > expected_size {
            return Err(CompressionError::CorruptedData(format!(
                "decompressed size exceeds recorded header size: expected {expected_size}, got at least {next_size}",
            )));
        }
        decompressed.extend_from_slice(&buffer[..bytes_read]);
    }

    validate_decompressed_len(expected_size, decompressed.len())?;
    Ok(decompressed)
}

#[cfg(not(feature = "zstd"))]
pub(super) fn decompress(
    _data: &[u8],
    expected_size: u64,
    _dictionary: Option<&[u8]>,
) -> Result<Vec<u8>, CompressionError> {
    validate_size(expected_size)?;
    Err(CompressionError::InvalidOperation(
        "zstd-compressed data is unsupported in this build".to_string(),
    ))
}

pub(super) fn validate_size(size: u64) -> Result<(), CompressionError> {
    if size > MAX_DECOMPRESSED_SIZE {
        return Err(CompressionError::SizeLimitExceeded {
            size,
            max: MAX_DECOMPRESSED_SIZE,
        });
    }
    Ok(())
}

#[cfg(feature = "zstd")]
fn validate_decompressed_len(expected: u64, actual: usize) -> Result<(), CompressionError> {
    let actual = u64::try_from(actual).map_err(|_| {
        CompressionError::CorruptedData("decompressed size exceeds platform limits".to_string())
    })?;
    if actual != expected {
        return Err(CompressionError::CorruptedData(format!(
            "decompressed size mismatch: expected {expected}, got {actual}",
        )));
    }
    Ok(())
}
