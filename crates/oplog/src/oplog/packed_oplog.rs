// SPDX-License-Identifier: Apache-2.0
//! Packed binary oplog.
//!
//! Repository format v3 accepts only the V4 container with StateId record
//! schema 4. Earlier containers and record schemas are refused without
//! mutation; normal loads require the latest single-file container with an EOF
//! index footer.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{TimeZone, Utc};
use heddle_schema::op_record::{
    CURRENT_OP_RECORD_SCHEMA_VERSION, decode_current_record, encode_current_record,
    validate_op_record_schema_version,
};
use objects::{
    error::{HeddleError, Result},
    fs_atomic::{create_dir_all_durable, sync_directory, temp_path, write_file_atomic},
    fs_clone::{ReflinkOutcome, try_reflink},
};

use super::oplog_types::{OpBatch, OpEntry, OpRecord};
const MAGIC: &[u8; 8] = b"LMOPLOG\0";
const INDEX_MAGIC: &[u8; 8] = b"LMOPIDX\0";
const CURRENT_CONTAINER_VERSION: u32 = 4;
const OPLOG_CONTAINER_STORAGE: &str = "packed oplog container";
const INDEX_VERSION: u32 = 1;
const LEGACY_HEADER_LEN: u64 = 8 + 4 + 8 + 8;
const V4_HEADER_LEN: u64 = 8 + 4 + 4 + 8 + 8;
const FOOTER_U64_FIELDS: u64 = 13;
const FOOTER_LEN: u64 = 8 + 4 + 4 + (FOOTER_U64_FIELDS * 8);
const ENTRY_OFFSET_RECORD_LEN: u64 = 16;
const BATCH_DIR_RECORD_LEN: u64 = 48;
const TX_DIR_RECORD_LEN: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconstructibleAppendStrategy {
    CloneAndRewriteTail,
    Rewrite,
}

fn reconstructible_append_strategy(outcome: ReflinkOutcome) -> ReconstructibleAppendStrategy {
    match outcome {
        ReflinkOutcome::Cloned => ReconstructibleAppendStrategy::CloneAndRewriteTail,
        ReflinkOutcome::Unsupported | ReflinkOutcome::SourceVanished => {
            ReconstructibleAppendStrategy::Rewrite
        }
    }
}

fn validate_container_version(version: u32) -> Result<()> {
    if version < CURRENT_CONTAINER_VERSION {
        return Err(HeddleError::StorageFormatMigrationRequired {
            storage: OPLOG_CONTAINER_STORAGE.to_string(),
            found: version,
            required: CURRENT_CONTAINER_VERSION,
        });
    }
    if version > CURRENT_CONTAINER_VERSION {
        return Err(HeddleError::StorageFormatTooNew {
            storage: OPLOG_CONTAINER_STORAGE.to_string(),
            found: version,
            supported: CURRENT_CONTAINER_VERSION,
        });
    }
    Ok(())
}

/// Version-agnostic materialized oplog data.
#[derive(Clone)]
pub(crate) struct OplogData {
    pub(crate) entries: Vec<OpEntry>, // sorted by id ascending
    pub(crate) head_id: u64,
}

fn decode_current_container(bytes: &[u8]) -> Result<OplogData> {
    let (header, cursor) = parse_header_with_cursor(bytes)?;
    let footer = PackedFooter::parse(bytes, &header)?;
    if cursor.offset as u64 > footer.entry_data_end {
        return Err(HeddleError::InvalidObject(
            "oplog footer points before the entry stream".to_string(),
        ));
    }
    let entry_bytes_end = usize::try_from(footer.entry_data_end)
        .map_err(|_| HeddleError::InvalidObject("oplog entry section too large".to_string()))?;
    let mut entry_cursor = Cursor::new(&bytes[cursor.offset..entry_bytes_end]);
    let entries = parse_current_entries(&mut entry_cursor, header.entry_count as usize)?;
    if cursor.offset + entry_cursor.offset != entry_bytes_end {
        return Err(HeddleError::InvalidObject(
            "oplog entry/index boundary disagreement".to_string(),
        ));
    }
    Ok(OplogData {
        entries,
        head_id: header.head_id,
    })
}

#[derive(Clone)]
pub(crate) struct PackedOpLog {
    pub(crate) entries: Vec<OpEntry>, // sorted by id ascending
    pub(crate) head_id: u64,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct PackedOpLogIndex {
    path: PathBuf,
    header: PackedHeader,
    footer: PackedFooter,
    file_stamp: Option<PackedFileStamp>,
    validated_indexes: Arc<ValidatedIndexSections>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackedFileStamp {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    len: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

fn packed_file_stamp(file: &File) -> Result<Option<PackedFileStamp>> {
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        Ok(Some(PackedFileStamp {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(None)
    }
}

fn read_packed_file(path: &Path) -> Result<(Vec<u8>, Option<PackedFileStamp>)> {
    let mut file = File::open(path)?;
    let file_stamp = packed_file_stamp(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, file_stamp))
}

#[derive(Clone, Copy, Debug)]
struct PackedHeader {
    entry_count: u64,
    head_id: u64,
    header_len: u64,
}

#[derive(Clone, Copy, Debug)]
struct PackedFooter {
    entry_data_end: u64,
    entry_offsets_offset: u64,
    entry_offsets_count: u64,
    batch_offsets_offset: u64,
    batch_offsets_count: u64,
    batch_dir_offset: u64,
    batch_dir_count: u64,
    tx_key_bytes_offset: u64,
    tx_key_bytes_len: u64,
    tx_dir_offset: u64,
    tx_dir_count: u64,
    entry_count: u64,
    head_id: u64,
}

#[derive(Clone, Copy, Debug)]
struct EntryOffsetRecord {
    entry_id: u64,
    entry_offset: u64,
}

#[derive(Clone, Debug)]
struct BatchDirRecord {
    batch_id: u64,
    newest_entry_id: u64,
    first_offset_index: u64,
    entry_count: u32,
    scope_state: u8,
}

#[derive(Clone, Debug)]
struct TxDirRecord {
    key_offset: u64,
    key_len: u32,
    commit_entry_id: u64,
    batch_id: u64,
}

#[derive(Debug, Default)]
struct ValidatedIndexSections {
    entry_offsets: Vec<EntryOffsetRecord>,
    batch_offsets: Vec<u64>,
    batch_dir: Vec<BatchDirRecord>,
    tx_key_bytes: Vec<u8>,
    tx_dir: Vec<TxDirRecord>,
}

impl PackedOpLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            head_id: 0,
            path,
        }
    }

    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let data = match load_latest(&bytes) {
            Ok(data) => data,
            Err(err) => {
                if let Some(data) = recover_truncated_latest(path, &bytes, &err)? {
                    data
                } else {
                    return Err(err);
                }
            }
        };
        Ok(Self {
            entries: data.entries,
            head_id: data.head_id,
            path: path.to_path_buf(),
        })
    }

    pub(crate) fn ensure_current(path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let _ = read_header(path)?;
        let _ = PackedOpLogIndex::open_v4(path)?;
        Ok(())
    }

    /// Read only the `head_id` from the fixed-size current-format header.
    ///
    /// V2/V3 containers share the legacy header prefix, but repository format
    /// v3 deliberately refuses them instead of synthesizing StateIds.
    pub(crate) fn read_head_id(path: &Path) -> Result<u64> {
        Ok(read_header(path)?.head_id)
    }

    pub(crate) fn validate_header(path: &Path) -> Result<()> {
        read_header(path).map(|_| ())
    }

