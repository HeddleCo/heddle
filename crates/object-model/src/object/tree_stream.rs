// SPDX-License-Identifier: Apache-2.0
//! Streaming Tree entry reader with persistable resume cursors.

use serde::{Deserialize, Serialize};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    ContentHash, EntryType, FileMode, SpoolId, StateId, Tree, TreeEntry, TreeError,
    tree::git_format_from_tag,
    tree_canonical::{
        TREE_BLOCK_ENCODING_VERSION, TREE_BLOCK_INDEX_LEN, TREE_BLOCK_PREAMBLE_LEN,
        TREE_ENCODING_VERSION, TREE_HEADER_LEN, TREE_LEAN_ENCODING_VERSION, TREE_LEAN_MAGIC,
        TreeBlockHeader, TreeBlockIndex, TreeHeader, decode_block_header, decode_block_index,
        decode_block_payload, decode_entry_frame, decode_header,
    },
    tree_source::{TreeBodyIntegrity, TreeByteSource},
};

/// Failure while streaming or range-resuming a canonical tree.
#[derive(Debug, thiserror::Error)]
pub enum TreeStreamError {
    #[error("invalid tree entry: {0}")]
    Invalid(#[from] TreeError),
    #[error(
        "unsupported tree encoding version {found} (this binary supports {TREE_ENCODING_VERSION}, {TREE_BLOCK_ENCODING_VERSION}, and {TREE_LEAN_ENCODING_VERSION})"
    )]
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
    #[error("tree compression failed: {0}")]
    Compression(String),
    #[error("tree I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoded tree hash {found} does not match {expected}")]
    HashMismatch {
        expected: ContentHash,
        found: ContentHash,
    },
}

/// Caller-sized page budget. Zero limits fail closed.
///
/// Fields stay private so callers cannot bypass [`Self::new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreePageLimits {
    max_entries: usize,
    max_decoded_bytes: usize,
}

