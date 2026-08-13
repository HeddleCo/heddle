// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Compression utilities for Heddle storage.
//!
//! Provides configurable compression with support for:
//! - zstd: High compression ratio, good speed
//! - Delta encoding: For similar versions of the same file

mod dictionaries;
mod frame;
mod zstd_codec;

pub use dictionaries::CompressionDictionary;
use dictionaries::PLAIN_ZSTD_DICTIONARY_ID;
#[cfg(all(test, feature = "zstd"))]
use frame::ZSTD_MAGIC;
use frame::{
    DICTIONARY_HEADER_LEN as DICTIONARY_COMPRESSED_HEADER_LEN, HEADER_LEN as COMPRESSED_HEADER_LEN,
};

/// Compression algorithm selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CompressionType {
    /// Zstandard compression.
    Zstd = 1,
}

impl CompressionType {
    /// Convert from byte value.
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(CompressionType::Zstd),
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
    #[error("unknown compression dictionary id: {0}")]
    UnknownDictionary(u32),
    #[error("object size {size} exceeds maximum {max}")]
    SizeLimitExceeded { size: u64, max: u64 },
}

#[cfg(feature = "bench")]
/// Compress data using zstd.
pub fn compress_zstd(data: &[u8], level: i32) -> Result<Vec<u8>, CompressionError> {
    zstd_codec::compress(data, level, None)
}

#[cfg(feature = "bench")]
/// Decompress zstd data while enforcing the recorded output size.
pub fn decompress_zstd(data: &[u8], expected_size: u64) -> Result<Vec<u8>, CompressionError> {
    zstd_codec::decompress(data, expected_size, None)
}

/// Compress data with automatic algorithm selection.
///
/// Returns the compressed data with header, or None if compression
/// doesn't help (compressed would be larger).
pub fn compress(
    data: &[u8],
    config: &CompressionConfig,
) -> Result<Option<Vec<u8>>, CompressionError> {
    compress_impl(data, config, None)
}

/// Compress data with a durable, versioned dictionary.
///
/// The dictionary ID is embedded in the compression wrapper, so [`decompress`]
/// can select the exact bundled dictionary without object-kind context.
pub fn compress_with_dictionary(
    data: &[u8],
    config: &CompressionConfig,
    dictionary: CompressionDictionary,
) -> Result<Option<Vec<u8>>, CompressionError> {
    compress_impl(data, config, Some(dictionary))
}

fn compress_impl(
    data: &[u8],
    config: &CompressionConfig,
    dictionary: Option<CompressionDictionary>,
) -> Result<Option<Vec<u8>>, CompressionError> {
    if !config.enabled || data.len() < config.min_size {
        return Ok(None);
    }

    zstd_codec::validate_size(data.len() as u64)?;

    let compressed = zstd_codec::compress(
        data,
        config.level,
        dictionary.map(CompressionDictionary::bytes),
    )?;

    // Only use compression if it actually helps
    if compressed.len() >= data.len() {
        return Ok(None);
    }

    // Legacy/plain: [type][size][zstd frame]
    // Dictionary:   [type][size][dictionary id][zstd frame]
    let header_len = if dictionary.is_some() {
        DICTIONARY_COMPRESSED_HEADER_LEN
    } else {
        COMPRESSED_HEADER_LEN
    };
    let mut result = Vec::with_capacity(header_len + compressed.len());
    result.push(CompressionType::Zstd as u8);
    result.extend_from_slice(&(data.len() as u64).to_be_bytes());
    if let Some(dictionary) = dictionary {
        result.extend_from_slice(&dictionary.id().to_be_bytes());
    }
    result.extend_from_slice(&compressed);

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
        CompressionType::Zstd if frame::parse_zstd(data).is_some() => {
            decompress_zstd_with_header(data)
        }
        CompressionType::Zstd => Ok(data.to_vec()),
    }
}

/// Check if data is compressed (has compression header).
pub fn is_compressed(data: &[u8]) -> bool {
    if data.len() < COMPRESSED_HEADER_LEN {
        return false;
    }

    matches!(
        CompressionType::from_u8(data[0]),
        Some(CompressionType::Zstd)
    ) && frame::parse_zstd(data).is_some()
}

/// Peek at the recorded *uncompressed* size in a header-prefixed blob,
/// without decompressing the payload. Returns `None` for short or
/// unprefixed inputs (the caller can then fall back to the file length).
///
/// Used by header-only size queries (e.g. [`ObjectStore::blob_size`])
/// where reading the full blob would dominate. Only the first 9 bytes
/// plus enough bytes to identify the following zstd frame are consulted
/// (13 bytes for plain zstd, 17 for dictionary zstd).
pub fn header_uncompressed_size(data: &[u8]) -> Option<u64> {
    if data.len() < COMPRESSED_HEADER_LEN {
        return None;
    }
    let CompressionType::Zstd = CompressionType::from_u8(data[0])?;
    Some(frame::parse_zstd(data)?.uncompressed_size)
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
    let header = frame::parse_zstd(data).ok_or_else(|| {
        CompressionError::CorruptedData("zstd compression header is invalid".to_string())
    })?;
    let dictionary = if header.dictionary_id == PLAIN_ZSTD_DICTIONARY_ID {
        None
    } else {
        Some(
            dictionaries::lookup(header.dictionary_id)
                .ok_or(CompressionError::UnknownDictionary(header.dictionary_id))?,
        )
    };
    zstd_codec::decompress(&data[header.len..], header.uncompressed_size, dictionary)
}

#[cfg(test)]
mod compression_tests;