    /// Cheap O(1) integrity check that the EOF index footer/trailer is present
    /// and well-framed, WITHOUT parsing the entry stream or index tables.
    ///
    /// Reads only the fixed-size footer prefix by seeking to `file_len -
    /// FOOTER_LEN` and validating the footer magic, index version, and self-
    /// declared footer length. A truncated-but-header-valid oplog (the fixed
    /// header survives, but the trailing index/footer was cut off) fails one of
    /// these three checks — either the file is now shorter than
    /// `header_len + FOOTER_LEN`, or the bytes now sitting at the (new) EOF are
    /// mid-entry-stream garbage rather than the footer magic.
    ///
    /// This is the load-bearing complement to
    /// [`validate_header`](Self::validate_header): header validation alone
    /// succeeds for a truncated oplog; pairing it with `trailer_ok` keeps the
    /// healthy path O(1) while routing damaged oplogs into salvage.
    ///
    /// Returns `Ok(false)` for any damaged/short/mis-framed trailer (so the
    /// caller can fall through to salvage); only genuine I/O errors propagate.
    pub(crate) fn trailer_ok(path: &Path) -> Result<bool> {
        let mut file = File::open(path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        // Must be able to hold at least the largest fixed header plus a footer.
        if file_len < LEGACY_HEADER_LEN + FOOTER_LEN {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(file_len - FOOTER_LEN))?;
        let magic = read_array_from_file::<8>(&mut file)?;
        if &magic != INDEX_MAGIC {
            return Ok(false);
        }
        let index_version = read_u32_from_file(&mut file)?;
        if index_version != INDEX_VERSION {
            return Ok(false);
        }
        let footer_len = read_u32_from_file(&mut file)?;
        Ok(footer_len == FOOTER_LEN as u32)
    }

    pub(crate) fn save(&self) -> Result<()> {
        let data = OplogData {
            entries: self.entries.clone(),
            head_id: self.head_id,
        };
        let mut bytes = Vec::new();
        encode_current_container(&data, &mut bytes)?;
        write_file_atomic(&self.path, &bytes)?;
        Ok(())
    }

    #[cfg(test)]
    fn serialize(&self) -> Result<Vec<u8>> {
        let data = OplogData {
            entries: self.entries.clone(),
            head_id: self.head_id,
        };
        let mut buf = Vec::new();
        encode_current_container(&data, &mut buf)?;
        Ok(buf)
    }

    #[cfg(test)]
    fn parse(bytes: &[u8], path: PathBuf) -> Result<Self> {
        let data = load_latest(bytes)?;
        Ok(Self {
            entries: data.entries,
            head_id: data.head_id,
            path,
        })
    }

    #[cfg(test)]
    pub(crate) fn append(&mut self, new_entries: Vec<OpEntry>) {
        let last_id = new_entries.last().map(|e| e.id).unwrap_or(self.head_id);
        self.entries.extend(new_entries);
        self.head_id = last_id;
    }

    pub(crate) fn set_undone(&mut self, batch_id: u64, undone: bool) {
        for entry in &mut self.entries {
            if entry.batch_id == batch_id || (entry.batch_id == 0 && entry.id == batch_id) {
                entry.undone = undone;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn collect_batches_scoped(
        &self,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Vec<OpBatch> {
        collect_batches_from_entries(self.entries.iter().rev().cloned(), count, predicate, scope)
    }
}

impl PackedOpLogIndex {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_v4(path)
    }

    fn open_v4(path: &Path) -> Result<Self> {
        let (bytes, file_stamp) = read_packed_file(path)?;
        match Self::open_v4_bytes(path, &bytes, file_stamp) {
            Ok(index) => Ok(index),
            Err(err) => {
                if recover_truncated_latest(path, &bytes, &err)?.is_some() {
                    let (bytes, file_stamp) = read_packed_file(path)?;
                    Self::open_v4_bytes(path, &bytes, file_stamp)
                } else {
                    Err(err)
                }
            }
        }
    }

    fn open_v4_bytes(
        path: &Path,
        bytes: &[u8],
        file_stamp: Option<PackedFileStamp>,
    ) -> Result<Self> {
        let header = parse_header(bytes)?;
        let footer = PackedFooter::parse(bytes, &header)?;
        let mut index = Self {
            path: path.to_path_buf(),
            header,
            footer,
            file_stamp,
            validated_indexes: Arc::default(),
        };
        index.validated_indexes = Arc::new(index.validate_index_records(bytes)?);
        Ok(index)
    }

    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            header: PackedHeader {
                entry_count: 0,
                head_id: 0,
                header_len: V4_HEADER_LEN,
            },
            footer: PackedFooter {
                entry_data_end: V4_HEADER_LEN,
                entry_offsets_offset: V4_HEADER_LEN,
                entry_offsets_count: 0,
                batch_offsets_offset: V4_HEADER_LEN,
                batch_offsets_count: 0,
                batch_dir_offset: V4_HEADER_LEN,
                batch_dir_count: 0,
                tx_key_bytes_offset: V4_HEADER_LEN,
                tx_key_bytes_len: 0,
                tx_dir_offset: V4_HEADER_LEN,
                tx_dir_count: 0,
                entry_count: 0,
                head_id: 0,
            },
            file_stamp: None,
            validated_indexes: Arc::default(),
        }
    }

    pub(crate) fn head_id(&self) -> u64 {
        self.header.head_id
    }

    pub(crate) fn matches_file_on_disk(&self) -> Result<bool> {
        let Some(expected) = self.file_stamp else {
            return Ok(false);
        };
        let file = File::open(&self.path)?;
        Ok(packed_file_stamp(&file)? == Some(expected))
    }

    pub(crate) fn last_entry(&self) -> Result<Option<OpEntry>> {
        let mut entries = self.recent_entries(1)?;
        Ok(entries.pop())
    }

    pub(crate) fn recent_entries(&self, count: usize) -> Result<Vec<OpEntry>> {
        if count == 0 || self.header.entry_count == 0 {
            return Ok(Vec::new());
        }
        let offsets = &self.validated_indexes.entry_offsets;
        let take = count.min(offsets.len());
        let mut file = File::open(&self.path)?;
        let mut out = Vec::with_capacity(take);
        for record in offsets.iter().rev().take(take) {
            out.push(read_entry_at(&mut file, record.entry_offset)?);
        }
        Ok(out)
    }

    pub(crate) fn entries_after(&self, since_head_id: u64) -> Result<Vec<OpEntry>> {
        let offsets = &self.validated_indexes.entry_offsets;
        let start = offsets.partition_point(|record| record.entry_id <= since_head_id);
        let mut file = File::open(&self.path)?;
        let mut out = Vec::with_capacity(offsets.len().saturating_sub(start));
        for record in &offsets[start..] {
            out.push(read_entry_at(&mut file, record.entry_offset)?);
        }
        Ok(out)
    }

    pub(crate) fn collect_batches_scoped(
        &self,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        self.collect_batches_after_scoped(0, count, predicate, scope)
    }

    pub(crate) fn collect_batches_after_scoped(
        &self,
        since_head_id: u64,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        if count == 0 || self.header.entry_count == 0 {
            return Ok(Vec::new());
        }

        let batch_offsets = &self.validated_indexes.batch_offsets;
        let batch_dir = &self.validated_indexes.batch_dir;
        let mut file = File::open(&self.path)?;
        let mut batches = Vec::new();

        for record in batch_dir {
            #[cfg(test)]
            BATCH_DIR_RECORDS_VISITED.with(|visits| visits.set(visits.get() + 1));
            if record.newest_entry_id <= since_head_id {
                // Fsck enforces newest-first order, so every remaining batch is also at/before it.
                break;
            }
            if let Some(scope) = scope
                && record.scope_state == ScopeState::None as u8
                && !scope.is_empty()
            {
                continue;
            }

            let first = usize::try_from(record.first_offset_index).map_err(|_| {
                HeddleError::InvalidObject("batch offset index too large".to_string())
            })?;
            let len = usize::try_from(record.entry_count).map_err(|_| {
                HeddleError::InvalidObject("batch entry count too large".to_string())
            })?;
            let end = first.checked_add(len).ok_or_else(|| {
                HeddleError::InvalidObject("oplog batch directory range overflows".to_string())
            })?;
            if end > batch_offsets.len() {
                return Err(HeddleError::InvalidObject(
                    "oplog batch directory range points outside offset list".to_string(),
                ));
            }
            let mut entries = Vec::with_capacity(len);
            for offset in &batch_offsets[first..end] {
                let entry = read_entry_at(&mut file, *offset)?;
                if entry.id > since_head_id {
                    entries.push(entry);
                }
            }
            if entries.is_empty() {
                continue;
            }
            entries.sort_by_key(|entry| entry.batch_index);
            let batch = OpBatch {
                id: record.batch_id,
                entries,
            };
            if let Some(scope) = scope
                && !batch
                    .entries
                    .iter()
                    .all(|entry| entry.scope.as_deref() == Some(scope))
            {
                continue;
            }
            if predicate(&batch) {
                batches.push(batch);
                if batches.len() == count {
                    break;
                }
            }
        }

        Ok(batches)
    }

    pub(crate) fn transaction_commit(&self, transaction_id: &str) -> Result<Option<(u64, u64)>> {
        let key_bytes = &self.validated_indexes.tx_key_bytes;
        let records = &self.validated_indexes.tx_dir;
        let needle = transaction_id.as_bytes();

        let mut left = 0;
        let mut right = records.len();
        while left < right {
            let mid = left + ((right - left) / 2);
            let key = tx_record_key(key_bytes, &records[mid])?;
            match key.cmp(needle) {
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
                std::cmp::Ordering::Equal => {
                    let record = &records[mid];
                    return Ok(Some((record.commit_entry_id, record.batch_id)));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn committed_batch_records(&self, transaction_id: &str) -> Result<Vec<OpRecord>> {
        let Some((_commit_entry_id, batch_id)) = self.transaction_commit(transaction_id)? else {
            return Ok(Vec::new());
        };
        let mut batches = self.collect_batches_scoped(1, |batch| batch.id == batch_id, None)?;
        let Some(batch) = batches.pop() else {
            return Ok(Vec::new());
        };
        Ok(batch
            .entries
            .into_iter()
            .filter(|entry| !super::oplog_types::is_transaction_commit(&entry.operation))
            .map(|entry| entry.operation)
            .collect())
    }

    pub(crate) fn committed_batch(&self, transaction_id: &str) -> Result<Option<OpBatch>> {
        let Some((_commit_entry_id, batch_id)) = self.transaction_commit(transaction_id)? else {
            return Ok(None);
        };
        Ok(self
            .collect_batches_scoped(1, |batch| batch.id == batch_id, None)?
            .pop())
    }

    pub(crate) fn append_entries(&self, new_entries: &[OpEntry]) -> Result<Self> {
        self.append_entries_inner(new_entries, true)
    }

    /// Rewrite the packed oplog as a materialized view of an independently
    /// durable commit artifact. The temp-file + rename keeps readers from
    /// observing a partial container, but deliberately omits fsync: recovery
    /// can reconstruct these entries from the authoritative snapshot pack.
    pub(crate) fn append_entries_reconstructible(&self, new_entries: &[OpEntry]) -> Result<Self> {
        self.append_entries_inner(new_entries, false)
    }

    fn append_entries_inner(&self, new_entries: &[OpEntry], durable: bool) -> Result<Self> {
        if new_entries.is_empty() {
            return Ok(self.clone());
        }
        // TODO(#423 follow-up): segmented/rollover append if write-amplification
        // becomes a ceiling on large logs.
        let new_head = new_entries
            .last()
            .map(|entry| entry.id)
            .unwrap_or(self.header.head_id);
        let new_count = self
            .header
            .entry_count
            .checked_add(new_entries.len() as u64)
            .ok_or_else(|| HeddleError::InvalidObject("oplog entry count overflow".to_string()))?;
        let mut tmp_new_entry_bytes = Vec::new();
        let mut new_entry_offsets = Vec::with_capacity(new_entries.len());
        let mut offset = self.footer.entry_data_end;
        for entry in new_entries {
            new_entry_offsets.push(EntryOffsetRecord {
                entry_id: entry.id,
                entry_offset: offset,
            });
            encode_entry(entry, &mut tmp_new_entry_bytes)?;
            let encoded_new_len = u64::try_from(tmp_new_entry_bytes.len()).map_err(|_| {
                HeddleError::InvalidObject("oplog entry stream too large".to_string())
            })?;
            offset = self
                .footer
                .entry_data_end
                .checked_add(encoded_new_len)
                .ok_or_else(|| {
                    HeddleError::InvalidObject("oplog entry stream too large".to_string())
                })?;
        }

        let mut old_offsets = self.validated_indexes.entry_offsets.clone();
        old_offsets.extend(new_entry_offsets);
        let old_batch_offsets = self.validated_indexes.batch_offsets.clone();
        let new_entries_by_offset = new_entries
            .iter()
            .zip(old_offsets[self.header.entry_count as usize..].iter())
            .map(|(entry, offset)| (entry.clone(), offset.entry_offset))
            .collect::<Vec<_>>();
        let batch_index = build_index_sections_from_existing(
            old_batch_offsets,
            &self.validated_indexes.batch_dir,
            &self.validated_indexes.tx_key_bytes,
            &self.validated_indexes.tx_dir,
            &new_entries_by_offset,
        )?;

        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        create_dir_all_durable(parent)?;
        let tmp = temp_path(&self.path);
        let strategy = if durable {
            ReconstructibleAppendStrategy::Rewrite
        } else {
            match try_reflink(&self.path, &tmp) {
                Ok(outcome) => reconstructible_append_strategy(outcome),
                Err(_) => ReconstructibleAppendStrategy::Rewrite,
            }
        };
        let write_result = match strategy {
            ReconstructibleAppendStrategy::CloneAndRewriteTail => self
                .write_appended_reflinked_tmp(
                    &tmp,
                    (new_count, new_head),
                    &tmp_new_entry_bytes,
                    &old_offsets,
                    &batch_index,
                ),
            ReconstructibleAppendStrategy::Rewrite => {
                let _ = std::fs::remove_file(&tmp);
                self.write_appended_tmp(
                    &tmp,
                    (new_count, new_head),
                    &tmp_new_entry_bytes,
                    &old_offsets,
                    &batch_index,
                    durable,
                )
            }
        };
        let footer = match write_result {
            Ok(footer) => footer,
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
        };
        std::fs::rename(&tmp, &self.path)?;
        if durable {
            sync_directory(parent)?;
            return Self::open_v4(&self.path);
        }

        // The snapshot pack is authoritative for this reconstructible view, so
        // avoid rereading and revalidating the whole oplog after publishing the
        // exact header/footer metadata that the writer just serialized. Durable
        // appends retain the full readback above.
        Ok(Self {
            path: self.path.clone(),
            header: PackedHeader {
                entry_count: new_count,
                head_id: new_head,
                header_len: V4_HEADER_LEN,
            },
            footer,
            file_stamp: packed_file_stamp(&File::open(&self.path)?)?,
            validated_indexes: Arc::new(ValidatedIndexSections {
                entry_offsets: old_offsets,
                batch_offsets: batch_index.batch_offsets,
                batch_dir: batch_index.batch_dir,
                tx_key_bytes: batch_index.tx_key_bytes,
                tx_dir: batch_index.tx_dir,
            }),
        })
    }

    /// Reuse the immutable historical entry prefix from a CoW clone, discard
    /// the old derived indexes/footer, and write only the new tail. The source
    /// oplog remains untouched; the completed temp still publishes through the
    /// same atomic rename as the ordinary rewrite path.
    fn write_appended_reflinked_tmp(
        &self,
        tmp: &Path,
        new_header: (u64, u64),
        new_entry_bytes: &[u8],
        entry_offsets: &[EntryOffsetRecord],
        batch_index: &BuiltIndexSections,
    ) -> Result<PackedFooter> {
        let (new_count, new_head) = new_header;
        let mut out = OpenOptions::new().read(true).write(true).open(tmp)?;
        out.set_len(self.footer.entry_data_end)?;
        out.seek(SeekFrom::Start(self.footer.entry_data_end))?;
        out.write_all(new_entry_bytes)?;
        let entry_data_end = out.stream_position()?;
        let footer = write_index_sections(
            &mut out,
            IndexWritePlan {
                entry_data_end,
                entry_offsets,
                batch_offsets: &batch_index.batch_offsets,
                batch_dir: &batch_index.batch_dir,
                tx_key_bytes: &batch_index.tx_key_bytes,
                tx_dir: &batch_index.tx_dir,
                entry_count: new_count,
                head_id: new_head,
            },
        )?;
        out.seek(SeekFrom::Start(0))?;
        write_header(&mut out, CURRENT_CONTAINER_VERSION, new_count, new_head)?;
        Ok(footer)
    }

    fn write_appended_tmp(
        &self,
        tmp: &Path,
        new_header: (u64, u64),
        new_entry_bytes: &[u8],
        entry_offsets: &[EntryOffsetRecord],
        batch_index: &BuiltIndexSections,
        durable: bool,
    ) -> Result<PackedFooter> {
        let (new_count, new_head) = new_header;
        let mut out = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(tmp)?;
        write_header(&mut out, CURRENT_CONTAINER_VERSION, new_count, new_head)?;

        let mut old = File::open(&self.path)?;
        old.seek(SeekFrom::Start(self.header.header_len))?;
        let old_entry_len = self.footer.entry_data_end - self.header.header_len;
        std::io::copy(&mut old.take(old_entry_len), &mut out)?;
        out.write_all(new_entry_bytes)?;

        let entry_data_end = out.stream_position()?;
        let footer = write_index_sections(
            &mut out,
            IndexWritePlan {
                entry_data_end,
                entry_offsets,
                batch_offsets: &batch_index.batch_offsets,
                batch_dir: &batch_index.batch_dir,
                tx_key_bytes: &batch_index.tx_key_bytes,
                tx_dir: &batch_index.tx_dir,
                entry_count: new_count,
                head_id: new_head,
            },
        )?;
        if durable {
            out.sync_all()?;
        }
        Ok(footer)
    }

    fn read_entry_offsets(&self) -> Result<Vec<EntryOffsetRecord>> {
        #[cfg(test)]
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(reads.get() + 1));
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.footer.entry_offsets_offset))?;
        let mut records = Vec::with_capacity(self.footer.entry_offsets_count as usize);
        for _ in 0..self.footer.entry_offsets_count {
            records.push(EntryOffsetRecord {
                entry_id: read_u64_from_file(&mut file)?,
                entry_offset: read_u64_from_file(&mut file)?,
            });
        }
        Ok(records)
    }

    fn read_batch_offsets(&self) -> Result<Vec<u64>> {
        #[cfg(test)]
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(reads.get() + 1));
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.footer.batch_offsets_offset))?;
        let mut offsets = Vec::with_capacity(self.footer.batch_offsets_count as usize);
        for _ in 0..self.footer.batch_offsets_count {
            offsets.push(read_u64_from_file(&mut file)?);
        }
        Ok(offsets)
    }

