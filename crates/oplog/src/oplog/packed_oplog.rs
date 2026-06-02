// SPDX-License-Identifier: Apache-2.0
//! Packed binary oplog: all entries in a single file, loaded into memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
};

use super::oplog_types::{OpBatch, OpEntry, OpRecord};

const MAGIC: &[u8; 8] = b"LMOPLOG\0";
/// Binary oplog format version.
///
/// `2` adds the W1 fields: each entry now encodes its `actor` (principal
/// name + email, length-prefixed UTF-8) and `operation_id` (tag byte +
/// optional 16-byte UUID). Pre-W1 v1 files are rejected — there are no
/// live deployments to migrate from.
const VERSION: u32 = 2;

pub(crate) struct PackedOpLog {
    pub(crate) entries: Vec<OpEntry>, // sorted by id ascending
    pub(crate) head_id: u64,
    pub(crate) path: PathBuf,
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
        Self::parse(&bytes, path.to_path_buf())
    }

    /// Read only the `head_id` from the fixed-size header — the cheap O(1)
    /// generation gate (heddle#330). Layout: MAGIC(8) + VERSION(4) +
    /// entry_count(8) + head_id(8); `head_id` is at byte offset 20.
    ///
    /// Validates BOTH the magic AND the version: the fast path must reject the
    /// same headers the full [`parse`](Self::parse) would (heddle#354 r6, cid
    /// 3329711891). Otherwise a right-magic / unsupported-version file yields a
    /// generation and lets a reconciled read take the `tip == watermark`
    /// shortcut, silently trusting a format the parser would refuse.
    pub(crate) fn read_head_id(path: &Path) -> Result<u64> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut header = [0u8; 28];
        file.read_exact(&mut header)?;
        if &header[0..8] != MAGIC {
            return Err(HeddleError::InvalidObject(
                "invalid oplog magic".to_string(),
            ));
        }
        let mut version_bytes = [0u8; 4];
        version_bytes.copy_from_slice(&header[8..12]);
        let version = u32::from_le_bytes(version_bytes);
        if version != VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported oplog version {version}"
            )));
        }
        let mut head_id_bytes = [0u8; 8];
        head_id_bytes.copy_from_slice(&header[20..28]);
        Ok(u64::from_le_bytes(head_id_bytes))
    }

    pub(crate) fn save(&self) -> Result<()> {
        let bytes = self.serialize()?;
        write_file_atomic(&self.path, &bytes)?;
        Ok(())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        buf.extend_from_slice(&self.head_id.to_le_bytes());

        for entry in &self.entries {
            buf.extend_from_slice(&entry.id.to_le_bytes());
            buf.extend_from_slice(&entry.batch_id.to_le_bytes());
            buf.extend_from_slice(&entry.batch_index.to_le_bytes());
            buf.extend_from_slice(&entry.timestamp.timestamp().to_le_bytes());
            buf.extend_from_slice(&entry.timestamp.timestamp_subsec_nanos().to_le_bytes());
            buf.push(if entry.undone { 1 } else { 0 });

            let scope_bytes = entry.scope.as_deref().unwrap_or("").as_bytes();
            buf.extend_from_slice(&(scope_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(scope_bytes);

            let op_data = rmp_serde::to_vec(&entry.operation)
                .map_err(|e| HeddleError::Serialization(e.to_string()))?;
            buf.extend_from_slice(&(op_data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&op_data);

            // v2: actor (name + email) + operation_id.
            let actor_name = entry.actor.name.as_bytes();
            buf.extend_from_slice(&(actor_name.len() as u16).to_le_bytes());
            buf.extend_from_slice(actor_name);
            let actor_email = entry.actor.email.as_bytes();
            buf.extend_from_slice(&(actor_email.len() as u16).to_le_bytes());
            buf.extend_from_slice(actor_email);
            match entry.operation_id {
                Some(op_id) => {
                    buf.push(1);
                    buf.extend_from_slice(op_id.as_bytes());
                }
                None => buf.push(0),
            }
        }

        Ok(buf)
    }

    fn parse(bytes: &[u8], path: PathBuf) -> Result<Self> {
        let mut c = Cursor::new(bytes);

        let magic = c.read_array::<8>()?;
        if &magic != MAGIC {
            return Err(HeddleError::InvalidObject(
                "invalid oplog magic".to_string(),
            ));
        }

        let version = c.read_u32()?;
        if version != VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported oplog version {version}"
            )));
        }

        let entry_count = c.read_u64()? as usize;
        let head_id = c.read_u64()?;
        let mut entries = Vec::with_capacity(entry_count);

        for _ in 0..entry_count {
            let id = c.read_u64()?;
            let batch_id = c.read_u64()?;
            let batch_index = c.read_u32()?;
            let timestamp_secs = c.read_i64()?;
            let timestamp_ns = c.read_u32()?;
            let undone = c.read_u8()? != 0;

            let scope_len = c.read_u16()? as usize;
            let scope_bytes = c.read_bytes(scope_len)?;
            let scope = if scope_bytes.is_empty() {
                None
            } else {
                Some(String::from_utf8(scope_bytes).map_err(|_| {
                    HeddleError::InvalidObject("invalid UTF-8 in scope".to_string())
                })?)
            };

            let op_data_len = c.read_u32()? as usize;
            let op_data = c.read_bytes(op_data_len)?;
            let operation: OpRecord = rmp_serde::from_slice(&op_data)
                .map_err(|e| HeddleError::Serialization(e.to_string()))?;

            // v2 fields: actor (name + email) and operation_id.
            let actor_name_len = c.read_u16()? as usize;
            let actor_name = String::from_utf8(c.read_bytes(actor_name_len)?).map_err(|_| {
                HeddleError::InvalidObject("invalid UTF-8 in actor.name".to_string())
            })?;
            let actor_email_len = c.read_u16()? as usize;
            let actor_email = String::from_utf8(c.read_bytes(actor_email_len)?).map_err(|_| {
                HeddleError::InvalidObject("invalid UTF-8 in actor.email".to_string())
            })?;
            let actor = std::sync::Arc::new(objects::object::Principal {
                name: actor_name,
                email: actor_email,
            });
            let operation_id_tag = c.read_u8()?;
            let operation_id = match operation_id_tag {
                0 => None,
                1 => {
                    let bytes = c.read_array::<16>()?;
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

            // Reject nanos in the leap-second range (>= 1e9) explicitly.
            // Heddle's serializer writes `timestamp_subsec_nanos()` which
            // is always < 1e9 in normal time; chrono's `timestamp_opt`
            // accepts nanos up to 2e9 for leap-second representation, so
            // without this guard a corrupted oplog with nanos in
            // [1e9, 2e9) would parse as a phantom leap-second timestamp.
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

            entries.push(OpEntry {
                id,
                timestamp,
                operation,
                undone,
                batch_id,
                batch_index,
                scope,
                actor,
                operation_id,
            });
        }

        Ok(Self {
            entries,
            head_id,
            path,
        })
    }

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

    pub(crate) fn last_entry(&self) -> Option<&OpEntry> {
        self.entries.last()
    }

    pub(crate) fn recent_entries(&self, count: usize) -> Vec<OpEntry> {
        self.entries.iter().rev().take(count).cloned().collect()
    }

    pub(crate) fn collect_batches_scoped(
        &self,
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

        let mut batches: Vec<OpBatch> = Vec::new();
        // Entries are append-ordered, so a batch is complete once the reverse
        // scan reaches a different batch id. Pending therefore stays <= count.
        let mut pending: HashMap<u64, PendingBatch> = HashMap::with_capacity(count.min(1));
        let mut current_batch_id: Option<u64> = None;

        let finalize_batch = |batch_id: u64,
                              pending: &mut HashMap<u64, PendingBatch>,
                              batches: &mut Vec<OpBatch>| {
            let Some(mut pending_batch) = pending.remove(&batch_id) else {
                return;
            };
            if !pending_batch.scope_matches {
                return;
            }

            pending_batch.entries.sort_by_key(|e| e.batch_index);
            let batch = OpBatch {
                id: batch_id,
                entries: pending_batch.entries,
            };

            if predicate(&batch) {
                batches.push(batch);
            }
        };

        for entry in self.entries.iter().rev() {
            let batch_id = if entry.batch_id == 0 {
                entry.id
            } else {
                entry.batch_id
            };

            if current_batch_id != Some(batch_id) {
                if let Some(previous_batch_id) = current_batch_id {
                    finalize_batch(previous_batch_id, &mut pending, &mut batches);
                    if batches.len() == count {
                        break;
                    }
                }
                current_batch_id = Some(batch_id);
            }

            let batch = pending.entry(batch_id).or_insert_with(|| PendingBatch {
                entries: Vec::new(),
                scope_matches: true,
            });
            if let Some(scope) = scope
                && entry.scope.as_deref() != Some(scope)
            {
                batch.scope_matches = false;
            }
            batch.entries.push(entry.clone());
        }

        if batches.len() < count
            && let Some(batch_id) = current_batch_id
        {
            finalize_batch(batch_id, &mut pending, &mut batches);
        }

        batches
    }
}

