// SPDX-License-Identifier: Apache-2.0
//! Streaming Tree entry reader with persistable resume cursors.

use serde::{Deserialize, Serialize};

use super::{
    ContentHash, Tree, TreeEntry, TreeError,
    tree_canonical::{
        TREE_ENCODING_VERSION, TREE_HEADER_LEN, TreeHeader, decode_entry_frame, decode_header,
    },
    tree_source::{TreeBodyIntegrity, TreeByteSource},
};

/// Failure while streaming or range-resuming a canonical tree.
#[derive(Debug, thiserror::Error)]
pub enum TreeStreamError {
    #[error("invalid tree entry: {0}")]
    Invalid(#[from] TreeError),
    #[error("unsupported tree encoding version {found} (this binary writes {TREE_ENCODING_VERSION})")]
    UnsupportedVersion { found: u8 },
    #[error("tree resume cursor does not match this object: {0}")]
    CursorMismatch(String),
    #[error("truncated tree frame at byte {offset}")]
    TruncatedFrame { offset: u64 },
    #[error("tree payload has {extra} trailing byte(s) after declared end")]
    TrailingBytes { extra: u64 },
    #[error("tree ended after {decoded} of {expected} declared entries")]
    UnexpectedEof { expected: u64, decoded: u64 },
    #[error("tree entry exceeds page byte limit ({decoded_bytes} > {max_decoded_bytes})")]
    OversizedEntry {
        decoded_bytes: usize,
        max_decoded_bytes: usize,
    },
    #[error("tree page limits must be nonzero")]
    InvalidPageLimits,
    #[error("ranged tree resume requires a verified-placement object source")]
    UnverifiedRange,
    #[error("malformed tree encoding: {0}")]
    Malformed(String),
    #[error("tree I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoded tree hash {found} does not match {expected}")]
    HashMismatch {
        expected: ContentHash,
        found: ContentHash,
    },
}

/// Caller-sized page budget. Zero limits fail closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreePageLimits {
    pub max_entries: usize,
    pub max_decoded_bytes: usize,
}

impl TreePageLimits {
    pub fn new(
        max_entries: usize,
        max_decoded_bytes: usize,
    ) -> Result<Self, TreeStreamError> {
        if max_entries == 0 || max_decoded_bytes == 0 {
            return Err(TreeStreamError::InvalidPageLimits);
        }
        Ok(Self {
            max_entries,
            max_decoded_bytes,
        })
    }
}

/// Persistable entry-boundary cursor bound to a tree id and encoding version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeResumeCursor {
    pub(crate) tree_id: ContentHash,
    pub(crate) encoding_version: u8,
    pub(crate) ordinal: u64,
    pub(crate) byte_offset: u64,
    pub(crate) prev_name: Option<String>,
}

impl TreeResumeCursor {
    pub fn start(tree_id: ContentHash) -> Self {
        Self {
            tree_id,
            encoding_version: TREE_ENCODING_VERSION,
            ordinal: 0,
            byte_offset: TREE_HEADER_LEN as u64,
            prev_name: None,
        }
    }

    pub fn tree_id(&self) -> ContentHash {
        self.tree_id
    }

    pub fn encoding_version(&self) -> u8 {
        self.encoding_version
    }

    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    pub fn prev_name(&self) -> Option<&str> {
        self.prev_name.as_deref()
    }
}

/// One bounded page of decoded entries plus the cursor after the last entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreePage {
    pub entries: Vec<TreeEntry>,
    pub resume_cursor: TreeResumeCursor,
}

/// Incremental HTR4 reader. Does not materialize a full `Vec<TreeEntry>`.
#[derive(Debug)]
pub struct TreeEntryReader<S: TreeByteSource> {
    source: S,
    header: TreeHeader,
    cursor: TreeResumeCursor,
    hasher: Option<blake3::Hasher>,
    pending: Option<(TreeEntry, usize)>,
    finished: bool,
}