    fn read_batch_dir(&self) -> Result<Vec<BatchDirRecord>> {
        #[cfg(test)]
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(reads.get() + 1));
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.footer.batch_dir_offset))?;
        let mut records = Vec::with_capacity(self.footer.batch_dir_count as usize);
        for _ in 0..self.footer.batch_dir_count {
            let batch_id = read_u64_from_file(&mut file)?;
            let newest_entry_id = read_u64_from_file(&mut file)?;
            let first_offset_index = read_u64_from_file(&mut file)?;
            let entry_count = read_u32_from_file(&mut file)?;
            let scope_state = read_u8_from_file(&mut file)?;
            let _padding = read_array_from_file::<3>(&mut file)?;
            let _scope_key_off = read_u64_from_file(&mut file)?;
            let _scope_key_len = read_u32_from_file(&mut file)?;
            let _padding = read_array_from_file::<4>(&mut file)?;
            records.push(BatchDirRecord {
                batch_id,
                newest_entry_id,
                first_offset_index,
                entry_count,
                scope_state,
            });
        }
        Ok(records)
    }

    fn read_tx_key_bytes(&self) -> Result<Vec<u8>> {
        #[cfg(test)]
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(reads.get() + 1));
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.footer.tx_key_bytes_offset))?;
        let len = usize::try_from(self.footer.tx_key_bytes_len).map_err(|_| {
            HeddleError::InvalidObject("transaction key bytes section too large".to_string())
        })?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn read_tx_dir(&self) -> Result<Vec<TxDirRecord>> {
        #[cfg(test)]
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(reads.get() + 1));
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.footer.tx_dir_offset))?;
        let mut records = Vec::with_capacity(self.footer.tx_dir_count as usize);
        for _ in 0..self.footer.tx_dir_count {
            let key_offset = read_u64_from_file(&mut file)?;
            let key_len = read_u32_from_file(&mut file)?;
            let _padding = read_array_from_file::<4>(&mut file)?;
            let commit_entry_id = read_u64_from_file(&mut file)?;
            let batch_id = read_u64_from_file(&mut file)?;
            records.push(TxDirRecord {
                key_offset,
                key_len,
                commit_entry_id,
                batch_id,
            });
        }
        Ok(records)
    }

    fn validate_index_records(&self, bytes: &[u8]) -> Result<ValidatedIndexSections> {
        let offsets = self.read_entry_offsets()?;
        if offsets.len() as u64 != self.header.entry_count {
            return Err(HeddleError::InvalidObject(
                "oplog entry-offset count disagrees with header".to_string(),
            ));
        }
        if self.header.entry_count == 0 {
            if self.header.head_id != 0 {
                return Err(HeddleError::InvalidObject(
                    "empty oplog has non-zero head_id".to_string(),
                ));
            }
        } else if offsets.last().map(|record| record.entry_id) != Some(self.header.head_id) {
            return Err(HeddleError::InvalidObject(
                "oplog entry-offset tail disagrees with head_id".to_string(),
            ));
        }

        let mut last_id = None;
        for record in &offsets {
            if record.entry_offset < self.header.header_len
                || record.entry_offset >= self.footer.entry_data_end
            {
                return Err(HeddleError::InvalidObject(
                    "oplog entry offset points outside entry section".to_string(),
                ));
            }
            if last_id.is_some_and(|id| record.entry_id <= id) {
                return Err(HeddleError::InvalidObject(
                    "oplog entry offsets are not sorted by id".to_string(),
                ));
            }
            last_id = Some(record.entry_id);
        }

        let batch_offsets = self.read_batch_offsets()?;
        for offset in &batch_offsets {
            if *offset < self.header.header_len || *offset >= self.footer.entry_data_end {
                return Err(HeddleError::InvalidObject(
                    "oplog batch directory points outside entry section".to_string(),
                ));
            }
        }

        let batch_dir = self.read_batch_dir()?;
        let mut batch_offset_total = 0u64;
        let mut prev_newest = None;
        for record in &batch_dir {
            if prev_newest.is_some_and(|id| record.newest_entry_id >= id) {
                return Err(HeddleError::InvalidObject(
                    "oplog batch directory is not newest-first".to_string(),
                ));
            }
            prev_newest = Some(record.newest_entry_id);
            let end = record.first_offset_index + u64::from(record.entry_count);
            if end > self.footer.batch_offsets_count {
                return Err(HeddleError::InvalidObject(
                    "oplog batch directory range points outside offset list".to_string(),
                ));
            }
            batch_offset_total += u64::from(record.entry_count);
        }
        if batch_offset_total != self.footer.batch_offsets_count {
            return Err(HeddleError::InvalidObject(
                "oplog batch offset list disagrees with batch directory".to_string(),
            ));
        }

        let key_bytes = self.read_tx_key_bytes()?;
        let tx_dir = self.read_tx_dir()?;
        let offset_by_id = offsets
            .iter()
            .map(|record| (record.entry_id, record.entry_offset))
            .collect::<HashMap<_, _>>();
        let mut prev_key: Option<Vec<u8>> = None;
        for record in &tx_dir {
            let key = tx_record_key(&key_bytes, record)?;
            if prev_key.as_deref().is_some_and(|prev| key <= prev) {
                return Err(HeddleError::InvalidObject(
                    "oplog transaction directory is not sorted".to_string(),
                ));
            }
            prev_key = Some(key.to_vec());

            let Some(offset) = offset_by_id.get(&record.commit_entry_id) else {
                return Err(HeddleError::InvalidObject(
                    "oplog transaction directory references a missing entry".to_string(),
                ));
            };
            if *offset >= self.footer.entry_data_end {
                return Err(HeddleError::InvalidObject(
                    "oplog transaction directory points past entry data".to_string(),
                ));
            }
            let mut cursor =
                Cursor::new(&bytes[*offset as usize..self.footer.entry_data_end as usize]);
            let entry = parse_current_entry(&mut cursor)?;
            match &entry.operation {
                OpRecord::TransactionCommit { transaction_id, .. } => {
                    if transaction_id.as_bytes() != key {
                        return Err(HeddleError::InvalidObject(
                            "oplog transaction directory key disagrees with commit transaction_id"
                                .to_string(),
                        ));
                    }
                }
                OpRecord::Snapshot { .. }
                | OpRecord::Goto { .. }
                | OpRecord::ThreadCreate { .. }
                | OpRecord::ThreadDelete { .. }
                | OpRecord::ThreadUpdate { .. }
                | OpRecord::Fork { .. }
                | OpRecord::Collapse { .. }
                | OpRecord::MarkerCreate { .. }
                | OpRecord::MarkerDelete { .. }
                | OpRecord::Checkpoint { .. }
                | OpRecord::TransactionAbort { .. }
                | OpRecord::EphemeralThreadCollapse { .. }
                | OpRecord::ConflictResolved { .. }
                | OpRecord::Redact { .. }
                | OpRecord::Purge { .. }
                | OpRecord::FastForward { .. }
                | OpRecord::GitCheckpoint { .. }
                | OpRecord::RemoteThreadUpdate { .. }
                | OpRecord::RemoteThreadDelete { .. }
                | OpRecord::UndoRecoveryUpdate { .. }
                | OpRecord::StateVisibilitySet { .. }
                | OpRecord::StateVisibilityPromote { .. } => {
                    return Err(HeddleError::InvalidObject(
                        "oplog transaction directory references a non-commit entry".to_string(),
                    ));
                }
            }
        }
        Ok(ValidatedIndexSections {
            entry_offsets: offsets,
            batch_offsets,
            batch_dir,
            tx_key_bytes: key_bytes,
            tx_dir,
        })
    }
}

#[cfg(test)]
thread_local! {
    static BATCH_DIR_RECORDS_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static INDEX_SECTION_DISK_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl PackedFooter {
    fn parse(bytes: &[u8], header: &PackedHeader) -> Result<Self> {
        let file_len = bytes.len() as u64;
        if file_len < header.header_len + FOOTER_LEN {
            return Err(HeddleError::InvalidObject(
                "oplog missing index footer".to_string(),
            ));
        }
        let footer_start = file_len - FOOTER_LEN;
        let mut cursor = Cursor::new(&bytes[footer_start as usize..]);
        let magic = cursor.read_array::<8>()?;
        if &magic != INDEX_MAGIC {
            return Err(HeddleError::InvalidObject(
                "invalid oplog index magic".to_string(),
            ));
        }
        let index_version = cursor.read_u32()?;
        if index_version != INDEX_VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported oplog index version {index_version}"
            )));
        }
        let footer_len = cursor.read_u32()?;
        if footer_len != FOOTER_LEN as u32 {
            return Err(HeddleError::InvalidObject(
                "oplog index footer length mismatch".to_string(),
            ));
        }
        let footer = Self {
            entry_data_end: cursor.read_u64()?,
            entry_offsets_offset: cursor.read_u64()?,
            entry_offsets_count: cursor.read_u64()?,
            batch_offsets_offset: cursor.read_u64()?,
            batch_offsets_count: cursor.read_u64()?,
            batch_dir_offset: cursor.read_u64()?,
            batch_dir_count: cursor.read_u64()?,
            tx_key_bytes_offset: cursor.read_u64()?,
            tx_key_bytes_len: cursor.read_u64()?,
            tx_dir_offset: cursor.read_u64()?,
            tx_dir_count: cursor.read_u64()?,
            entry_count: cursor.read_u64()?,
            head_id: cursor.read_u64()?,
        };
        footer.validate(header, file_len, footer_start)?;
        Ok(footer)
    }

    fn validate(&self, header: &PackedHeader, file_len: u64, footer_start: u64) -> Result<()> {
        if self.entry_count != header.entry_count || self.head_id != header.head_id {
            return Err(HeddleError::InvalidObject(
                "oplog header/footer entry metadata disagreement".to_string(),
            ));
        }
        if self.entry_data_end < header.header_len || self.entry_data_end > footer_start {
            return Err(HeddleError::InvalidObject(
                "oplog entry section points outside file".to_string(),
            ));
        }
        if self.entry_offsets_count != header.entry_count {
            return Err(HeddleError::InvalidObject(
                "oplog footer entry count disagrees with offset table".to_string(),
            ));
        }

        let sections = [
            (
                self.entry_offsets_offset,
                self.entry_offsets_count,
                ENTRY_OFFSET_RECORD_LEN,
                "entry offsets",
            ),
            (
                self.batch_offsets_offset,
                self.batch_offsets_count,
                8,
                "batch offsets",
            ),
            (
                self.batch_dir_offset,
                self.batch_dir_count,
                BATCH_DIR_RECORD_LEN,
                "batch directory",
            ),
            (
                self.tx_key_bytes_offset,
                self.tx_key_bytes_len,
                1,
                "tx keys",
            ),
            (
                self.tx_dir_offset,
                self.tx_dir_count,
                TX_DIR_RECORD_LEN,
                "tx dir",
            ),
        ];
        for (offset, count, width, name) in sections {
            checked_section(offset, count, width, footer_start, name)?;
            if offset < self.entry_data_end && count > 0 {
                return Err(HeddleError::InvalidObject(format!(
                    "oplog {name} section overlaps entry data"
                )));
            }
        }
        if self.entry_data_end > self.entry_offsets_offset {
            return Err(HeddleError::InvalidObject(
                "oplog entry offsets start before entry data ends".to_string(),
            ));
        }
        if footer_start + FOOTER_LEN != file_len {
            return Err(HeddleError::InvalidObject(
                "oplog footer length disagrees with file length".to_string(),
            ));
        }
        Ok(())
    }
}

#[repr(u8)]
enum ScopeState {
    None = 0,
    One = 1,
    Mixed = 2,
}

struct BuiltIndexSections {
    batch_offsets: Vec<u64>,
    batch_dir: Vec<BatchDirRecord>,
    tx_key_bytes: Vec<u8>,
    tx_dir: Vec<TxDirRecord>,
}

fn load_latest(bytes: &[u8]) -> Result<OplogData> {
    decode_current_container(bytes)
}

/// How the recovered prefix was located.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryStrategy {
    /// The surviving end-of-file footer was intact: its recorded
    /// `entry_data_end` told us exactly where the complete entry stream ends,
    /// so the recovered prefix is precise rather than greedily re-derived.
    FooterGuided,
    /// The footer was unrecoverable; the complete-record prefix was re-derived
    /// by parsing entries forward and stopping at the first torn record.
    ForwardGreedy,
}

impl RecoveryStrategy {
    fn as_str(self) -> &'static str {
        match self {
            RecoveryStrategy::FooterGuided => "footer-guided",
            RecoveryStrategy::ForwardGreedy => "forward-greedy",
        }
    }
}

struct TruncatedTailRecovery {
    data: OplogData,
    original_entry_count: Option<u64>,
    damaged_byte_start: usize,
    damaged_byte_end: usize,
    strategy: RecoveryStrategy,
}

impl TruncatedTailRecovery {
    fn recovered_records(&self) -> u64 {
        self.data.entries.len() as u64
    }

    fn lost_records(&self) -> Option<u64> {
        self.original_entry_count
            .map(|count| count.saturating_sub(self.recovered_records()))
    }
}

/// Structured outcome of an explicit (operator-invoked) or auto salvage.
///
/// Reuses the exact recovery planning that the auto-fallback path runs, so the
/// `heddle oplog recover` operator entrypoint reports precisely what the
/// silent fallback would have done — no second implementation.
#[derive(Clone, Debug)]
pub struct OplogRecoveryReport {
    /// True when the file parsed cleanly and no salvage was needed *this run*.
    /// May still carry sidecar-derived numbers from a prior recovery — see
    /// [`prior_recovery`](Self::prior_recovery).
    pub already_healthy: bool,
    /// True when the reported numbers come from a `.oplog.recovery` sidecar
    /// left by an EARLIER recovery (e.g. the silent auto-fallback ran first)
    /// rather than from a salvage performed by this call.
    pub prior_recovery: bool,
    /// Which strategy located the recovered prefix (`None` when no recovery is
    /// known): `footer-guided` or `forward-greedy`.
    pub strategy: Option<String>,
    /// Complete records kept.
    pub entries_recovered: u64,
    /// Records the original header claimed but that could not be salvaged.
    /// `None` when the original count was itself unreadable.
    pub entries_lost: Option<u64>,
    /// First byte of the damaged tail (the truncation/tear offset).
    pub damaged_byte_start: u64,
    /// One-past-the-last damaged byte (original file length).
    pub damaged_byte_end: u64,
    /// Where the damaged original was quarantined (`None` when none this run).
    pub quarantine_path: Option<PathBuf>,
    /// Where the recovery sidecar lives (`None` when no recovery is known).
    pub sidecar_path: Option<PathBuf>,
}

impl OplogRecoveryReport {
    /// A report for an oplog that parsed cleanly with no known prior recovery.
    pub(crate) fn healthy() -> Self {
        Self {
            already_healthy: true,
            prior_recovery: false,
            strategy: None,
            entries_recovered: 0,
            entries_lost: None,
            damaged_byte_start: 0,
            damaged_byte_end: 0,
            quarantine_path: None,
            sidecar_path: None,
        }
    }

    /// Build an `already_healthy` report from a `.oplog.recovery` sidecar left
    /// by a prior recovery, so the operator still sees the full salvage detail
    /// even when the silent auto-fallback ran before they invoked `recover`.
    /// Returns `None` if no readable, well-formed sidecar exists.
    pub fn from_prior_sidecar(oplog_path: &Path) -> Option<Self> {
        let sidecar_path = recovery_sidecar_path(oplog_path);
        let contents = std::fs::read_to_string(&sidecar_path).ok()?;
        let mut fields: HashMap<&str, &str> = HashMap::new();
        for line in contents.lines() {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim(), value.trim());
            }
        }
        let strategy = fields.get("strategy").map(|s| s.to_string());
        let entries_recovered = fields.get("entries_recovered")?.parse().ok()?;
        let entries_lost = fields
            .get("entries_lost")
            .and_then(|raw| raw.parse::<u64>().ok());
        let damaged_byte_start = fields
            .get("truncation_offset")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        let damaged_byte_end = fields
            .get("damaged_byte_end")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        Some(Self {
            already_healthy: true,
            prior_recovery: true,
            strategy,
            entries_recovered,
            entries_lost,
            damaged_byte_start,
            damaged_byte_end,
            quarantine_path: None,
            sidecar_path: Some(sidecar_path),
        })
    }
}

