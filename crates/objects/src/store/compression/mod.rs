// SPDX-License-Identifier: Apache-2.0
//! Compression utilities for Heddle storage.
//!
//! Provides configurable compression with support for:
//! - zstd: High compression ratio, good speed
//! - Delta encoding: For similar versions of the same file

#[cfg(feature = "zstd")]
use std::io::Read;

#[cfg(test)]
use crate::delta::{DeltaDecoder, DeltaEncoder, MAX_DELTA_OUTPUT_SIZE};

const COMPRESSED_HEADER_LEN: usize = 9;
const MAX_DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Compression algorithm selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CompressionType {
    /// No compression (stored as-is).
    None = 0,
    /// Zstandard compression.
    Zstd = 1,
    /// Delta compression (stores diff from base).
    Delta = 2,
}

impl CompressionType {
    /// Convert from byte value.
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(CompressionType::None),
            1 => Some(CompressionType::Zstd),
            2 => Some(CompressionType::Delta),
            _ => None,
        }
    }
}

/// Compression configuration.
#[derive(Debug, Clone, Copy)]
pub struct CompressionConfig {
    /// Whether compression is enabled.
    pub enabled: bool,
    /// Compression level (algorithm-specific).
    /// For zstd: 1-22 (1=fast, 22=best, 3=default)
    pub level: i32,
    /// Minimum size to compress (smaller objects aren't worth it).
    pub min_size: usize,
    /// Maximum size for delta compression base.
    pub max_delta_size: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: cfg!(feature = "zstd"),
            level: 3,                   // zstd default
            min_size: 256,              // Don't compress tiny objects
            max_delta_size: 10_000_000, // 10MB max for delta base
        }
    }
}

impl CompressionConfig {
    /// Create configuration from environment variables.
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("HEDDLE_COMPRESSION") {
            let requested = val != "0" && val.to_lowercase() != "false";
            config.enabled = requested && cfg!(feature = "zstd");
        }

        if let Ok(val) = std::env::var("HEDDLE_COMPRESSION_LEVEL")
            && let Ok(level) = val.parse::<i32>()
        {
            config.level = level.clamp(1, 22);
        }

        if let Ok(val) = std::env::var("HEDDLE_COMPRESSION_MIN_SIZE")
            && let Ok(size) = val.parse::<usize>()
        {
            config.min_size = size;
        }

        config
    }

    /// Disable compression.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            level: 0,
            min_size: usize::MAX,
            max_delta_size: 0,
        }
    }
}

/// Compression error type.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    #[error("decompression failed: {0}")]
    DecompressionFailed(String),
    #[error("compression failed: {0}")]
    CompressionFailed(String),
    #[error("invalid compression type: {0}")]
    InvalidType(u8),
    #[error("corrupted data: {0}")]
    CorruptedData(String),
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
    #[error("object size {size} exceeds maximum {max}")]
    SizeLimitExceeded { size: u64, max: u64 },
}

#[cfg(feature = "zstd")]
/// Compress data using zstd.
pub fn compress_zstd(data: &[u8], level: i32) -> Result<Vec<u8>, CompressionError> {
    zstd::encode_all(data, level).map_err(|e| CompressionError::CompressionFailed(e.to_string()))
}

#[cfg(not(feature = "zstd"))]
pub fn compress_zstd(_data: &[u8], _level: i32) -> Result<Vec<u8>, CompressionError> {
    Err(CompressionError::InvalidOperation(
        "zstd compression support not compiled into this build".to_string(),
    ))
}

#[cfg(feature = "zstd")]
/// Decompress zstd data while enforcing the recorded output size.
pub fn decompress_zstd(data: &[u8], expected_size: u64) -> Result<Vec<u8>, CompressionError> {
    validate_size(expected_size)?;

    let mut decoder = zstd::stream::read::Decoder::new(data)
        .map_err(|e| CompressionError::DecompressionFailed(e.to_string()))?;
    let mut decompressed = Vec::with_capacity(expected_size as usize);
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = decoder
            .read(&mut buffer)
            .map_err(|e| CompressionError::DecompressionFailed(e.to_string()))?;
        if bytes_read == 0 {
            break;
        }

        let next_size = (decompressed.len() + bytes_read) as u64;
        if next_size > expected_size {
            return Err(CompressionError::CorruptedData(format!(
                "decompressed size exceeds recorded header size: expected {expected_size}, got at least {next_size}",
            )));
        }

        decompressed.extend_from_slice(&buffer[..bytes_read]);
    }

    Ok(decompressed)
}