impl<S: TreeByteSource> TreeEntryReader<S> {
    pub fn open(
        mut source: S,
        expected_id: ContentHash,
        resume: Option<&TreeResumeCursor>,
    ) -> Result<Self, TreeStreamError> {
        let mut header_buf = [0u8; TREE_HEADER_LEN];
        source.read_exact_at(0, &mut header_buf)?;
        let header = decode_header(&header_buf)?;
        if header.tree_id != expected_id {
            return Err(TreeStreamError::HashMismatch {
                expected: expected_id,
                found: header.tree_id,
            });
        }
        let expected_len = TREE_HEADER_LEN as u64 + header.payload_len;
        if source.len() < expected_len {
            return Err(TreeStreamError::TruncatedFrame {
                offset: source.len(),
            });
        }
        if source.len() > expected_len {
            return Err(TreeStreamError::TrailingBytes {
                extra: source.len() - expected_len,
            });
        }
        let cursor = resume.cloned().unwrap_or_else(|| TreeResumeCursor::start(expected_id));
        validate_cursor(&header, &cursor)?;
        if cursor.ordinal > 0 && source.integrity() != TreeBodyIntegrity::VerifiedPlacement {
            return Err(TreeStreamError::UnverifiedRange);
        }
        let hasher = (cursor.ordinal == 0)
            .then(|| ContentHash::typed_hasher("tree", header.logical_len));
        let mut reader = Self {
            source,
            header,
            cursor,
            hasher,
            pending: None,
            finished: false,
        };
        reader.arm_pending_at_cursor()?;
        Ok(reader)
    }

    pub fn header(&self) -> &TreeHeader {
        &self.header
    }

    pub fn bytes_read(&self) -> u64 {
        self.source.bytes_read()
    }

    pub fn next_page(
        &mut self,
        limits: TreePageLimits,
    ) -> Result<Option<TreePage>, TreeStreamError> {
        if self.cursor.ordinal == self.header.entry_count {
            return Ok(None);
        }
        let mut entries = Vec::new();
        let mut decoded_bytes = 0usize;
        while entries.len() < limits.max_entries && self.cursor.ordinal < self.header.entry_count {
            let (entry, consumed) = self.take_next_entry()?;
            let size = entry.decoded_size();
            if size > limits.max_decoded_bytes {
                return Err(TreeStreamError::OversizedEntry {
                    decoded_bytes: size,
                    max_decoded_bytes: limits.max_decoded_bytes,
                });
            }
            if !entries.is_empty() && decoded_bytes.saturating_add(size) > limits.max_decoded_bytes {
                self.pending = Some((entry, consumed));
                break;
            }
            self.commit_entry(&entry, consumed)?;
            decoded_bytes += size;
            entries.push(entry);
        }
        Ok(Some(TreePage {
            entries,
            resume_cursor: self.cursor.clone(),
        }))
    }

    pub fn finish_and_verify(&mut self) -> Result<(), TreeStreamError> {
        if self.cursor.ordinal != self.header.entry_count {
            return Err(TreeStreamError::UnexpectedEof {
                expected: self.header.entry_count,
                decoded: self.cursor.ordinal,
            });
        }
        if let Some(hasher) = self.hasher.take() {
            let found = ContentHash::from_bytes(hasher.finalize().into());
            if found != self.header.tree_id {
                return Err(TreeStreamError::HashMismatch {
                    expected: self.header.tree_id,
                    found,
                });
            }
        } else if self.source.integrity() != TreeBodyIntegrity::VerifiedPlacement {
            return Err(TreeStreamError::UnverifiedRange);
        }
        self.finished = true;
        Ok(())
    }

    fn take_next_entry(&mut self) -> Result<(TreeEntry, usize), TreeStreamError> {
        if let Some(pending) = self.pending.take() {
            return Ok(pending);
        }
        self.read_entry_at(self.cursor.byte_offset)
    }

    fn read_entry_at(&mut self, offset: u64) -> Result<(TreeEntry, usize), TreeStreamError> {
        let mut len_buf = [0u8; 4];
        self.source.read_exact_at(offset, &mut len_buf)?;
        let frame_len = u32::from_le_bytes(len_buf) as usize;
        let mut frame = vec![0u8; frame_len];
        self.source
            .read_exact_at(offset.saturating_add(4), &mut frame)?;
        let entry = decode_entry_frame(&frame)?;
        Ok((entry, 4 + frame_len))
    }