fn recover_truncated_latest(
    path: &Path,
    bytes: &[u8],
    source_error: &HeddleError,
) -> Result<Option<OplogData>> {
    let Some(recovery) = plan_truncated_latest_recovery(bytes, source_error)? else {
        return Ok(None);
    };

    let mut recovered_bytes = Vec::new();
    encode_current_container(&recovery.data, &mut recovered_bytes)?;
    let corrupt_path = next_corrupt_path(path);
    std::fs::rename(path, &corrupt_path)?;
    write_file_atomic(path, &recovered_bytes)?;
    write_recovery_sidecar(path, &recovery)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }

    emit_truncated_oplog_recovery_warning(path, &corrupt_path, &recovery);
    Ok(Some(recovery.data))
}

/// Operator entrypoint: explicitly run the salvage path and report what it did.
///
/// Routes through the same [`plan_truncated_latest_recovery`] +
/// [`recover_truncated_latest`] machinery the silent auto-fallback uses, so the
/// reported numbers always match what `load()`/`ensure_current()` would do on
/// their own. Returns an `already_healthy` report (no side effects) when the
/// oplog parses cleanly.
pub(crate) fn recover_oplog_at(path: &Path) -> Result<OplogRecoveryReport> {
    let bytes = std::fs::read(path)?;
    let source_error = match load_latest(&bytes) {
        Ok(_) => {
            // Healthy this run. If an earlier recovery (e.g. the silent
            // auto-fallback) already salvaged it, surface that sidecar's detail
            // rather than a bare "nothing to recover".
            return Ok(OplogRecoveryReport::from_prior_sidecar(path)
                .unwrap_or_else(OplogRecoveryReport::healthy));
        }
        Err(err) => err,
    };

    let Some(recovery) = plan_truncated_latest_recovery(&bytes, &source_error)? else {
        // Not a truncation-shaped failure: surface the original error rather
        // than silently claiming a healthy oplog.
        return Err(source_error);
    };

    let mut recovered_bytes = Vec::new();
    encode_current_container(&recovery.data, &mut recovered_bytes)?;
    let corrupt_path = next_corrupt_path(path);
    std::fs::rename(path, &corrupt_path)?;
    write_file_atomic(path, &recovered_bytes)?;
    let sidecar_path = write_recovery_sidecar(path, &recovery)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    emit_truncated_oplog_recovery_warning(path, &corrupt_path, &recovery);

    Ok(OplogRecoveryReport {
        already_healthy: false,
        prior_recovery: false,
        strategy: Some(recovery.strategy.as_str().to_string()),
        entries_recovered: recovery.recovered_records(),
        entries_lost: recovery.lost_records(),
        damaged_byte_start: recovery.damaged_byte_start as u64,
        damaged_byte_end: recovery.damaged_byte_end as u64,
        quarantine_path: Some(corrupt_path),
        sidecar_path: Some(sidecar_path),
    })
}

fn plan_truncated_latest_recovery(
    bytes: &[u8],
    source_error: &HeddleError,
) -> Result<Option<TruncatedTailRecovery>> {
    if !is_truncation_shaped_error(source_error) {
        return Ok(None);
    }

    let (header, mut cursor) = match parse_header_with_cursor(bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            if !is_truncation_shaped_error(&err)
                || bytes.len() > V4_HEADER_LEN as usize
                || !(MAGIC.starts_with(bytes) || bytes.starts_with(MAGIC))
            {
                return Ok(None);
            }
            return Ok(Some(TruncatedTailRecovery {
                data: OplogData {
                    entries: Vec::new(),
                    head_id: 0,
                },
                original_entry_count: None,
                damaged_byte_start: 0,
                damaged_byte_end: bytes.len(),
                strategy: RecoveryStrategy::ForwardGreedy,
            }));
        }
    };

    // Footer-guided first: when only the trailing bytes (the index sections or
    // a torn tail after the footer) are damaged but an intact footer survives,
    // its recorded `entry_data_end` frames the complete prefix EXACTLY. That is
    // more precise than re-deriving the boundary forward, so try it before the
    // forward-greedy fallback below.
    if let Some(recovery) = scan_footer_guided_recovery(bytes, &header, header.header_len as usize)
    {
        return Ok(Some(recovery));
    }

    let mut entries = Vec::new();
    let mut damaged_byte_start = cursor.offset;
    for _ in 0..header.entry_count {
        let entry_start = cursor.offset;
        match parse_current_entry(&mut cursor) {
            Ok(entry) => {
                damaged_byte_start = cursor.offset;
                entries.push(entry);
            }
            Err(err) if is_truncation_shaped_error(&err) => {
                damaged_byte_start = entry_start;
                break;
            }
            Err(err) => return Err(err),
        }
    }

    if entries.len() as u64 == header.entry_count {
        damaged_byte_start = cursor.offset;
    }

    let head_id = entries.last().map(|entry| entry.id).unwrap_or(0);
    Ok(Some(TruncatedTailRecovery {
        data: OplogData { entries, head_id },
        original_entry_count: Some(header.entry_count),
        damaged_byte_start,
        damaged_byte_end: bytes.len(),
        strategy: RecoveryStrategy::ForwardGreedy,
    }))
}

/// Scan backward from EOF for the 8-byte footer magic and, on the first intact
/// footer that frames a valid entry stream, recover footer-guided.
///
/// "Intact" means: correct magic, index version, and footer length; metadata
/// that agrees with the header; and an `entry_data_end` that is a real entry
/// boundary — i.e. exactly `header.entry_count` records parse and end precisely
/// at `entry_data_end`. When found, the damaged tail is everything after
/// `entry_data_end` (the torn index/footer region). Returns `None` when no
/// surviving footer frames a clean prefix, so the caller falls through to the
/// forward-greedy strategy.
fn scan_footer_guided_recovery(
    bytes: &[u8],
    header: &PackedHeader,
    entries_start: usize,
) -> Option<TruncatedTailRecovery> {
    let footer_len = FOOTER_LEN as usize;
    if bytes.len() < entries_start + footer_len {
        return None;
    }
    // Highest offset at which a full footer could still begin.
    let max_start = bytes.len() - footer_len;
    let mut candidate = max_start;
    loop {
        if bytes[candidate..candidate + INDEX_MAGIC.len()] == *INDEX_MAGIC
            && let Some(recovery) = try_footer_guided_at(bytes, header, entries_start, candidate)
        {
            return Some(recovery);
        }
        if candidate == entries_start {
            return None;
        }
        candidate -= 1;
    }
}

/// Attempt a footer-guided recovery using the footer that starts at
/// `footer_start`. Returns `None` unless the footer parses, agrees with the
/// header, and its `entry_data_end` is exactly the end of `entry_count`
/// well-formed records.
fn try_footer_guided_at(
    bytes: &[u8],
    header: &PackedHeader,
    entries_start: usize,
    footer_start: usize,
) -> Option<TruncatedTailRecovery> {
    let footer = parse_footer_at(bytes, footer_start).ok()?;

    // The recorded boundary must agree with the header and sit inside the
    // entry region (after the header, at or before this footer).
    if footer.entry_count != header.entry_count || footer.head_id != header.head_id {
        return None;
    }
    let entry_data_end = usize::try_from(footer.entry_data_end).ok()?;
    if entry_data_end < entries_start || entry_data_end > footer_start {
        return None;
    }

    // The footer is only trustworthy if its boundary is a real record boundary:
    // exactly `entry_count` records parse and consume up to `entry_data_end`.
    let mut cursor = Cursor::new(&bytes[entries_start..entry_data_end]);
    let mut entries = Vec::with_capacity(header.entry_count as usize);
    for _ in 0..header.entry_count {
        match parse_current_entry(&mut cursor) {
            Ok(entry) => entries.push(entry),
            Err(_) => return None,
        }
    }
    if cursor.offset != entry_data_end - entries_start {
        return None;
    }

    let head_id = entries.last().map(|entry| entry.id).unwrap_or(0);
    Some(TruncatedTailRecovery {
        data: OplogData { entries, head_id },
        original_entry_count: Some(header.entry_count),
        damaged_byte_start: entry_data_end,
        damaged_byte_end: bytes.len(),
        strategy: RecoveryStrategy::FooterGuided,
    })
}

/// Parse a footer that begins at `footer_start` (not necessarily at EOF),
/// validating only magic / index version / footer length. Field-level
/// agreement with the header and the entry stream is checked by the caller.
fn parse_footer_at(bytes: &[u8], footer_start: usize) -> Result<PackedFooter> {
    let footer_len = FOOTER_LEN as usize;
    if footer_start + footer_len > bytes.len() {
        return Err(HeddleError::InvalidObject(
            "oplog footer past end of file".to_string(),
        ));
    }
    let mut cursor = Cursor::new(&bytes[footer_start..footer_start + footer_len]);
    let magic = cursor.read_array::<8>()?;
    if &magic != INDEX_MAGIC {
        return Err(HeddleError::InvalidObject(
            "invalid oplog index magic".to_string(),
        ));
    }
    let index_version = cursor.read_u32()?;
    if index_version != INDEX_VERSION {
        return Err(HeddleError::InvalidObject(format!(
            "unsupported oplog index version {index_version}"
        )));
    }
    let footer_len_field = cursor.read_u32()?;
    if footer_len_field != FOOTER_LEN as u32 {
        return Err(HeddleError::InvalidObject(
            "oplog index footer length mismatch".to_string(),
        ));
    }
    Ok(PackedFooter {
        entry_data_end: cursor.read_u64()?,
        entry_offsets_offset: cursor.read_u64()?,
        entry_offsets_count: cursor.read_u64()?,
        batch_offsets_offset: cursor.read_u64()?,
        batch_offsets_count: cursor.read_u64()?,
        batch_dir_offset: cursor.read_u64()?,
        batch_dir_count: cursor.read_u64()?,
        tx_key_bytes_offset: cursor.read_u64()?,
        tx_key_bytes_len: cursor.read_u64()?,
        tx_dir_offset: cursor.read_u64()?,
        tx_dir_count: cursor.read_u64()?,
        entry_count: cursor.read_u64()?,
        head_id: cursor.read_u64()?,
    })
}

/// The sidecar filename suffix written next to `oplog.bin` after a recovery.
const RECOVERY_SIDECAR_SUFFIX: &str = ".oplog.recovery";

/// Write the `.oplog.recovery` sidecar recording that a salvage happened.
///
/// ADDITIVE alongside the `.corrupt` quarantine and the `state_corrupted`
/// eprintln — gives tooling/operators a durable, machine-readable marker that a
/// recovery occurred (truncation offset, counts, strategy, timestamp). The
/// sidecar is named for the oplog file (`oplog.bin` → `oplog.bin.oplog.recovery`)
/// and is overwritten on each recovery so it always reflects the latest event.
fn write_recovery_sidecar(path: &Path, recovery: &TruncatedTailRecovery) -> Result<PathBuf> {
    let sidecar_path = recovery_sidecar_path(path);
    let lost = recovery
        .lost_records()
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = Utc::now().to_rfc3339();
    let contents = format!(
        "schema=1\n\
         strategy={}\n\
         truncation_offset={}\n\
         damaged_byte_end={}\n\
         entries_recovered={}\n\
         entries_lost={}\n\
         recovered_at={}\n",
        recovery.strategy.as_str(),
        recovery.damaged_byte_start,
        recovery.damaged_byte_end,
        recovery.recovered_records(),
        lost,
        timestamp,
    );
    write_file_atomic(&sidecar_path, contents.as_bytes())?;
    Ok(sidecar_path)
}

fn recovery_sidecar_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{RECOVERY_SIDECAR_SUFFIX}", path.display()))
}

fn is_truncation_shaped_error(err: &HeddleError) -> bool {
    match err {
        HeddleError::InvalidObject(message) => {
            message.contains("oplog truncated")
                || message.contains("oplog missing index footer")
                || message.contains("invalid oplog index magic")
                || message.contains("oplog index footer length mismatch")
                || message.contains("oplog entry/index boundary disagreement")
                || message.contains("oplog entry section points outside file")
                || message.contains("oplog footer length disagrees with file length")
                || message.contains("section points outside file")
        }
        _ => false,
    }
}