#[cfg(not(feature = "zstd"))]
pub fn decompress_zstd(_data: &[u8], expected_size: u64) -> Result<Vec<u8>, CompressionError> {
    validate_size(expected_size)?;
    Err(CompressionError::InvalidOperation(
        "zstd-compressed data is unsupported in this build".to_string(),
    ))
}

/// Compress data with automatic algorithm selection.
///
/// Returns the compressed data with header, or None if compression
/// doesn't help (compressed would be larger).
pub fn compress(
    data: &[u8],
    config: &CompressionConfig,
) -> Result<Option<Vec<u8>>, CompressionError> {
    if !config.enabled || data.len() < config.min_size {
        return Ok(None);
    }

    validate_size(data.len() as u64)?;

    // Try zstd compression
    let compressed = compress_zstd(data, config.level)?;

    // Only use compression if it actually helps
    if compressed.len() >= data.len() {
        return Ok(None);
    }

    // Build header: [type][size][data]
    let mut result = Vec::with_capacity(COMPRESSED_HEADER_LEN + compressed.len());
    result.push(CompressionType::Zstd as u8);
    result.extend_from_slice(&(data.len() as u64).to_be_bytes());
    result.extend_from_slice(&compressed);

    Ok(Some(result))
}

#[cfg(test)]
/// Compress using delta encoding against a base.
///
/// Returns the delta-compressed data with header, or None if delta
/// doesn't help.
fn compress_delta(
    data: &[u8],
    base: &[u8],
    config: &CompressionConfig,
) -> Result<Option<Vec<u8>>, CompressionError> {
    if !config.enabled || data.len() < config.min_size || base.len() > config.max_delta_size {
        return Ok(None);
    }

    validate_size(data.len() as u64)?;

    let delta = DeltaEncoder::encode(base, data);

    // Only use delta if it helps
    if delta.len() >= data.len() {
        return Ok(None);
    }

    // Build header: [type][size][data]
    let mut result = Vec::with_capacity(COMPRESSED_HEADER_LEN + delta.len());
    result.push(CompressionType::Delta as u8);
    result.extend_from_slice(&(data.len() as u64).to_be_bytes());
    result.extend_from_slice(&delta);

    Ok(Some(result))
}

/// Decompress data based on header.
///
/// Returns the decompressed data, or original data if uncompressed.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    if data.len() < COMPRESSED_HEADER_LEN {
        // Too short for header, assume uncompressed
        return Ok(data.to_vec());
    }

    let compression_type =
        CompressionType::from_u8(data[0]).ok_or_else(|| CompressionError::InvalidType(data[0]))?;

    match compression_type {
        CompressionType::None => {
            let expected_size = read_u64_size(data)?;
            let payload = data[COMPRESSED_HEADER_LEN..].to_vec();
            validate_decompressed_len(expected_size, payload.len())?;
            Ok(payload)
        }
        CompressionType::Zstd if zstd_header_len(data).is_some() => {
            decompress_zstd_with_header(data)
        }
        CompressionType::Zstd => Ok(data.to_vec()),
        CompressionType::Delta => {
            // Delta requires base - this is handled separately
            Err(CompressionError::InvalidOperation(
                "Delta compression requires base object".to_string(),
            ))
        }
    }
}

#[cfg(test)]
/// Decompress delta-encoded data.
fn decompress_delta(delta_data: &[u8], base: &[u8]) -> Result<Vec<u8>, CompressionError> {
    if delta_data.len() < COMPRESSED_HEADER_LEN {
        return Err(CompressionError::CorruptedData(
            "Delta data too short".to_string(),
        ));
    }

    let compression_type = CompressionType::from_u8(delta_data[0])
        .ok_or_else(|| CompressionError::InvalidType(delta_data[0]))?;

    if compression_type != CompressionType::Delta {
        return Err(CompressionError::InvalidOperation(
            "Expected delta compression".to_string(),
        ));
    }

    decompress_delta_with_header(delta_data, base)
}

/// Check if data is compressed (has compression header).
pub fn is_compressed(data: &[u8]) -> bool {
    if data.len() < COMPRESSED_HEADER_LEN {
        return false;
    }

    matches!(
        CompressionType::from_u8(data[0]),
        Some(CompressionType::Zstd)
    ) && zstd_header_len(data).is_some()
}