    fn commit_entry(&mut self, entry: &TreeEntry, consumed: usize) -> Result<(), TreeStreamError> {
        if let Some(previous) = self.cursor.prev_name.as_deref()
            && previous >= entry.name()
        {
            return Err(TreeError::InvalidStructure(
                "entries must be strictly sorted by name".into(),
            )
            .into());
        }
        if let Some(hasher) = &mut self.hasher {
            entry.update_hasher(hasher);
        }
        self.cursor.ordinal += 1;
        self.cursor.byte_offset += consumed as u64;
        self.cursor.prev_name = Some(entry.name().to_string());
        Ok(())
    }

    fn arm_pending_at_cursor(&mut self) -> Result<(), TreeStreamError> {
        if self.cursor.ordinal == 0 || self.cursor.ordinal == self.header.entry_count {
            return Ok(());
        }
        let (entry, consumed) = self.read_entry_at(self.cursor.byte_offset)?;
        if let Some(previous) = self.cursor.prev_name.as_deref()
            && previous >= entry.name()
        {
            return Err(TreeStreamError::CursorMismatch(
                "cursor previous name is not a valid predecessor".into(),
            ));
        }
        self.pending = Some((entry, consumed));
        Ok(())
    }
}

#[cfg(test)]
#[path = "tree_stream_tests.rs"]
mod tree_stream_tests;
#[cfg(test)]
#[path = "tree_stream_proptests.rs"]
mod tree_stream_proptests;

fn validate_cursor(header: &TreeHeader, cursor: &TreeResumeCursor) -> Result<(), TreeStreamError> {
    if cursor.encoding_version != TREE_ENCODING_VERSION {
        return Err(TreeStreamError::CursorMismatch(format!(
            "encoding version {} is not {TREE_ENCODING_VERSION}",
            cursor.encoding_version
        )));
    }
    if cursor.tree_id != header.tree_id {
        return Err(TreeStreamError::CursorMismatch(
            "cursor tree id does not match the opened object".into(),
        ));
    }
    let payload_end = TREE_HEADER_LEN as u64 + header.payload_len;
    if cursor.ordinal > header.entry_count {
        return Err(TreeStreamError::CursorMismatch(
            "cursor ordinal is past the declared entry count".into(),
        ));
    }
    if cursor.ordinal == 0 {
        if cursor.byte_offset != TREE_HEADER_LEN as u64 || cursor.prev_name.is_some() {
            return Err(TreeStreamError::CursorMismatch(
                "start cursor must be the first entry boundary".into(),
            ));
        }
        return Ok(());
    }
    if cursor.ordinal == header.entry_count {
        if cursor.byte_offset != payload_end {
            return Err(TreeStreamError::CursorMismatch(
                "end cursor is not the declared payload end".into(),
            ));
        }
        return Ok(());
    }
    if cursor.byte_offset < TREE_HEADER_LEN as u64 || cursor.byte_offset >= payload_end {
        return Err(TreeStreamError::CursorMismatch(
            "cursor byte offset is not inside the payload".into(),
        ));
    }
    Ok(())
}

impl Tree {
    /// Decode HTR4 through the streaming reader and collect the eager `Tree`.
    pub fn decode_canonical_streamed(data: &[u8]) -> Result<Self, TreeStreamError> {
        let header = decode_header(data)?;
        let mut reader = TreeEntryReader::open(
            super::tree_source::BytesTreeSource::sequential_verify(bytes::Bytes::copy_from_slice(
                data,
            )),
            header.tree_id,
            None,
        )?;
        let mut entries = Vec::new();
        let limits = TreePageLimits::new(usize::MAX, usize::MAX)?;
        while let Some(page) = reader.next_page(limits)? {
            entries.extend(page.entries);
        }
        reader.finish_and_verify()?;
        Tree::try_from_decoded_entries(entries).map_err(TreeStreamError::from)
    }
}