fn next_corrupt_path(path: &Path) -> PathBuf {
    let base = PathBuf::from(format!("{}.corrupt", path.display()));
    if !base.exists() {
        return base;
    }
    for index in 1.. {
        let candidate = PathBuf::from(format!("{}.corrupt.{index}", path.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded corrupt-path suffix search should always return")
}

fn emit_truncated_oplog_recovery_warning(
    path: &Path,
    corrupt_path: &Path,
    recovery: &TruncatedTailRecovery,
) {
    let recovered_records = recovery.recovered_records();
    let lost_records = recovery
        .lost_records()
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    eprintln!(
        "Warning: kind=state_corrupted error=\"Packed oplog was truncated; recovered complete records\" path={} quarantined={} strategy={} recovered_records={} lost_records={} damaged_byte_range={}..{} hint=\"Heddle kept complete oplog records, moved the damaged tail to .corrupt, wrote a .oplog.recovery sidecar, and rebuilt oplog.bin; full fsck-style recovery remains a manual follow-up.\"",
        path.display(),
        corrupt_path.display(),
        recovery.strategy.as_str(),
        recovered_records,
        lost_records,
        recovery.damaged_byte_start,
        recovery.damaged_byte_end,
    );
}

fn encode_current_container(data: &OplogData, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    write_header_to_vec(
        out,
        CURRENT_CONTAINER_VERSION,
        data.entries.len() as u64,
        data.head_id,
    );
    let mut entry_offsets = Vec::with_capacity(data.entries.len());
    for entry in &data.entries {
        entry_offsets.push(EntryOffsetRecord {
            entry_id: entry.id,
            entry_offset: out.len() as u64,
        });
        encode_entry(entry, out)?;
    }
    let entry_data_end = out.len() as u64;
    let batch_index = build_index_sections(
        data.entries
            .iter()
            .cloned()
            .zip(entry_offsets.iter().copied()),
    )?;
    write_index_sections_to_vec(
        out,
        IndexWritePlan {
            entry_data_end,
            entry_offsets: &entry_offsets,
            batch_offsets: &batch_index.batch_offsets,
            batch_dir: &batch_index.batch_dir,
            tx_key_bytes: &batch_index.tx_key_bytes,
            tx_dir: &batch_index.tx_dir,
            entry_count: data.entries.len() as u64,
            head_id: data.head_id,
        },
    );
    Ok(())
}

struct IndexWritePlan<'a> {
    entry_data_end: u64,
    entry_offsets: &'a [EntryOffsetRecord],
    batch_offsets: &'a [u64],
    batch_dir: &'a [BatchDirRecord],
    tx_key_bytes: &'a [u8],
    tx_dir: &'a [TxDirRecord],
    entry_count: u64,
    head_id: u64,
}

fn write_index_sections<W: Write + Seek>(
    out: &mut W,
    plan: IndexWritePlan<'_>,
) -> Result<PackedFooter> {
    let entry_offsets_offset = out.stream_position()?;
    for record in plan.entry_offsets {
        out.write_all(&record.entry_id.to_le_bytes())?;
        out.write_all(&record.entry_offset.to_le_bytes())?;
    }
    let batch_offsets_offset = out.stream_position()?;
    for offset in plan.batch_offsets {
        out.write_all(&offset.to_le_bytes())?;
    }
    let batch_dir_offset = out.stream_position()?;
    for record in plan.batch_dir {
        write_batch_dir_record(out, record)?;
    }
    let tx_key_bytes_offset = out.stream_position()?;
    out.write_all(plan.tx_key_bytes)?;
    let tx_dir_offset = out.stream_position()?;
    for record in plan.tx_dir {
        write_tx_dir_record(out, record)?;
    }
    let footer = PackedFooter {
        entry_data_end: plan.entry_data_end,
        entry_offsets_offset,
        entry_offsets_count: plan.entry_offsets.len() as u64,
        batch_offsets_offset,
        batch_offsets_count: plan.batch_offsets.len() as u64,
        batch_dir_offset,
        batch_dir_count: plan.batch_dir.len() as u64,
        tx_key_bytes_offset,
        tx_key_bytes_len: plan.tx_key_bytes.len() as u64,
        tx_dir_offset,
        tx_dir_count: plan.tx_dir.len() as u64,
        entry_count: plan.entry_count,
        head_id: plan.head_id,
    };
    write_footer(out, &footer)?;
    Ok(footer)
}

fn write_index_sections_to_vec(out: &mut Vec<u8>, plan: IndexWritePlan<'_>) {
    let entry_offsets_offset = out.len() as u64;
    for record in plan.entry_offsets {
        out.extend_from_slice(&record.entry_id.to_le_bytes());
        out.extend_from_slice(&record.entry_offset.to_le_bytes());
    }
    let batch_offsets_offset = out.len() as u64;
    for offset in plan.batch_offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    let batch_dir_offset = out.len() as u64;
    for record in plan.batch_dir {
        write_batch_dir_record_to_vec(out, record);
    }
    let tx_key_bytes_offset = out.len() as u64;
    out.extend_from_slice(plan.tx_key_bytes);
    let tx_dir_offset = out.len() as u64;
    for record in plan.tx_dir {
        write_tx_dir_record_to_vec(out, record);
    }
    write_footer_to_vec(
        out,
        &PackedFooter {
            entry_data_end: plan.entry_data_end,
            entry_offsets_offset,
            entry_offsets_count: plan.entry_offsets.len() as u64,
            batch_offsets_offset,
            batch_offsets_count: plan.batch_offsets.len() as u64,
            batch_dir_offset,
            batch_dir_count: plan.batch_dir.len() as u64,
            tx_key_bytes_offset,
            tx_key_bytes_len: plan.tx_key_bytes.len() as u64,
            tx_dir_offset,
            tx_dir_count: plan.tx_dir.len() as u64,
            entry_count: plan.entry_count,
            head_id: plan.head_id,
        },
    );
}

fn build_index_sections(
    entries: impl IntoIterator<Item = (OpEntry, EntryOffsetRecord)>,
) -> Result<BuiltIndexSections> {
    let mut groups: BTreeMap<u64, Vec<(OpEntry, u64)>> = BTreeMap::new();
    let mut tx_first: BTreeMap<Vec<u8>, (u64, u64)> = BTreeMap::new();

    for (entry, offset) in entries {
        let batch_id = effective_batch_id(&entry);
        if let OpRecord::TransactionCommit { transaction_id, .. } = &entry.operation {
            tx_first
                .entry(transaction_id.as_bytes().to_vec())
                .or_insert((entry.id, batch_id));
        }
        groups
            .entry(batch_id)
            .or_default()
            .push((entry, offset.entry_offset));
    }

    let mut batch_records = groups
        .into_iter()
        .map(|(batch_id, mut entries)| {
            entries.sort_by_key(|(entry, _)| (entry.batch_index, entry.id));
            let newest_entry_id = entries
                .iter()
                .map(|(entry, _)| entry.id)
                .max()
                .unwrap_or_default();
            let scope_state = scope_state(entries.iter().map(|(entry, _)| entry.scope.as_deref()));
            (batch_id, newest_entry_id, scope_state, entries)
        })
        .collect::<Vec<_>>();
    batch_records.sort_by_key(|record| Reverse(record.1));

    let mut batch_offsets = Vec::new();
    let mut batch_dir = Vec::with_capacity(batch_records.len());
    for (batch_id, newest_entry_id, scope_state, entries) in batch_records {
        let first_offset_index = batch_offsets.len() as u64;
        for (_entry, offset) in &entries {
            batch_offsets.push(*offset);
        }
        batch_dir.push(BatchDirRecord {
            batch_id,
            newest_entry_id,
            first_offset_index,
            entry_count: entries.len() as u32,
            scope_state,
        });
    }

    let mut tx_key_bytes = Vec::new();
    let mut tx_dir = Vec::with_capacity(tx_first.len());
    for (key, (commit_entry_id, batch_id)) in tx_first {
        let key_offset = tx_key_bytes.len() as u64;
        tx_key_bytes.extend_from_slice(&key);
        tx_dir.push(TxDirRecord {
            key_offset,
            key_len: key.len() as u32,
            commit_entry_id,
            batch_id,
        });
    }

    Ok(BuiltIndexSections {
        batch_offsets,
        batch_dir,
        tx_key_bytes,
        tx_dir,
    })
}

fn build_index_sections_from_existing(
    mut old_batch_offsets: Vec<u64>,
    old_batch_dir: &[BatchDirRecord],
    old_tx_key_bytes: &[u8],
    old_tx_dir: &[TxDirRecord],
    new_entries: &[(OpEntry, u64)],
) -> Result<BuiltIndexSections> {
    let mut batch_groups: BTreeMap<u64, Vec<(OpEntry, u64)>> = BTreeMap::new();
    for (entry, offset) in new_entries {
        batch_groups
            .entry(effective_batch_id(entry))
            .or_default()
            .push((entry.clone(), *offset));
    }

    let mut batch_dir = old_batch_dir.to_vec();
    for (batch_id, mut entries) in batch_groups {
        entries.sort_by_key(|(entry, _)| (entry.batch_index, entry.id));
        let newest_entry_id = entries
            .iter()
            .map(|(entry, _)| entry.id)
            .max()
            .unwrap_or_default();
        let first_offset_index = old_batch_offsets.len() as u64;
        for (_entry, offset) in &entries {
            old_batch_offsets.push(*offset);
        }
        let record = BatchDirRecord {
            batch_id,
            newest_entry_id,
            first_offset_index,
            entry_count: entries.len() as u32,
            scope_state: scope_state(entries.iter().map(|(entry, _)| entry.scope.as_deref())),
        };
        let insert_at =
            batch_dir.partition_point(|existing| existing.newest_entry_id > newest_entry_id);
        batch_dir.insert(insert_at, record);
    }

    let mut tx_key_bytes = old_tx_key_bytes.to_vec();
    let mut tx_dir = old_tx_dir.to_vec();
    for (entry, _offset) in new_entries {
        if let OpRecord::TransactionCommit { transaction_id, .. } = &entry.operation {
            let key = transaction_id.as_bytes();
            if let Err(insert_at) = search_tx_dir(&tx_key_bytes, &tx_dir, key)? {
                let key_offset = tx_key_bytes.len() as u64;
                tx_key_bytes.extend_from_slice(key);
                tx_dir.insert(
                    insert_at,
                    TxDirRecord {
                        key_offset,
                        key_len: key.len() as u32,
                        commit_entry_id: entry.id,
                        batch_id: effective_batch_id(entry),
                    },
                );
            }
        }
    }

    Ok(BuiltIndexSections {
        batch_offsets: old_batch_offsets,
        batch_dir,
        tx_key_bytes,
        tx_dir,
    })
}

fn search_tx_dir(
    key_bytes: &[u8],
    records: &[TxDirRecord],
    needle: &[u8],
) -> Result<std::result::Result<usize, usize>> {
    let mut left = 0;
    let mut right = records.len();
    while left < right {
        let mid = left + ((right - left) / 2);
        match tx_record_key(key_bytes, &records[mid])?.cmp(needle) {
            std::cmp::Ordering::Less => left = mid + 1,
            std::cmp::Ordering::Greater => right = mid,
            std::cmp::Ordering::Equal => return Ok(Ok(mid)),
        }
    }
    Ok(Err(left))
}

fn scope_state<'a>(scopes: impl Iterator<Item = Option<&'a str>>) -> u8 {
    let mut first = None;
    for scope in scopes {
        match (first, scope) {
            (None, None) => first = Some(None),
            (None, Some(value)) => first = Some(Some(value)),
            (Some(prev), current) if prev == current => {}
            _ => return ScopeState::Mixed as u8,
        }
    }
    match first {
        Some(None) | None => ScopeState::None as u8,
        Some(Some(_)) => ScopeState::One as u8,
    }
}

#[cfg(test)]
fn collect_batches_from_entries(
    entries: impl Iterator<Item = OpEntry>,
    count: usize,
    predicate: impl Fn(&OpBatch) -> bool,
    scope: Option<&str>,
) -> Vec<OpBatch> {
    if count == 0 {
        return Vec::new();
    }

    struct PendingBatch {
        entries: Vec<OpEntry>,
        scope_matches: bool,
    }

    let mut batch_order = Vec::new();
    let mut pending: HashMap<u64, PendingBatch> = HashMap::new();

    for entry in entries {
        let batch_id = effective_batch_id(&entry);
        let batch = pending.entry(batch_id).or_insert_with(|| {
            batch_order.push(batch_id);
            PendingBatch {
                entries: Vec::new(),
                scope_matches: true,
            }
        });
        if let Some(scope) = scope
            && entry.scope.as_deref() != Some(scope)
        {
            batch.scope_matches = false;
        }
        batch.entries.push(entry);
    }

    let mut batches = Vec::new();
    for batch_id in batch_order {
        let Some(mut pending_batch) = pending.remove(&batch_id) else {
            continue;
        };
        if !pending_batch.scope_matches {
            continue;
        }

        pending_batch.entries.reverse();
        pending_batch.entries.sort_by_key(|entry| entry.batch_index);
        let batch = OpBatch {
            id: batch_id,
            entries: pending_batch.entries,
        };
        if predicate(&batch) {
            batches.push(batch);
            if batches.len() == count {
                break;
            }
        }
    }
    batches
}

fn effective_batch_id(entry: &OpEntry) -> u64 {
    if entry.batch_id == 0 {
        entry.id
    } else {
        entry.batch_id
    }
}

fn tx_record_key<'a>(key_bytes: &'a [u8], record: &TxDirRecord) -> Result<&'a [u8]> {
    let start = usize::try_from(record.key_offset)
        .map_err(|_| HeddleError::InvalidObject("transaction key offset too large".to_string()))?;
    let len = usize::try_from(record.key_len)
        .map_err(|_| HeddleError::InvalidObject("transaction key length too large".to_string()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| HeddleError::InvalidObject("transaction key range overflow".to_string()))?;
    if end > key_bytes.len() {
        return Err(HeddleError::InvalidObject(
            "transaction key range points outside key section".to_string(),
        ));
    }
    Ok(&key_bytes[start..end])
}

fn checked_section(
    offset: u64,
    count: u64,
    width: u64,
    footer_start: u64,
    name: &str,
) -> Result<()> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| HeddleError::InvalidObject(format!("oplog {name} section overflow")))?;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| HeddleError::InvalidObject(format!("oplog {name} section overflow")))?;
    if offset > footer_start || end > footer_start {
        return Err(HeddleError::InvalidObject(format!(
            "oplog {name} section points outside file"
        )));
    }
    Ok(())
}

fn parse_header(bytes: &[u8]) -> Result<PackedHeader> {
    let (header, _cursor) = parse_header_with_cursor(bytes)?;
    Ok(header)
}

fn parse_header_with_cursor(bytes: &[u8]) -> Result<(PackedHeader, Cursor<'_>)> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.read_array::<8>()?;
    if &magic != MAGIC {
        return Err(HeddleError::InvalidObject(
            "invalid oplog magic".to_string(),
        ));
    }
    let version = cursor.read_u32()?;
    validate_container_version(version)?;
    validate_op_record_schema_version(cursor.read_u32()?)?;
    let entry_count = cursor.read_u64()?;
    let head_id = cursor.read_u64()?;
    Ok((
        PackedHeader {
            entry_count,
            head_id,
            header_len: V4_HEADER_LEN,
        },
        cursor,
    ))
}

fn read_header(path: &Path) -> Result<PackedHeader> {
    // Read only the largest supported fixed header, never the whole file: this
    // path backs the O(1) `head_id`/`validate_header` reconciliation checks.
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(V4_HEADER_LEN as usize);
    file.take(V4_HEADER_LEN).read_to_end(&mut bytes)?;
    if (bytes.len() as u64) < LEGACY_HEADER_LEN {
        return Err(HeddleError::InvalidObject("oplog truncated".to_string()));
    }
    parse_header(&bytes)
}

fn parse_current_entries(cursor: &mut Cursor<'_>, entry_count: usize) -> Result<Vec<OpEntry>> {
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        entries.push(parse_current_entry(cursor)?);
    }
    Ok(entries)
}