impl TreePageLimits {
    pub fn new(max_entries: usize, max_decoded_bytes: usize) -> Result<Self, TreeStreamError> {
        if max_entries == 0 || max_decoded_bytes == 0 {
            return Err(TreeStreamError::InvalidPageLimits);
        }
        Ok(Self {
            max_entries,
            max_decoded_bytes,
        })
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn max_decoded_bytes(&self) -> usize {
        self.max_decoded_bytes
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
        Self::start_for_version(tree_id, TREE_ENCODING_VERSION)
    }

    fn start_for_version(tree_id: ContentHash, encoding_version: u8) -> Self {
        Self {
            tree_id,
            encoding_version,
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

#[derive(Debug)]
enum TreeReaderLayout {
    Raw,
    Lean {
        body_start: u64,
    },
    Blocked {
        header: TreeBlockHeader,
        cache: Option<DecodedBlock>,
        validated_blocks: Vec<bool>,
    },
}

#[derive(Debug)]
struct DecodedBlock {
    block: usize,
    raw: Vec<u8>,
    frames: Vec<(usize, usize)>,
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
    layout: TreeReaderLayout,
    cursor: TreeResumeCursor,
    hasher: Option<blake3::Hasher>,
    lean_hash_entries: Option<Vec<TreeEntry>>,
    decoded_logical_len: u64,
    started_at_zero: bool,
    pending: Option<(TreeEntry, usize)>,
    finished: bool,
}

impl<S: TreeByteSource> TreeEntryReader<S> {
    pub fn open(
        mut source: S,
        expected_id: ContentHash,
        resume: Option<&TreeResumeCursor>,
    ) -> Result<Self, TreeStreamError> {
        let mut magic = [0u8; 4];
        source.read_exact_at(0, &mut magic)?;
        let (header, layout) = if &magic == TREE_LEAN_MAGIC {
            let (entry_count, body_start) = read_source_varint(&mut source, 4)?;
            let entry_count = u64::try_from(entry_count)
                .map_err(|_| TreeStreamError::Malformed("tree entry count exceeds u64".into()))?;
            let payload_len = source
                .len()
                .checked_sub(body_start)
                .ok_or(TreeStreamError::TruncatedFrame { offset: body_start })?;
            (
                TreeHeader {
                    version: TREE_LEAN_ENCODING_VERSION,
                    tree_id: expected_id,
                    entry_count,
                    payload_len,
                    logical_len: 0,
                },
                TreeReaderLayout::Lean { body_start },
            )
        } else {
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
            let layout = if header.version == TREE_BLOCK_ENCODING_VERSION {
                let mut preamble = [0u8; TREE_BLOCK_PREAMBLE_LEN];
                source.read_exact_at(TREE_HEADER_LEN as u64, &mut preamble)?;
                let block_header = decode_block_header(&header, &preamble, source.len())?;
                TreeReaderLayout::Blocked {
                    header: block_header,
                    cache: None,
                    validated_blocks: vec![false; block_header.block_count],
                }
            } else {
                TreeReaderLayout::Raw
            };
            (header, layout)
        };
        let mut cursor = resume
            .cloned()
            .unwrap_or_else(|| TreeResumeCursor::start_for_version(expected_id, header.version));
        if resume.is_none()
            && let TreeReaderLayout::Lean { body_start } = &layout
        {
            cursor.byte_offset = *body_start;
        }
        validate_cursor(&header, &layout, &cursor)?;
        if cursor.ordinal > 0 && source.integrity() != TreeBodyIntegrity::VerifiedPlacement {
            return Err(TreeStreamError::UnverifiedRange);
        }
        let is_lean = matches!(layout, TreeReaderLayout::Lean { .. });
        let hasher = (cursor.ordinal == 0 && !is_lean)
            .then(|| ContentHash::typed_hasher("tree", header.logical_len));
        let lean_hash_entries = (cursor.ordinal == 0 && is_lean).then(Vec::new);
        let started_at_zero = cursor.ordinal == 0;
        let mut reader = Self {
            source,
            header,
            layout,
            cursor,
            hasher,
            lean_hash_entries,
            decoded_logical_len: 0,
            started_at_zero,
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
        if limits.max_entries() == 0 || limits.max_decoded_bytes() == 0 {
            return Err(TreeStreamError::InvalidPageLimits);
        }
        if self.cursor.ordinal == self.header.entry_count {
            return Ok(None);
        }
        let mut entries = Vec::new();
        let mut decoded_bytes = 0usize;
        while entries.len() < limits.max_entries() && self.cursor.ordinal < self.header.entry_count
        {
            let (entry, consumed) = self.take_next_entry()?;
            let size = entry.decoded_size();
            if size > limits.max_decoded_bytes() {
                return Err(TreeStreamError::OversizedEntry {
                    decoded_bytes: size,
                    max_decoded_bytes: limits.max_decoded_bytes(),
                });
            }
            if !entries.is_empty()
                && decoded_bytes.saturating_add(size) > limits.max_decoded_bytes()
            {
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

    /// Yield one decoded entry. Used by full-object collect without a page ceiling.
    pub fn next_entry(&mut self) -> Result<Option<TreeEntry>, TreeStreamError> {
        if self.cursor.ordinal == self.header.entry_count {
            return Ok(None);
        }
        let (entry, consumed) = self.take_next_entry()?;
        self.commit_entry(&entry, consumed)?;
        Ok(Some(entry))
    }

    pub fn finish_and_verify(&mut self) -> Result<(), TreeStreamError> {
        if self.cursor.ordinal != self.header.entry_count {
            return Err(TreeStreamError::UnexpectedEof {
                expected: self.header.entry_count,
                decoded: self.cursor.ordinal,
            });
        }
        let payload_end = self.logical_payload_end();
        if self.cursor.byte_offset != payload_end {
            return Err(TreeStreamError::TrailingBytes {
                extra: payload_end.abs_diff(self.cursor.byte_offset),
            });
        }
        if let Some(hasher) = self.hasher.take() {
            if self.decoded_logical_len != self.header.logical_len {
                return Err(TreeStreamError::Malformed(
                    "declared logical length does not match entries".into(),
                ));
            }
            let found = ContentHash::from_bytes(hasher.finalize().into());
            if found != self.header.tree_id {
                return Err(TreeStreamError::HashMismatch {
                    expected: self.header.tree_id,
                    found,
                });
            }
        } else if let Some(entries) = self.lean_hash_entries.take() {
            let tree = Tree::try_from_decoded_entries(entries)?;
            let found = tree.hash();
            if found != self.header.tree_id {
                return Err(TreeStreamError::HashMismatch {
                    expected: self.header.tree_id,
                    found,
                });
            }
        } else if self.source.integrity() != TreeBodyIntegrity::VerifiedPlacement {
            return Err(TreeStreamError::UnverifiedRange);
        }
        self.validate_complete_block_layout()?;
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
        if matches!(self.layout, TreeReaderLayout::Lean { .. }) {
            return self.read_lean_entry(offset);
        }
        if matches!(self.layout, TreeReaderLayout::Blocked { .. }) {
            return self.read_blocked_entry();
        }
        let payload_end = TREE_HEADER_LEN as u64 + self.header.payload_len;
        let mut len_buf = [0u8; 4];
        self.source.read_exact_at(offset, &mut len_buf)?;
        let frame_len = u64::from(u32::from_le_bytes(len_buf));
        let frame_start = offset
            .checked_add(4)
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        let frame_end = frame_start
            .checked_add(frame_len)
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        if frame_end > payload_end || frame_end > self.source.len() {
            return Err(TreeStreamError::TruncatedFrame { offset });
        }
        let frame_len =
            usize::try_from(frame_len).map_err(|_| TreeStreamError::TruncatedFrame { offset })?;
        let mut frame = vec![0u8; frame_len];
        self.source.read_exact_at(frame_start, &mut frame)?;
        let entry = decode_entry_frame(&frame)?;
        Ok((entry, 4 + frame_len))
    }

    fn logical_payload_end(&self) -> u64 {
        match &self.layout {
            TreeReaderLayout::Raw => TREE_HEADER_LEN as u64 + self.header.payload_len,
            TreeReaderLayout::Lean { .. } => self.source.len(),
            TreeReaderLayout::Blocked { header, .. } => {
                TREE_HEADER_LEN as u64 + header.raw_payload_len
            }
        }
    }

    fn read_lean_entry(&mut self, offset: u64) -> Result<(TreeEntry, usize), TreeStreamError> {
        let mut cursor = offset;
        let tag = read_source_byte(&mut self.source, &mut cursor)?;
        let mode = FileMode::from_byte(tag >> 3).ok_or_else(|| {
            TreeStreamError::Malformed(format!("invalid compact entry mode {}", tag >> 3))
        })?;
        let kind = EntryType::from_byte(tag & 0x07).ok_or_else(|| {
            TreeStreamError::Malformed(format!("invalid compact entry kind {}", tag & 0x07))
        })?;
        let prefix = read_source_varint_at(&mut self.source, &mut cursor)?;
        let suffix_len = read_source_varint_at(&mut self.source, &mut cursor)?;
        let previous = self.cursor.prev_name.as_deref().unwrap_or("");
        if prefix > previous.len() {
            return Err(TreeStreamError::Malformed(
                "compact name prefix exceeds predecessor".into(),
            ));
        }
        let mut suffix = vec![0u8; suffix_len];
        self.source.read_exact_at(cursor, &mut suffix)?;
        cursor = cursor
            .checked_add(suffix_len as u64)
            .ok_or_else(|| TreeStreamError::Malformed("compact name offset overflow".into()))?;
        let mut name = previous.as_bytes()[..prefix].to_vec();
        name.extend_from_slice(&suffix);
        let name = String::from_utf8(name)
            .map_err(|_| TreeStreamError::Malformed("compact entry name is not UTF-8".into()))?;
        let entry = match kind {
            EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
                let mut bytes = [0u8; 32];
                self.source.read_exact_at(cursor, &mut bytes)?;
                cursor = cursor.checked_add(32).ok_or_else(|| {
                    TreeStreamError::Malformed("compact hash offset overflow".into())
                })?;
                let hash = ContentHash::from_bytes(bytes);
                match kind {
                    EntryType::Blob => TreeEntry::file(name, hash, mode == FileMode::Executable)?,
                    EntryType::Tree => TreeEntry::directory(name, hash)?,
                    EntryType::Symlink => TreeEntry::symlink(name, hash)?,
                    EntryType::Gitlink | EntryType::Spoollink => {
                        return Err(TreeStreamError::Malformed(
                            "invalid compact content-addressed kind".into(),
                        ));
                    }
                }
            }
            EntryType::Gitlink => {
                let format = git_format_from_tag(read_source_byte(&mut self.source, &mut cursor)?)?;
                let oid_len = match format {
                    GitObjectFormat::Sha1 => 20,
                    GitObjectFormat::Sha256 => 32,
                };
                let mut oid = vec![0u8; oid_len];
                self.source.read_exact_at(cursor, &mut oid)?;
                cursor = cursor.checked_add(oid_len as u64).ok_or_else(|| {
                    TreeStreamError::Malformed("compact gitlink offset overflow".into())
                })?;
                let target = GitObjectId::from_raw(format, &oid).map_err(|error| {
                    TreeStreamError::Malformed(format!("invalid compact gitlink: {error}"))
                })?;
                TreeEntry::gitlink(name, target)?
            }
            EntryType::Spoollink => {
                let spool_len = read_source_varint_at(&mut self.source, &mut cursor)?;
                let mut spool = vec![0u8; spool_len];
                self.source.read_exact_at(cursor, &mut spool)?;
                cursor = cursor.checked_add(spool_len as u64).ok_or_else(|| {
                    TreeStreamError::Malformed("compact spool offset overflow".into())
                })?;
                let spool = std::str::from_utf8(&spool).map_err(|_| {
                    TreeStreamError::Malformed("compact spool id is not UTF-8".into())
                })?;
                let spool_id = SpoolId::parse(spool).map_err(|error| {
                    TreeStreamError::Malformed(format!("invalid compact spool id: {error}"))
                })?;
                let mut state = [0u8; 32];
                self.source.read_exact_at(cursor, &mut state)?;
                cursor = cursor.checked_add(32).ok_or_else(|| {
                    TreeStreamError::Malformed("compact state offset overflow".into())
                })?;
                TreeEntry::spoollink(name, spool_id, StateId::from_bytes(state))?
            }
        };
        if entry.mode() != mode {
            return Err(TreeStreamError::Malformed(format!(
                "compact entry kind/mode mismatch for {}: {kind:?}/{mode:?}",
                entry.name()
            )));
        }
        let consumed = usize::try_from(cursor - offset)
            .map_err(|_| TreeStreamError::Malformed("compact entry length exceeds usize".into()))?;
        Ok((entry, consumed))
    }

    fn read_blocked_entry(&mut self) -> Result<(TreeEntry, usize), TreeStreamError> {
        let block_header = match &self.layout {
            TreeReaderLayout::Blocked { header, .. } => *header,
            TreeReaderLayout::Raw | TreeReaderLayout::Lean { .. } => {
                return Err(TreeStreamError::Malformed(
                    "raw tree entered blocked reader".into(),
                ));
            }
        };
        let ordinal = usize::try_from(self.cursor.ordinal)
            .map_err(|_| TreeStreamError::Malformed("tree ordinal exceeds usize".into()))?;
        let block = ordinal / block_header.block_entries;
        let within_block = ordinal % block_header.block_entries;
        let index = self.read_block_index(block, &block_header)?;
        let block_logical_start = (TREE_HEADER_LEN as u64)
            .checked_add(index.raw_offset)
            .ok_or_else(|| TreeStreamError::Malformed("tree block raw offset overflow".into()))?;

        if within_block == 0 {
            if self.cursor.byte_offset != block_logical_start {
                return Err(TreeStreamError::CursorMismatch(
                    "cursor byte offset is not the block restart boundary".into(),
                ));
            }
            return self.read_block_anchor(index);
        }

        let needs_load = match &self.layout {
            TreeReaderLayout::Blocked { cache, .. } => {
                cache.as_ref().is_none_or(|cached| cached.block != block)
            }
            TreeReaderLayout::Raw | TreeReaderLayout::Lean { .. } => true,
        };
        if needs_load {
            let decoded = self.load_block(index, block, &block_header)?;
            if let TreeReaderLayout::Blocked {
                cache,
                validated_blocks,
                ..
            } = &mut self.layout
            {
                *cache = Some(decoded);
                if let Some(validated) = validated_blocks.get_mut(block) {
                    *validated = true;
                }
            }
        }
        let cached = match &self.layout {
            TreeReaderLayout::Blocked {
                cache: Some(cached),
                ..
            } => cached,
            _ => {
                return Err(TreeStreamError::Malformed(
                    "tree block cache was not populated".into(),
                ));
            }
        };
        let (frame_start, frame_end) =
            cached
                .frames
                .get(within_block)
                .copied()
                .ok_or(TreeStreamError::UnexpectedEof {
                    expected: self.cursor.ordinal + 1,
                    decoded: self.cursor.ordinal,
                })?;
        let frame =
            cached
                .raw
                .get(frame_start + 4..frame_end)
                .ok_or(TreeStreamError::TruncatedFrame {
                    offset: frame_start as u64,
                })?;
        let consumed = frame_end - frame_start;
        let expected_offset = block_logical_start
            .checked_add(frame_start as u64)
            .ok_or_else(|| TreeStreamError::Malformed("tree cursor offset overflow".into()))?;
        if self.cursor.byte_offset != expected_offset {
            return Err(TreeStreamError::CursorMismatch(
                "cursor byte offset is not an entry boundary".into(),
            ));
        }
        Ok((decode_entry_frame(frame)?, consumed))
    }

    fn read_block_index(
        &mut self,
        block: usize,
        block_header: &TreeBlockHeader,
    ) -> Result<TreeBlockIndex, TreeStreamError> {
        let relative = block
            .checked_mul(TREE_BLOCK_INDEX_LEN)
            .ok_or_else(|| TreeStreamError::Malformed("tree block index overflow".into()))?;
        let offset = TREE_HEADER_LEN
            .checked_add(TREE_BLOCK_PREAMBLE_LEN)
            .and_then(|start| start.checked_add(relative))
            .ok_or_else(|| TreeStreamError::Malformed("tree block index offset overflow".into()))?;
        let mut bytes = [0u8; TREE_BLOCK_INDEX_LEN];
        self.source.read_exact_at(offset as u64, &mut bytes)?;
        decode_block_index(&bytes, block, block_header, self.source.len())
    }

    fn read_block_anchor(
        &mut self,
        index: TreeBlockIndex,
    ) -> Result<(TreeEntry, usize), TreeStreamError> {
        let mut len_bytes = [0u8; 4];
        self.source
            .read_exact_at(index.stored_offset, &mut len_bytes)?;
        let frame_len = u32::from_le_bytes(len_bytes) as usize;
        let consumed = 4usize
            .checked_add(frame_len)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: index.stored_offset,
            })?;
        if consumed > index.stored_len || consumed > index.raw_len {
            return Err(TreeStreamError::TruncatedFrame {
                offset: index.stored_offset,
            });
        }
        let mut frame = vec![0u8; frame_len];
        self.source
            .read_exact_at(index.stored_offset + 4, &mut frame)?;
        Ok((decode_entry_frame(&frame)?, consumed))
    }

    fn load_block(
        &mut self,
        index: TreeBlockIndex,
        block: usize,
        block_header: &TreeBlockHeader,
    ) -> Result<DecodedBlock, TreeStreamError> {
        let mut stored = vec![0u8; index.stored_len];
        self.source
            .read_exact_at(index.stored_offset, &mut stored)?;
        let raw = decode_block_payload(&stored, index.raw_len)?;
        let first_entry = block
            .checked_mul(block_header.block_entries)
            .ok_or_else(|| TreeStreamError::Malformed("tree block ordinal overflow".into()))?;
        let remaining = usize::try_from(self.header.entry_count)
            .map_err(|_| TreeStreamError::Malformed("tree entry count exceeds usize".into()))?
            .checked_sub(first_entry)
            .ok_or_else(|| TreeStreamError::Malformed("tree block starts past entries".into()))?;
        let expected_entries = remaining.min(block_header.block_entries);
        let frames = decode_block_frames(&raw, expected_entries)?;
        Ok(DecodedBlock { block, raw, frames })
    }

    fn validate_complete_block_layout(&mut self) -> Result<(), TreeStreamError> {
        let block_header = match &self.layout {
            TreeReaderLayout::Raw | TreeReaderLayout::Lean { .. } => return Ok(()),
            TreeReaderLayout::Blocked { header, .. } => *header,
        };
        let mut expected_raw_offset = 0u64;
        let mut expected_stored_offset = block_header.index_end;
        for block in 0..block_header.block_count {
            let index = self.read_block_index(block, &block_header)?;
            if index.raw_offset != expected_raw_offset
                || index.stored_offset != expected_stored_offset
            {
                return Err(TreeStreamError::Malformed(
                    "tree blocks are not contiguous".into(),
                ));
            }
            expected_raw_offset = expected_raw_offset
                .checked_add(index.raw_len as u64)
                .ok_or_else(|| TreeStreamError::Malformed("raw tree length overflow".into()))?;
            expected_stored_offset = expected_stored_offset
                .checked_add(index.stored_len as u64)
                .ok_or_else(|| TreeStreamError::Malformed("stored tree length overflow".into()))?;
            let needs_payload_validation = self.started_at_zero
                && match &self.layout {
                    TreeReaderLayout::Blocked {
                        validated_blocks, ..
                    } => validated_blocks
                        .get(block)
                        .is_none_or(|validated| !validated),
                    TreeReaderLayout::Raw | TreeReaderLayout::Lean { .. } => false,
                };
            if needs_payload_validation {
                let _ = self.load_block(index, block, &block_header)?;
                if let TreeReaderLayout::Blocked {
                    validated_blocks, ..
                } = &mut self.layout
                    && let Some(validated) = validated_blocks.get_mut(block)
                {
                    *validated = true;
                }
            }
        }
        if expected_raw_offset != block_header.raw_payload_len
            || expected_stored_offset != self.source.len()
        {
            return Err(TreeStreamError::Malformed(
                "tree block lengths do not match the header".into(),
            ));
        }
        Ok(())
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
        if let Some(entries) = &mut self.lean_hash_entries {
            entries.push(entry.clone());
        }
        self.decoded_logical_len = self
            .decoded_logical_len
            .checked_add(entry.encoded_len() as u64)
            .ok_or_else(|| TreeStreamError::Malformed("logical length overflow".into()))?;
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
#[path = "tree_stream_proptests.rs"]
mod tree_stream_proptests;
#[cfg(test)]
#[path = "tree_stream_tests.rs"]
mod tree_stream_tests;

fn decode_block_frames(
    raw: &[u8],
    expected_entries: usize,
) -> Result<Vec<(usize, usize)>, TreeStreamError> {
    let mut frames = Vec::with_capacity(expected_entries);
    let mut offset = 0usize;
    for _ in 0..expected_entries {
        let len_bytes = raw
            .get(offset..offset + 4)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?;
        let frame_len = u32::from_le_bytes(
            len_bytes
                .try_into()
                .map_err(|_| TreeStreamError::Malformed("invalid block frame length".into()))?,
        ) as usize;
        let frame_start = offset
            .checked_add(4)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?;
        let frame_end =
            frame_start
                .checked_add(frame_len)
                .ok_or(TreeStreamError::TruncatedFrame {
                    offset: offset as u64,
                })?;
        if frame_end > raw.len() {
            return Err(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            });
        }
        frames.push((offset, frame_end));
        offset = frame_end;
    }
    if offset != raw.len() {
        return Err(TreeStreamError::TrailingBytes {
            extra: raw.len().abs_diff(offset) as u64,
        });
    }
    Ok(frames)
}

fn validate_cursor(
    header: &TreeHeader,
    layout: &TreeReaderLayout,
    cursor: &TreeResumeCursor,
) -> Result<(), TreeStreamError> {
    if cursor.encoding_version != header.version {
        return Err(TreeStreamError::CursorMismatch(format!(
            "encoding version {} is not {}",
            cursor.encoding_version, header.version
        )));
    }
    if cursor.tree_id != header.tree_id {
        return Err(TreeStreamError::CursorMismatch(
            "cursor tree id does not match the opened object".into(),
        ));
    }
    let payload_end = match layout {
        TreeReaderLayout::Raw => TREE_HEADER_LEN as u64 + header.payload_len,
        TreeReaderLayout::Lean { body_start } => body_start + header.payload_len,
        TreeReaderLayout::Blocked { header, .. } => TREE_HEADER_LEN as u64 + header.raw_payload_len,
    };
    let payload_start = match layout {
        TreeReaderLayout::Raw | TreeReaderLayout::Blocked { .. } => TREE_HEADER_LEN as u64,
        TreeReaderLayout::Lean { body_start } => *body_start,
    };
    if cursor.ordinal > header.entry_count {
        return Err(TreeStreamError::CursorMismatch(
            "cursor ordinal is past the declared entry count".into(),
        ));
    }
    if cursor.ordinal == 0 {
        if cursor.byte_offset != payload_start || cursor.prev_name.is_some() {
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
    if cursor.byte_offset < payload_start || cursor.byte_offset >= payload_end {
        return Err(TreeStreamError::CursorMismatch(
            "cursor byte offset is not inside the payload".into(),
        ));
    }
    Ok(())
}

fn read_source_byte<S: TreeByteSource>(
    source: &mut S,
    offset: &mut u64,
) -> Result<u8, TreeStreamError> {
    let mut byte = [0u8; 1];
    source.read_exact_at(*offset, &mut byte)?;
    *offset = offset
        .checked_add(1)
        .ok_or_else(|| TreeStreamError::Malformed("tree byte offset overflow".into()))?;
    Ok(byte[0])
}

fn read_source_varint<S: TreeByteSource>(
    source: &mut S,
    offset: u64,
) -> Result<(usize, u64), TreeStreamError> {
    let mut cursor = offset;
    let value = read_source_varint_at(source, &mut cursor)?;
    Ok((value, cursor))
}

fn read_source_varint_at<S: TreeByteSource>(
    source: &mut S,
    offset: &mut u64,
) -> Result<usize, TreeStreamError> {
    let mut value = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = read_source_byte(source, offset)?;
        value |= ((byte & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or_else(|| TreeStreamError::Malformed("varint overflow".into()))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(TreeStreamError::Malformed("varint overflow".into()))
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
        while let Some(entry) = reader.next_entry()? {
            entries.push(entry);
        }
        reader.finish_and_verify()?;
        let tree = Tree::try_from_decoded_entries(entries).map_err(TreeStreamError::from)?;
        let found = tree.hash();
        if found != header.tree_id {
            return Err(TreeStreamError::HashMismatch {
                expected: header.tree_id,
                found,
            });
        }
        Ok(tree)
    }
}