/// Peek at the recorded *uncompressed* size in a header-prefixed blob,
/// without decompressing the payload. Returns `None` for short or
/// unprefixed inputs (the caller can then fall back to the file length).
///
/// Used by header-only size queries (e.g. [`ObjectStore::blob_size`])
/// where reading the full blob would dominate. Only the first 9 bytes
/// of the input are consulted.
pub fn header_uncompressed_size(data: &[u8]) -> Option<u64> {
    if data.len() < COMPRESSED_HEADER_LEN {
        return None;
    }
    match CompressionType::from_u8(data[0])? {
        CompressionType::Zstd => {
            zstd_header_len(data)?;
            Some(u64::from_be_bytes(
                data[1..COMPRESSED_HEADER_LEN].try_into().ok()?,
            ))
        }
        CompressionType::None | CompressionType::Delta => None,
    }
}

#[cfg(test)]
/// Get compression info from header.
fn compression_info(data: &[u8]) -> Option<(CompressionType, u64)> {
    if data.len() < COMPRESSED_HEADER_LEN {
        return None;
    }

    let compression_type = CompressionType::from_u8(data[0])?;
    let uncompressed_size = u64::from_be_bytes(data[1..COMPRESSED_HEADER_LEN].try_into().ok()?);

    Some((compression_type, uncompressed_size))
}

fn decompress_zstd_with_header(data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    try_decompress_zstd(data, COMPRESSED_HEADER_LEN, read_u64_size)
}

fn zstd_header_len(data: &[u8]) -> Option<usize> {
    if has_magic_at(data, COMPRESSED_HEADER_LEN, ZSTD_MAGIC) {
        Some(COMPRESSED_HEADER_LEN)
    } else {
        None
    }
}

fn try_decompress_zstd<F>(
    data: &[u8],
    header_len: usize,
    read_size: F,
) -> Result<Vec<u8>, CompressionError>
where
    F: Fn(&[u8]) -> Result<u64, CompressionError>,
{
    let uncompressed_size = read_size(data)?;
    let decompressed = decompress_zstd(&data[header_len..], uncompressed_size)?;
    validate_decompressed_len(uncompressed_size, decompressed.len())?;
    Ok(decompressed)
}

#[cfg(test)]
fn decompress_delta_with_header(
    delta_data: &[u8],
    base: &[u8],
) -> Result<Vec<u8>, CompressionError> {
    try_decompress_delta(delta_data, base, COMPRESSED_HEADER_LEN, read_u64_size)
}

#[cfg(test)]
fn try_decompress_delta<F>(
    delta_data: &[u8],
    base: &[u8],
    header_len: usize,
    read_size: F,
) -> Result<Vec<u8>, CompressionError>
where
    F: Fn(&[u8]) -> Result<u64, CompressionError>,
{
    let uncompressed_size = read_size(delta_data)?;

    if uncompressed_size > MAX_DELTA_OUTPUT_SIZE as u64 {
        return Err(CompressionError::DecompressionFailed(format!(
            "delta output size {} exceeds max {}",
            uncompressed_size, MAX_DELTA_OUTPUT_SIZE
        )));
    }

    let delta = &delta_data[header_len..];
    let decompressed = DeltaDecoder::decode(base, delta, uncompressed_size as usize)
        .map_err(|error| CompressionError::DecompressionFailed(error.to_string()))?;
    validate_decompressed_len(uncompressed_size, decompressed.len())?;
    Ok(decompressed)
}

fn read_u64_size(data: &[u8]) -> Result<u64, CompressionError> {
    if data.len() < COMPRESSED_HEADER_LEN {
        return Err(CompressionError::CorruptedData(
            "compression header truncated".to_string(),
        ));
    }

    let recorded_size =
        u64::from_be_bytes(data[1..COMPRESSED_HEADER_LEN].try_into().map_err(|_| {
            CompressionError::CorruptedData("compression header truncated".to_string())
        })?);
    validate_size(recorded_size)?;
    Ok(recorded_size)
}

fn validate_size(size: u64) -> Result<(), CompressionError> {
    if size > MAX_DECOMPRESSED_SIZE {
        return Err(CompressionError::SizeLimitExceeded {
            size,
            max: MAX_DECOMPRESSED_SIZE,
        });
    }

    Ok(())
}

fn validate_decompressed_len(expected: u64, actual: usize) -> Result<(), CompressionError> {
    if actual as u64 != expected {
        return Err(CompressionError::CorruptedData(format!(
            "decompressed size mismatch: expected {expected}, got {actual}",
        )));
    }

    Ok(())
}

fn has_magic_at(data: &[u8], offset: usize, magic: [u8; 4]) -> bool {
    data.get(offset..offset + magic.len()) == Some(magic.as_slice())
}

#[cfg(test)]
mod compression_tests;