fn parse_current_entry(cursor: &mut Cursor<'_>) -> Result<OpEntry> {
    let id = cursor.read_u64()?;
    let batch_id = cursor.read_u64()?;
    let batch_index = cursor.read_u32()?;
    let timestamp_secs = cursor.read_i64()?;
    let timestamp_ns = cursor.read_u32()?;
    let undone = cursor.read_u8()? != 0;

    let scope_len = cursor.read_u16()? as usize;
    let scope_bytes = cursor.read_bytes(scope_len)?;
    let scope = if scope_bytes.is_empty() {
        None
    } else {
        Some(
            String::from_utf8(scope_bytes)
                .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in scope".to_string()))?,
        )
    };

    let op_data_len = cursor.read_u32()? as usize;
    let op_data = cursor.read_bytes(op_data_len)?;
    let operation = decode_current_record(&op_data)?;

    let actor_name_len = cursor.read_u16()? as usize;
    let actor_name = String::from_utf8(cursor.read_bytes(actor_name_len)?)
        .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in actor.name".to_string()))?;
    let actor_email_len = cursor.read_u16()? as usize;
    let actor_email = String::from_utf8(cursor.read_bytes(actor_email_len)?)
        .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in actor.email".to_string()))?;
    let actor = std::sync::Arc::new(objects::object::Principal {
        name: actor_name,
        email: actor_email,
    });
    let operation_id_tag = cursor.read_u8()?;
    let operation_id = match operation_id_tag {
        0 => None,
        1 => {
            let bytes = cursor.read_array::<16>()?;
            Some(objects::object::OperationId::from_uuid(
                uuid::Uuid::from_bytes(bytes),
            ))
        }
        other => {
            return Err(HeddleError::InvalidObject(format!(
                "invalid operation_id tag byte {other}"
            )));
        }
    };

    if timestamp_ns >= 1_000_000_000 {
        return Err(HeddleError::InvalidObject(format!(
            "invalid oplog timestamp secs={timestamp_secs} nanos={timestamp_ns}"
        )));
    }
    let timestamp = Utc
        .timestamp_opt(timestamp_secs, timestamp_ns)
        .single()
        .ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "invalid oplog timestamp secs={timestamp_secs} nanos={timestamp_ns}"
            ))
        })?;

    Ok(OpEntry {
        id,
        timestamp,
        operation,
        undone,
        batch_id,
        batch_index,
        scope,
        actor,
        operation_id,
    })
}

fn read_entry_at(file: &mut File, offset: u64) -> Result<OpEntry> {
    file.seek(SeekFrom::Start(offset))?;
    let id = read_u64_from_file(file)?;
    let batch_id = read_u64_from_file(file)?;
    let batch_index = read_u32_from_file(file)?;
    let timestamp_secs = read_i64_from_file(file)?;
    let timestamp_ns = read_u32_from_file(file)?;
    let undone = read_u8_from_file(file)? != 0;

    let scope_len = read_u16_from_file(file)? as usize;
    let scope_bytes = read_vec_from_file(file, scope_len)?;
    let scope = if scope_bytes.is_empty() {
        None
    } else {
        Some(
            String::from_utf8(scope_bytes)
                .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in scope".to_string()))?,
        )
    };

    let op_data_len = read_u32_from_file(file)? as usize;
    let op_data = read_vec_from_file(file, op_data_len)?;
    let operation = decode_current_record(&op_data)?;

    let actor_name_len = read_u16_from_file(file)? as usize;
    let actor_name = String::from_utf8(read_vec_from_file(file, actor_name_len)?)
        .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in actor.name".to_string()))?;
    let actor_email_len = read_u16_from_file(file)? as usize;
    let actor_email = String::from_utf8(read_vec_from_file(file, actor_email_len)?)
        .map_err(|_| HeddleError::InvalidObject("invalid UTF-8 in actor.email".to_string()))?;
    let actor = std::sync::Arc::new(objects::object::Principal {
        name: actor_name,
        email: actor_email,
    });
    let operation_id_tag = read_u8_from_file(file)?;
    let operation_id = match operation_id_tag {
        0 => None,
        1 => Some(objects::object::OperationId::from_uuid(
            uuid::Uuid::from_bytes(read_array_from_file::<16>(file)?),
        )),
        other => {
            return Err(HeddleError::InvalidObject(format!(
                "invalid operation_id tag byte {other}"
            )));
        }
    };

    if timestamp_ns >= 1_000_000_000 {
        return Err(HeddleError::InvalidObject(format!(
            "invalid oplog timestamp secs={timestamp_secs} nanos={timestamp_ns}"
        )));
    }
    let timestamp = Utc
        .timestamp_opt(timestamp_secs, timestamp_ns)
        .single()
        .ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "invalid oplog timestamp secs={timestamp_secs} nanos={timestamp_ns}"
            ))
        })?;

    Ok(OpEntry {
        id,
        timestamp,
        operation,
        undone,
        batch_id,
        batch_index,
        scope,
        actor,
        operation_id,
    })
}

fn encode_entry(entry: &OpEntry, out: &mut Vec<u8>) -> Result<()> {
    encode_entry_with(entry, out, encode_current_record)
}

fn encode_entry_with(
    entry: &OpEntry,
    out: &mut Vec<u8>,
    encode_record: impl Fn(&OpRecord) -> Result<Vec<u8>>,
) -> Result<()> {
    out.extend_from_slice(&entry.id.to_le_bytes());
    out.extend_from_slice(&entry.batch_id.to_le_bytes());
    out.extend_from_slice(&entry.batch_index.to_le_bytes());
    out.extend_from_slice(&entry.timestamp.timestamp().to_le_bytes());
    out.extend_from_slice(&entry.timestamp.timestamp_subsec_nanos().to_le_bytes());
    out.push(if entry.undone { 1 } else { 0 });

    let scope_bytes = entry.scope.as_deref().unwrap_or("").as_bytes();
    out.extend_from_slice(&(scope_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(scope_bytes);

    let op_data = encode_record(&entry.operation)?;
    out.extend_from_slice(&(op_data.len() as u32).to_le_bytes());
    out.extend_from_slice(&op_data);

    let actor_name = entry.actor.name.as_bytes();
    out.extend_from_slice(&(actor_name.len() as u16).to_le_bytes());
    out.extend_from_slice(actor_name);
    let actor_email = entry.actor.email.as_bytes();
    out.extend_from_slice(&(actor_email.len() as u16).to_le_bytes());
    out.extend_from_slice(actor_email);
    match entry.operation_id {
        Some(op_id) => {
            out.push(1);
            out.extend_from_slice(op_id.as_bytes());
        }
        None => out.push(0),
    }
    Ok(())
}

fn write_header<W: Write>(out: &mut W, version: u32, entry_count: u64, head_id: u64) -> Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&version.to_le_bytes())?;
    if version == CURRENT_CONTAINER_VERSION {
        out.write_all(&CURRENT_OP_RECORD_SCHEMA_VERSION.to_le_bytes())?;
    }
    out.write_all(&entry_count.to_le_bytes())?;
    out.write_all(&head_id.to_le_bytes())?;
    Ok(())
}

fn write_header_to_vec(out: &mut Vec<u8>, version: u32, entry_count: u64, head_id: u64) {
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    if version == CURRENT_CONTAINER_VERSION {
        out.extend_from_slice(&CURRENT_OP_RECORD_SCHEMA_VERSION.to_le_bytes());
    }
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&head_id.to_le_bytes());
}

fn write_footer<W: Write>(out: &mut W, footer: &PackedFooter) -> Result<()> {
    out.write_all(INDEX_MAGIC)?;
    out.write_all(&INDEX_VERSION.to_le_bytes())?;
    out.write_all(&(FOOTER_LEN as u32).to_le_bytes())?;
    for value in footer_u64_values(footer) {
        out.write_all(&value.to_le_bytes())?;
    }
    Ok(())
}

fn write_footer_to_vec(out: &mut Vec<u8>, footer: &PackedFooter) {
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&(FOOTER_LEN as u32).to_le_bytes());
    for value in footer_u64_values(footer) {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn footer_u64_values(footer: &PackedFooter) -> [u64; FOOTER_U64_FIELDS as usize] {
    [
        footer.entry_data_end,
        footer.entry_offsets_offset,
        footer.entry_offsets_count,
        footer.batch_offsets_offset,
        footer.batch_offsets_count,
        footer.batch_dir_offset,
        footer.batch_dir_count,
        footer.tx_key_bytes_offset,
        footer.tx_key_bytes_len,
        footer.tx_dir_offset,
        footer.tx_dir_count,
        footer.entry_count,
        footer.head_id,
    ]
}

fn write_batch_dir_record<W: Write>(out: &mut W, record: &BatchDirRecord) -> Result<()> {
    out.write_all(&record.batch_id.to_le_bytes())?;
    out.write_all(&record.newest_entry_id.to_le_bytes())?;
    out.write_all(&record.first_offset_index.to_le_bytes())?;
    out.write_all(&record.entry_count.to_le_bytes())?;
    out.write_all(&[record.scope_state])?;
    out.write_all(&[0; 3])?;
    out.write_all(&0u64.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&[0; 4])?;
    Ok(())
}

fn write_batch_dir_record_to_vec(out: &mut Vec<u8>, record: &BatchDirRecord) {
    out.extend_from_slice(&record.batch_id.to_le_bytes());
    out.extend_from_slice(&record.newest_entry_id.to_le_bytes());
    out.extend_from_slice(&record.first_offset_index.to_le_bytes());
    out.extend_from_slice(&record.entry_count.to_le_bytes());
    out.push(record.scope_state);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
}

fn write_tx_dir_record<W: Write>(out: &mut W, record: &TxDirRecord) -> Result<()> {
    out.write_all(&record.key_offset.to_le_bytes())?;
    out.write_all(&record.key_len.to_le_bytes())?;
    out.write_all(&[0; 4])?;
    out.write_all(&record.commit_entry_id.to_le_bytes())?;
    out.write_all(&record.batch_id.to_le_bytes())?;
    Ok(())
}

fn write_tx_dir_record_to_vec(out: &mut Vec<u8>, record: &TxDirRecord) {
    out.extend_from_slice(&record.key_offset.to_le_bytes());
    out.extend_from_slice(&record.key_len.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&record.commit_entry_id.to_le_bytes());
    out.extend_from_slice(&record.batch_id.to_le_bytes());
}

fn read_vec_from_file(file: &mut File, len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0; len];
    file.read_exact(&mut out)?;
    Ok(out)
}

fn read_array_from_file<const N: usize>(file: &mut File) -> Result<[u8; N]> {
    let mut out = [0; N];
    file.read_exact(&mut out)?;
    Ok(out)
}

fn read_u8_from_file(file: &mut File) -> Result<u8> {
    Ok(read_array_from_file::<1>(file)?[0])
}

fn read_u16_from_file(file: &mut File) -> Result<u16> {
    Ok(u16::from_le_bytes(read_array_from_file::<2>(file)?))
}

fn read_u32_from_file(file: &mut File) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array_from_file::<4>(file)?))
}

fn read_u64_from_file(file: &mut File) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array_from_file::<8>(file)?))
}