// Cursor for parsing
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.offset + N;
        if end > self.bytes.len() {
            return Err(HeddleError::InvalidObject("oplog truncated".to_string()));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(out)
    }

    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let end = self.offset + n;
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
    use objects::object::ChangeId;
    use tempfile::TempDir;

    use super::*;

    fn make_entry(id: u64, scope: Option<&str>) -> OpEntry {
        let state = ChangeId::generate();
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

    #[test]
    fn round_trip_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let log = PackedOpLog::new(path.clone());
        log.save().unwrap();
        let loaded = PackedOpLog::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 0);
        assert_eq!(loaded.head_id, 0);
    }

    #[test]
    fn round_trip_with_entries() {
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
    }

    /// Finding 2 (heddle#354 r6, cid 3329711891) — the fast-path header read
    /// rejects an unsupported version just as the full parser does, so a
    /// right-magic / wrong-version file can never yield a silently-trusted
    /// generation.
    #[test]
    fn read_head_id_rejects_unsupported_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, Some("lane"))]);
        log.head_id = 1;
        log.save().unwrap();

        // A valid file's fast-path read succeeds.
        assert_eq!(PackedOpLog::read_head_id(&path).unwrap(), 1);

        // Bump the version field (bytes 8..12) to a forward-incompatible value,
        // leaving the magic intact.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let err = PackedOpLog::read_head_id(&path).unwrap_err();
        assert!(
            matches!(&err, HeddleError::InvalidObject(message) if message.contains("unsupported oplog version")),
            "fast path must reject an unsupported version, got: {err:?}"
        );
        // And the full parser agrees — the two paths stay in lockstep.
        assert!(PackedOpLog::load(&path).is_err());
    }

    #[test]
    fn set_undone_flips_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, None)]);
        log.head_id = 1;
        assert!(!log.entries[0].undone);
        log.set_undone(1, true);
        assert!(log.entries[0].undone);
        log.set_undone(1, false);
        assert!(!log.entries[0].undone);
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
    fn parse_rejects_invalid_timestamp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oplog.bin");
        let mut log = PackedOpLog::new(path.clone());
        log.append(vec![make_entry(1, None)]);
        log.head_id = 1;

        let mut bytes = log.serialize().unwrap();
        let header_len = 8 + 4 + 8 + 8;
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
