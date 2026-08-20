// SPDX-License-Identifier: Apache-2.0
//! Byte sources for streamable Tree reads.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

use bytes::Bytes;

use super::tree_stream::TreeStreamError;

/// How a tree body may be trusted for content-address verification.
///
/// Sequential reads from offset 0 can recompute the typed tree hash. Ranged
/// resume skips the prefix, so it is allowed only for uncompressed loose HTR4
/// files stored at the content-hash path. That hash-path placement is the
/// documented verified-placement invariant: the filename is the tree id the
/// bytes claim to be. Memory copies, pack extracts, and re-encoded bodies
/// stay on [`Self::SequentialVerify`]. Silent weaker validation is not allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeBodyIntegrity {
    /// Loose uncompressed HTR4 at the content-hash path.
    VerifiedPlacement,
    /// The reader must start at entry 0 and call `finish_and_verify`.
    SequentialVerify,
}

/// Random-access source of an uncompressed canonical tree body.
pub trait TreeByteSource {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError>;
    fn len(&self) -> u64;
    fn integrity(&self) -> TreeBodyIntegrity;
    fn bytes_read(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory tree body, used by tests and stores that already hold the bytes.
#[derive(Debug)]
pub struct BytesTreeSource {
    bytes: Bytes,
    integrity: TreeBodyIntegrity,
    bytes_read: u64,
}

impl BytesTreeSource {
    pub fn verified_placement(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
            integrity: TreeBodyIntegrity::VerifiedPlacement,
            bytes_read: 0,
        }
    }

    pub fn sequential_verify(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
            integrity: TreeBodyIntegrity::SequentialVerify,
            bytes_read: 0,
        }
    }
}

impl TreeByteSource for BytesTreeSource {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        let start =
            usize::try_from(offset).map_err(|_| TreeStreamError::TruncatedFrame { offset })?;
        let end = start
            .checked_add(buf.len())
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        let slice = self
            .bytes
            .get(start..end)
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        buf.copy_from_slice(slice);
        self.bytes_read += buf.len() as u64;
        Ok(())
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn integrity(&self) -> TreeBodyIntegrity {
        self.integrity
    }

    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

/// File-backed tree body. Resume seeks to the cursor offset; the prefix is
/// not read.
#[derive(Debug)]
pub struct FileTreeSource {
    file: File,
    len: u64,
    integrity: TreeBodyIntegrity,
    bytes_read: u64,
}

impl FileTreeSource {
    pub fn verified_placement(file: File, len: u64) -> Self {
        Self {
            file,
            len,
            integrity: TreeBodyIntegrity::VerifiedPlacement,
            bytes_read: 0,
        }
    }
}

impl TreeByteSource for FileTreeSource {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(TreeStreamError::TruncatedFrame { offset });
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        self.bytes_read += buf.len() as u64;
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn integrity(&self) -> TreeBodyIntegrity {
        self.integrity
    }

    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

/// Store-facing handle: either in-memory bytes or a seekable loose file.
#[derive(Debug)]
pub enum OpenedTreeBody {
    Bytes(BytesTreeSource),
    File(FileTreeSource),
}

impl TreeByteSource for OpenedTreeBody {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        match self {
            Self::Bytes(source) => source.read_exact_at(offset, buf),
            Self::File(source) => source.read_exact_at(offset, buf),
        }
    }

    fn len(&self) -> u64 {
        match self {
            Self::Bytes(source) => source.len(),
            Self::File(source) => source.len(),
        }
    }

    fn integrity(&self) -> TreeBodyIntegrity {
        match self {
            Self::Bytes(source) => source.integrity(),
            Self::File(source) => source.integrity(),
        }
    }

    fn bytes_read(&self) -> u64 {
        match self {
            Self::Bytes(source) => source.bytes_read(),
            Self::File(source) => source.bytes_read(),
        }
    }
}