fn read_i64_from_file(file: &mut File) -> Result<i64> {
    Ok(i64::from_le_bytes(read_array_from_file::<8>(file)?))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| HeddleError::InvalidObject("oplog cursor overflow".to_string()))?;
        if end > self.bytes.len() {
            return Err(HeddleError::InvalidObject("oplog truncated".to_string()));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(out)
    }

    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or_else(|| HeddleError::InvalidObject("oplog cursor overflow".to_string()))?;
        if end > self.bytes.len() {
            return Err(HeddleError::InvalidObject("oplog truncated".to_string()));
        }
        let out = self.bytes[self.offset..end].to_vec();
        self.offset = end;
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.read_array::<8>()?))
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn reconstructible_append_reuses_reflinked_entry_prefix_when_available() {
        assert_eq!(
            reconstructible_append_strategy(ReflinkOutcome::Cloned),
            ReconstructibleAppendStrategy::CloneAndRewriteTail,
        );
        assert_eq!(
            reconstructible_append_strategy(ReflinkOutcome::Unsupported),
            ReconstructibleAppendStrategy::Rewrite,
        );
        assert_eq!(
            reconstructible_append_strategy(ReflinkOutcome::SourceVanished),
            ReconstructibleAppendStrategy::Rewrite,
        );
    }

    fn make_entry(id: u64, scope: Option<&str>) -> OpEntry {
        let state = crate::oplog::fresh_state_id();
        OpEntry {
            id,
            timestamp: Utc::now(),
            operation: OpRecord::Snapshot {
                new_state: state,
                prev_head: None,
                head: Some(state),
                thread: None,
            },
            undone: false,
            batch_id: id,
            batch_index: 0,
            scope: scope.map(str::to_string),
            actor: std::sync::Arc::new(objects::object::Principal::new("Test", "test@example.com")),
            operation_id: None,
        }
    }

    fn make_batch_entry(id: u64, batch_id: u64, batch_index: u32, scope: Option<&str>) -> OpEntry {
        let mut entry = make_entry(id, scope);
        entry.batch_id = batch_id;
        entry.batch_index = batch_index;
        entry
    }

    fn make_commit_entry(id: u64, transaction_id: &str) -> OpEntry {
        let mut entry = make_entry(id, Some("lane"));
        entry.operation = OpRecord::TransactionCommit {
            transaction_id: transaction_id.into(),
            op_count: 0,
        };
        entry
    }

    fn write_legacy_header(path: &Path, version: u32) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn write_v4_header_with_schema(path: &Path, schema: u32) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&CURRENT_CONTAINER_VERSION.to_le_bytes());
        bytes.extend_from_slice(&schema.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn decode_hex_fixture(encoded: &str) -> std::result::Result<Vec<u8>, String> {
        let compact = encoded.split_whitespace().collect::<String>();
        if compact.len() % 2 != 0 {
            return Err(format!(
                "hex fixture has an odd number of digits: {}",
                compact.len()
            ));
        }
        compact
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let digits = std::str::from_utf8(pair)
                    .map_err(|error| format!("hex fixture is not UTF-8: {error}"))?;
                u8::from_str_radix(digits, 16)
                    .map_err(|error| format!("invalid hex fixture byte {digits:?}: {error}"))
            })
            .collect()
    }

    fn directory_listing(path: &Path) -> Vec<String> {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn refused_without_mutation(path: &Path) -> HeddleError {
        let parent = path.parent().unwrap();
        let before = std::fs::read(path).unwrap();
        let before_listing = directory_listing(parent);
        let error = PackedOpLog::ensure_current(path).expect_err("legacy oplog must be refused");
        let repeated = PackedOpLog::ensure_current(path)
            .expect_err("repeated legacy oplog open must be refused");
        let load_error = match PackedOpLog::load(path) {
            Ok(_) => panic!("legacy oplog load must be refused"),
            Err(error) => error,
        };

        assert_eq!(repeated.to_string(), error.to_string());
        if matches!(
            &error,
            HeddleError::StorageFormatMigrationRequired { .. }
                | HeddleError::StorageFormatTooNew { .. }
        ) {
            assert_eq!(load_error.to_string(), error.to_string());
        }
        assert_eq!(std::fs::read(path).unwrap(), before);
        let after_listing = directory_listing(parent);
        assert_eq!(after_listing, before_listing);
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(
            !after_listing
                .iter()
                .any(|name| name.starts_with(&format!(".{file_name}.tmp-"))),
            "refusal must not leave an atomic-write temp file"
        );
        assert!(
            !after_listing
                .iter()
                .any(|name| name.starts_with(&format!("{file_name}.corrupt"))),
            "refusal must not quarantine a non-corrupt legacy oplog"
        );
        assert!(
            !recovery_sidecar_path(path).exists(),
            "refusal must not create a recovery sidecar"
        );
        error
    }

    fn read_current_entry_offsets(bytes: &[u8]) -> (Vec<EntryOffsetRecord>, PackedFooter) {
        let header = parse_header(bytes).unwrap();
        let footer = PackedFooter::parse(bytes, &header).unwrap();
        let mut cursor = Cursor::new(&bytes[footer.entry_offsets_offset as usize..]);
        let mut offsets = Vec::with_capacity(footer.entry_offsets_count as usize);
        for _ in 0..footer.entry_offsets_count {
            offsets.push(EntryOffsetRecord {
                entry_id: cursor.read_u64().unwrap(),
                entry_offset: cursor.read_u64().unwrap(),
            });
        }
        (offsets, footer)
    }

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let log = PackedOpLog::new(path.clone());
        log.save().unwrap();
        let loaded = PackedOpLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 0);
        assert_eq!(loaded.head_id, 0);
        assert_eq!(PackedOpLog::read_head_id(&path).unwrap(), 0);
    }

    #[test]
    fn round_trip_with_entries_and_index_reads() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![
            make_entry(1, Some("lane-a")),
            make_entry(2, Some("lane-b")),
        ]);
        log.head_id = 2;
        log.save().unwrap();

        let loaded = PackedOpLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.head_id, 2);
        assert_eq!(loaded.entries[0].id, 1);
        assert_eq!(loaded.entries[1].id, 2);
        assert_eq!(loaded.entries[0].scope.as_deref(), Some("lane-a"));

        let index = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(index.head_id(), 2);
        assert_eq!(index.last_entry().unwrap().unwrap().id, 2);
        assert_eq!(
            index
                .recent_entries(2)
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn read_head_id_rejects_too_new_container_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, Some("lane"))]);
        log.head_id = 1;
        log.save().unwrap();

        assert_eq!(PackedOpLog::read_head_id(&path).unwrap(), 1);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&(CURRENT_CONTAINER_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let before = std::fs::read(&path).unwrap();
        let err = PackedOpLog::read_head_id(&path).unwrap_err();
        assert!(
            matches!(
                &err,
                HeddleError::StorageFormatTooNew {
                    storage,
                    found,
                    supported,
                } if storage == OPLOG_CONTAINER_STORAGE
                    && *found == CURRENT_CONTAINER_VERSION + 1
                    && *supported == CURRENT_CONTAINER_VERSION
            ),
            "fast path must reject a too-new version, got: {err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(PackedOpLog::load(&path).is_err());
    }

    #[test]
    fn v2_and_v3_containers_are_refused_without_mutation() {
        for version in [2, 3] {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("oplog.bin");
            write_legacy_header(&path, version);
            let error = refused_without_mutation(&path);
            assert!(
                matches!(
                    &error,
                    HeddleError::StorageFormatMigrationRequired {
                        storage,
                        found,
                        required,
                    } if storage == OPLOG_CONTAINER_STORAGE
                        && *found == version
                        && *required == CURRENT_CONTAINER_VERSION
                ),
                "legacy container {version} must require migration, got {error:?}"
            );
        }
    }

    #[test]
    fn v4_legacy_record_schemas_are_refused_without_mutation() {
        for schema in 1..=3 {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("oplog.bin");
            write_v4_header_with_schema(&path, schema);
            let error = refused_without_mutation(&path);
            assert!(
                matches!(
                    &error,
                    HeddleError::StorageFormatMigrationRequired {
                        storage,
                        found,
                        required,
                    } if storage == "oplog record schema"
                        && *found == schema
                        && *required == CURRENT_OP_RECORD_SCHEMA_VERSION
                ),
                "legacy record schema {schema} must require migration, got {error:?}"
            );
        }
    }

    #[test]
    fn v4_too_new_record_schema_is_refused_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let schema = CURRENT_OP_RECORD_SCHEMA_VERSION + 1;
        write_v4_header_with_schema(&path, schema);

        let error = refused_without_mutation(&path);
        assert!(
            matches!(
                &error,
                HeddleError::StorageFormatTooNew {
                    storage,
                    found,
                    supported,
                } if storage == "oplog record schema"
                    && *found == schema
                    && *supported == CURRENT_OP_RECORD_SCHEMA_VERSION
            ),
            "too-new record schema must be refused, got {error:?}"
        );
    }

    #[test]
    fn checked_in_change_id_fixture_is_refused_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let oplog_dir = tmp.path().join("oplog");
        std::fs::create_dir(&oplog_dir).unwrap();
        let path = oplog_dir.join("oplog.bin");
        let bytes = decode_hex_fixture(include_str!(
            "../../tests/fixtures/issue-449-legacy-pre-atomic/oplog.bin.hex"
        ))
        .unwrap();
        assert_eq!(bytes.len(), 704, "historical fixture length changed");
        let expected_digest =
            decode_hex_fixture("584f0dab2367507f86c6184bc29660765c20416fe529a1618eb064a960708c33")
                .unwrap();
        assert_eq!(
            &Sha256::digest(&bytes)[..],
            expected_digest.as_slice(),
            "historical fixture bytes changed"
        );
        assert_eq!(&bytes[..12], b"LMOPLOG\0\x02\0\0\0");
        std::fs::write(&path, bytes).unwrap();

        let error = refused_without_mutation(&path);
        assert!(
            matches!(
                &error,
                HeddleError::StorageFormatMigrationRequired {
                    storage,
                    found: 2,
                    required,
                } if storage == OPLOG_CONTAINER_STORAGE
                    && *required == CURRENT_CONTAINER_VERSION
            ),
            "checked-in legacy fixture must require migration, got {error:?}"
        );

        let before = std::fs::read(&path).unwrap();
        let before_root_listing = directory_listing(tmp.path());
        let before_oplog_listing = directory_listing(&oplog_dir);
        let oplog = crate::oplog::OpLog::new_unattributed(tmp.path());
        for _ in 0..2 {
            let public_error = oplog
                .head_id()
                .expect_err("public OpLog reads must refuse the legacy fixture");
            assert_eq!(public_error.to_string(), error.to_string());
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(directory_listing(tmp.path()), before_root_listing);
        assert_eq!(directory_listing(&oplog_dir), before_oplog_listing);
    }

    #[test]
    fn hex_fixture_decoder_rejects_odd_or_invalid_input() {
        assert!(decode_hex_fixture("0").is_err());
        assert!(decode_hex_fixture("gg").is_err());
    }

    #[test]
    fn v1_and_corrupt_headers_fail_loudly() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");

        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 1, 0, 0);
        std::fs::write(&path, bytes).unwrap();
        let err = refused_without_mutation(&path);
        assert!(
            matches!(
                &err,
                HeddleError::StorageFormatMigrationRequired {
                    storage,
                    found: 1,
                    required,
                } if storage == OPLOG_CONTAINER_STORAGE
                    && *required == CURRENT_CONTAINER_VERSION
            ),
            "v1 must fail loudly, got {err:?}"
        );

        std::fs::write(&path, b"not an oplog").unwrap();
        let err = refused_without_mutation(&path);
        assert!(
            matches!(&err, HeddleError::InvalidObject(message) if message.contains("invalid oplog magic") || message.contains("truncated")),
            "corrupt header must fail loudly, got {err:?}"
        );
    }

    #[test]
    fn set_undone_flips_entries_and_rebuilt_index_agrees() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, None)]);
        log.head_id = 1;
        assert!(!log.entries[0].undone);
        log.set_undone(1, true);
        assert!(log.entries[0].undone);
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        assert!(index.last_entry().unwrap().unwrap().undone);
    }

    #[test]
    fn collect_batches_scoped_excludes_mixed_scope_batches_without_counting_them() {
        let tmp = TempDir::new().unwrap();
        let mut log = PackedOpLog::new(tmp.path().join("oplog.bin"));
        log.append(vec![
            make_batch_entry(1, 10, 0, Some("lane-a")),
            make_batch_entry(2, 10, 1, Some("lane-a")),
            make_batch_entry(3, 20, 0, Some("lane-a")),
            make_batch_entry(4, 20, 1, Some("lane-b")),
            make_batch_entry(5, 30, 0, Some("lane-a")),
            make_batch_entry(6, 40, 0, Some("lane-a")),
        ]);

        let batches = log.collect_batches_scoped(3, |_| true, Some("lane-a"));

        assert_eq!(
            batches.iter().map(|batch| batch.id).collect::<Vec<_>>(),
            vec![40, 30, 10]
        );
        assert_eq!(
            batches[2]
                .entries
                .iter()
                .map(|entry| entry.batch_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn index_collect_batches_merges_non_contiguous_runs_before_counting() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![
            make_batch_entry(1, 10, 0, Some("lane-a")),
            make_batch_entry(2, 10, 1, Some("lane-a")),
            make_batch_entry(3, 20, 0, Some("lane-a")),
            make_batch_entry(4, 20, 1, Some("lane-a")),
            make_batch_entry(5, 10, 2, Some("lane-a")),
            make_batch_entry(6, 10, 3, Some("lane-a")),
            make_batch_entry(7, 30, 0, Some("lane-a")),
        ]);
        log.head_id = 7;
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        let batches = index
            .collect_batches_scoped(2, |_| true, Some("lane-a"))
            .unwrap();

        assert_eq!(
            batches.iter().map(|batch| batch.id).collect::<Vec<_>>(),
            vec![30, 10]
        );
        assert_eq!(
            batches[1]
                .entries
                .iter()
                .map(|entry| entry.batch_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            batches[1]
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 5, 6]
        );
    }

    #[test]
    fn index_collect_batches_after_stops_at_newest_first_since_boundary() {
        const BATCH_COUNT: u64 = 128;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append((1..=BATCH_COUNT).map(|id| make_entry(id, None)).collect());
        log.head_id = BATCH_COUNT;
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        BATCH_DIR_RECORDS_VISITED.with(|visits| visits.set(0));
        let batches = index
            .collect_batches_after_scoped(BATCH_COUNT - 2, usize::MAX, |_| true, None)
            .unwrap();

        assert_eq!(
            batches.iter().map(|batch| batch.id).collect::<Vec<_>>(),
            vec![BATCH_COUNT, BATCH_COUNT - 1]
        );
        BATCH_DIR_RECORDS_VISITED.with(|visits| {
            assert_eq!(
                visits.get(),
                3,
                "the newest-first batch directory must stop at the since boundary"
            );
        });
    }

    #[test]
    fn transaction_index_finds_commit_and_batch_records() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut op = make_entry(1, Some("lane"));
        op.operation = OpRecord::Snapshot {
            new_state: crate::oplog::fresh_state_id(),
            prev_head: None,
            head: None,
            thread: Some("main".into()),
        };
        op.batch_id = 1;
        let mut commit = make_batch_entry(2, 1, 1, Some("lane"));
        commit.operation = OpRecord::TransactionCommit {
            transaction_id: "tx-1".into(),
            op_count: 1,
        };
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![op, commit]);
        log.head_id = 2;
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(index.transaction_commit("tx-1").unwrap(), Some((2, 1)));
        assert_eq!(index.committed_batch_records("tx-1").unwrap().len(), 1);
        assert!(index.committed_batch_records("missing").unwrap().is_empty());
    }

    #[test]
    fn transaction_index_rejects_key_that_disagrees_with_commit_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut op = make_entry(1, Some("lane"));
        op.operation = OpRecord::Snapshot {
            new_state: crate::oplog::fresh_state_id(),
            prev_head: None,
            head: None,
            thread: Some("main".into()),
        };
        op.batch_id = 1;
        let mut commit = make_batch_entry(2, 1, 1, Some("lane"));
        commit.operation = OpRecord::TransactionCommit {
            transaction_id: "tx-1".into(),
            op_count: 1,
        };
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![op, commit]);
        log.head_id = 2;
        log.save().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let (_offsets, footer) = read_current_entry_offsets(&bytes);
        let key_start = usize::try_from(footer.tx_key_bytes_offset).unwrap();
        bytes[key_start..key_start + 4].copy_from_slice(b"tx-2");
        std::fs::write(&path, bytes).unwrap();

        let err = PackedOpLogIndex::open(&path).unwrap_err();
        assert!(
            matches!(err, HeddleError::InvalidObject(ref message) if message.contains("key disagrees with commit transaction_id")),
            "expected mismatched tx_dir key to fail validation, got {err:?}"
        );
    }

    #[test]
    fn append_rebuilds_indexes_atomically() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let log = PackedOpLog::new(path.clone());
        log.save().unwrap();
        let index = PackedOpLogIndex::open(&path).unwrap();
        let mut first = make_entry(1, Some("lane"));
        first.batch_id = 1;
        let updated = index.append_entries(&[first]).unwrap();

        assert_eq!(updated.head_id(), 1);
        assert_eq!(updated.last_entry().unwrap().unwrap().id, 1);
        assert_eq!(PackedOpLog::load(&path).unwrap().entries.len(), 1);
    }

    #[test]
    fn reconstructible_append_preserves_history_and_rebuilds_current_indexes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![
            make_entry(1, Some("lane")),
            make_entry(2, Some("lane")),
        ]);
        log.head_id = 2;
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        let mut commit = make_entry(4, Some("lane"));
        commit.operation = OpRecord::TransactionCommit {
            transaction_id: "tx-reconstructible".into(),
            op_count: 0,
        };
        INDEX_SECTION_DISK_READS.with(|reads| reads.set(0));
        let updated = index
            .append_entries_reconstructible(&[make_entry(3, Some("lane")), commit])
            .unwrap();

        assert_eq!(updated.head_id(), 4);
        assert_eq!(updated.last_entry().unwrap().unwrap().id, 4);
        assert_eq!(
            updated.transaction_commit("tx-reconstructible").unwrap(),
            Some((4, 4))
        );
        INDEX_SECTION_DISK_READS.with(|reads| assert_eq!(reads.get(), 0));
        let reopened = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(
            reopened.transaction_commit("tx-reconstructible").unwrap(),
            Some((4, 4))
        );
        let loaded = PackedOpLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 4);
        assert_eq!(
            loaded
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn incremental_tx_index_inserts_out_of_order_keys_in_sorted_directory() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        PackedOpLog::new(path.clone()).save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        let updated = index
            .append_entries_reconstructible(&[
                make_commit_entry(1, "tx-zeta"),
                make_commit_entry(2, "tx-alpha"),
                make_commit_entry(3, "tx-middle"),
            ])
            .unwrap();

        let keys = updated
            .validated_indexes
            .tx_dir
            .iter()
            .map(|record| {
                std::str::from_utf8(
                    tx_record_key(&updated.validated_indexes.tx_key_bytes, record).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["tx-alpha", "tx-middle", "tx-zeta"]);
        assert_eq!(updated.transaction_commit("tx-zeta").unwrap(), Some((1, 1)));
        assert_eq!(
            updated.transaction_commit("tx-alpha").unwrap(),
            Some((2, 2))
        );
        assert_eq!(
            updated.transaction_commit("tx-middle").unwrap(),
            Some((3, 3))
        );

        let reopened = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(
            reopened.transaction_commit("tx-alpha").unwrap(),
            Some((2, 2))
        );
    }

    #[test]
    fn incremental_tx_index_keeps_first_duplicate_transaction() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        PackedOpLog::new(path.clone()).save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        let updated = index
            .append_entries_reconstructible(&[
                make_commit_entry(1, "tx-duplicate"),
                make_commit_entry(2, "tx-duplicate"),
            ])
            .unwrap();
        let updated = updated
            .append_entries_reconstructible(&[make_commit_entry(3, "tx-duplicate")])
            .unwrap();

        assert_eq!(
            updated.transaction_commit("tx-duplicate").unwrap(),
            Some((1, 1))
        );
        assert_eq!(updated.validated_indexes.tx_dir.len(), 1);
        assert_eq!(updated.validated_indexes.tx_key_bytes, b"tx-duplicate");
        let reopened = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(
            reopened.transaction_commit("tx-duplicate").unwrap(),
            Some((1, 1))
        );
    }

    #[test]
    fn incremental_batch_index_inserts_records_newest_first() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![
            make_batch_entry(1, 10, 0, Some("lane")),
            make_batch_entry(2, 10, 1, Some("lane")),
        ]);
        log.head_id = 2;
        log.save().unwrap();

        let index = PackedOpLogIndex::open(&path).unwrap();
        let updated = index
            .append_entries_reconstructible(&[
                make_batch_entry(3, 900, 0, Some("lane")),
                make_batch_entry(4, 5, 0, Some("lane")),
            ])
            .unwrap();

        assert_eq!(
            updated
                .validated_indexes
                .batch_dir
                .iter()
                .map(|record| (record.newest_entry_id, record.batch_id))
                .collect::<Vec<_>>(),
            vec![(4, 5), (3, 900), (2, 10)]
        );
        let reopened = PackedOpLogIndex::open(&path).unwrap();
        assert_eq!(
            reopened
                .collect_batches_scoped(3, |_| true, Some("lane"))
                .unwrap()
                .iter()
                .map(|batch| batch.id)
                .collect::<Vec<_>>(),
            vec![5, 900, 10]
        );
    }

    #[test]
    fn truncated_latest_oplog_is_quarantined_and_salvaged_at_complete_records() {
        let tmp = TempDir::new().unwrap();
        let source_path = tmp.path().join("source-oplog.bin");
        let mut log = PackedOpLog::new(source_path.clone());
        log.append(vec![
            make_entry(1, None),
            make_entry(2, None),
            make_entry(3, None),
        ]);
        log.head_id = 3;
        log.save().unwrap();

        let original = std::fs::read(&source_path).unwrap();
        let (offsets, footer) = read_current_entry_offsets(&original);
        let cases = [
            (
                "entry-header",
                (offsets[0].entry_offset + 4) as usize,
                0usize,
            ),
            (
                "mid-record",
                (offsets[1].entry_offset
                    + ((offsets[2].entry_offset - offsets[1].entry_offset) / 2))
                    as usize,
                1,
            ),
            (
                "last-record",
                (offsets[2].entry_offset + ((footer.entry_data_end - offsets[2].entry_offset) / 2))
                    as usize,
                2,
            ),
            ("footer", original.len() - 1, 3),
        ];

        for (name, truncate_at, expected_count) in cases {
            let case_dir = TempDir::new().unwrap();
            let path = case_dir.path().join("oplog.bin");
            let mut truncated = original.clone();
            truncated.truncate(truncate_at);
            std::fs::write(&path, truncated).unwrap();

            let index = PackedOpLogIndex::open(&path).unwrap_or_else(|err| {
                panic!("{name}: truncated oplog should salvage, got {err:?}")
            });
            assert_eq!(index.head_id(), expected_count as u64, "{name}: head id");
            assert!(
                case_dir.path().join("oplog.bin.corrupt").exists(),
                "{name}: damaged oplog must be quarantined"
            );

            let loaded = PackedOpLog::load(&path).unwrap();
            assert_eq!(
                loaded.entries.len(),
                expected_count,
                "{name}: recovered entry count"
            );
            assert_eq!(loaded.head_id, expected_count as u64, "{name}: loaded head");

            let appended = index.append_entries(&[make_entry((expected_count + 1) as u64, None)]);
            assert!(
                appended.is_ok(),
                "{name}: repo should be appendable afterward"
            );
        }
    }

    #[test]
    fn footer_header_disagreement_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, None)]);
        log.head_id = 1;
        log.save().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let footer_head_offset = bytes.len() - 8;
        bytes[footer_head_offset..].copy_from_slice(&99u64.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let err = PackedOpLogIndex::open(&path).unwrap_err();
        assert!(
            matches!(&err, HeddleError::InvalidObject(message) if message.contains("header/footer")),
            "metadata disagreement must reject loudly, got {err:?}"
        );
    }

    fn build_three_entry_oplog(path: &Path) -> Vec<u8> {
        let mut log = PackedOpLog::new(path.to_path_buf());
        log.append(vec![
            make_entry(1, None),
            make_entry(2, None),
            make_entry(3, None),
        ]);
        log.head_id = 3;
        log.save().unwrap();
        std::fs::read(path).unwrap()
    }

    #[test]
    fn footer_guided_recovery_uses_surviving_footer_when_trailing_bytes_torn() {
        // A V4 oplog whose entry stream + footer are intact, but trailing bytes
        // were torn off AFTER the footer (e.g. a partial fsync/append of an
        // unrelated tail). The standard parse rejects (file_len !=
        // footer_start + FOOTER_LEN); the backward footer scan must find the
        // intact footer and recover the full, valid prefix footer-guided.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source-oplog.bin");
        let original = build_three_entry_oplog(&source);
        let (_offsets, footer) = read_current_entry_offsets(&original);

        let case_dir = TempDir::new().unwrap();
        let path = case_dir.path().join("oplog.bin");
        let mut torn = original.clone();
        // Append garbage after the real footer so the EOF footer parse fails
        // but the genuine footer still lives intact mid-file.
        torn.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x01, 0x02]);
        std::fs::write(&path, &torn).unwrap();

        let report = recover_oplog_at(&path).unwrap();
        assert!(!report.already_healthy);
        assert_eq!(report.strategy.as_deref(), Some("footer-guided"));
        assert_eq!(report.entries_recovered, 3, "all complete records kept");
        assert_eq!(report.entries_lost, Some(0), "no complete record was lost");
        assert_eq!(
            report.damaged_byte_start, footer.entry_data_end,
            "damaged range starts at the recorded entry-data-end"
        );
        assert_eq!(report.damaged_byte_end as usize, torn.len());

        // The rebuilt oplog must load cleanly with the full prefix.
        let loaded = PackedOpLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.head_id, 3);

        // Quarantine + sidecar both exist.
        assert!(report.quarantine_path.unwrap().exists());
        assert!(report.sidecar_path.unwrap().exists());
    }

    #[test]
    fn footer_guided_preferred_over_forward_greedy_when_footer_intact() {
        // Damage only the index sections between entry_data_end and the footer
        // is harder to stage; instead validate the strategy selection directly:
        // when the footer survives, plan_truncated_latest_recovery must pick
        // footer-guided, not forward-greedy.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("oplog.bin");
        let original = build_three_entry_oplog(&source);

        let mut torn = original.clone();
        torn.push(0x00); // trailing tear after intact footer
        let err = match load_latest(&torn) {
            Ok(_) => panic!("trailing tear after footer should fail the standard parse"),
            Err(err) => err,
        };
        let recovery = plan_truncated_latest_recovery(&torn, &err)
            .unwrap()
            .expect("trailing tear after intact footer is recoverable");
        assert_eq!(recovery.strategy, RecoveryStrategy::FooterGuided);
        assert_eq!(recovery.data.entries.len(), 3);
    }

    #[test]
    fn recovery_sidecar_records_offset_counts_and_strategy() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source-oplog.bin");
        let original = build_three_entry_oplog(&source);
        let (_offsets, footer) = read_current_entry_offsets(&original);

        let case_dir = TempDir::new().unwrap();
        let path = case_dir.path().join("oplog.bin");
        let mut torn = original.clone();
        torn.extend_from_slice(&[0xFF, 0xFF]);
        std::fs::write(&path, &torn).unwrap();

        let report = recover_oplog_at(&path).unwrap();
        let sidecar = report.sidecar_path.clone().unwrap();
        assert_eq!(
            sidecar.file_name().unwrap().to_string_lossy(),
            "oplog.bin.oplog.recovery"
        );
        let contents = std::fs::read_to_string(&sidecar).unwrap();
        assert!(contents.contains("schema=1"), "{contents}");
        assert!(contents.contains("strategy=footer-guided"), "{contents}");
        assert!(
            contents.contains(&format!("truncation_offset={}", footer.entry_data_end)),
            "{contents}"
        );
        assert!(
            contents.contains(&format!("damaged_byte_end={}", torn.len())),
            "{contents}"
        );
        assert!(contents.contains("entries_recovered=3"), "{contents}");
        assert!(contents.contains("entries_lost=0"), "{contents}");
        assert!(contents.contains("recovered_at="), "{contents}");
    }

    #[test]
    fn forward_greedy_recovery_still_writes_sidecar_for_mid_record_truncation() {
        // A mid-record truncation destroys the footer, so footer-guided fails
        // and forward-greedy takes over — the sidecar must still be written and
        // must report the forward-greedy strategy.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source-oplog.bin");
        let original = build_three_entry_oplog(&source);
        let (offsets, _footer) = read_current_entry_offsets(&original);

        let case_dir = TempDir::new().unwrap();
        let path = case_dir.path().join("oplog.bin");
        let mut truncated = original.clone();
        // Cut in the middle of the third record: footer is gone.
        let cut = (offsets[2].entry_offset
            + ((offsets[2].entry_offset - offsets[1].entry_offset) / 2)) as usize;
        truncated.truncate(cut);
        std::fs::write(&path, &truncated).unwrap();

        let report = recover_oplog_at(&path).unwrap();
        assert_eq!(report.strategy.as_deref(), Some("forward-greedy"));
        assert_eq!(report.entries_recovered, 2);
        assert_eq!(report.entries_lost, Some(1));
        let contents = std::fs::read_to_string(report.sidecar_path.unwrap()).unwrap();
        assert!(contents.contains("strategy=forward-greedy"), "{contents}");
        assert!(contents.contains("entries_recovered=2"), "{contents}");
        assert!(contents.contains("entries_lost=1"), "{contents}");
    }

    #[test]
    fn recover_on_healthy_oplog_is_a_noop_and_leaves_bytes_unchanged() {
        // The intact-file path must be byte-for-byte unchanged: no quarantine,
        // no sidecar, no rewrite.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let original = build_three_entry_oplog(&path);

        let report = recover_oplog_at(&path).unwrap();
        assert!(report.already_healthy);
        assert_eq!(report.strategy, None);
        assert!(report.quarantine_path.is_none());
        assert!(report.sidecar_path.is_none());

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, original, "healthy oplog must not be rewritten");
        assert!(
            !path.with_file_name("oplog.bin.corrupt").exists(),
            "no quarantine for a healthy oplog"
        );
        assert!(
            !path.with_file_name("oplog.bin.oplog.recovery").exists(),
            "no sidecar for a healthy oplog"
        );
    }

    #[test]
    fn parse_rejects_invalid_timestamp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, None)]);
        log.head_id = 1;

        let mut bytes = log.serialize().unwrap();
        let header_len = V4_HEADER_LEN as usize;
        let timestamp_ns_offset = header_len + 8 + 8 + 4 + 8;
        bytes[timestamp_ns_offset..timestamp_ns_offset + 4]
            .copy_from_slice(&1_500_000_000u32.to_le_bytes());

        let error = match PackedOpLog::parse(&bytes, path) {
            Ok(_) => panic!("timestamp should be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, HeddleError::InvalidObject(message) if message.contains("invalid oplog timestamp"))
        );
    }
}
