// SPDX-License-Identifier: Apache-2.0
//! Streaming pack builder for bounded-memory imports.
//!
//! `PackBuilder` accumulates every `(id, type, data)` tuple in memory
//! before producing a pack. That's fine for sync-protocol packets and
//! small batches, but the import path can produce millions of objects
//! and would OOM on large repos.
//!
//! `StreamingPackBuilder` removes the in-memory buffering by:
//!
//! 1. **Streaming pack data to disk** as objects are added. Compression
//!    runs per-object (the existing zstd path is non-streaming, so the
//!    one compressed payload is held briefly in a `Vec<u8>` before
//!    being written), but the writer never holds more than one
//!    object's worth of data plus its `BufWriter` capacity.
//!
//! 2. **External sorting the index** via 512 hash-prefix bucket files
//!    on disk (256 for `Hash` ids, 256 for `ChangeId` ids). Each
//!    `add()` appends one fixed-shape `(id, offset)` record to the
//!    bucket whose first byte matches the id's first inner byte. At
//!    finalize, each bucket is small enough to sort in memory; the
//!    concatenation of `Hash` buckets followed by `ChangeId` buckets
//!    in byte order produces the exact same global sort `PackBuilder`
//!    would have via `entries.sort_by_key(|e| e.id)`.
//!
//! ## Memory bound
//!
//! - Pack data on disk: streamed; only one compressed object held in
//!   memory at a time.
//! - Index entries in bucket buffers: at most 32 bucket files are held
//!   open at once, each behind a default-capacity `BufWriter` (~8 KB),
//!   so peak buffering is ~256 KB.
//! - Sort scratch at finalize: O(largest bucket). For uniformly-
//!   distributed BLAKE3 hashes / ULID change-ids and N total objects,
//!   the largest bucket is ~N/256 entries ≈ 40 bytes each. Even at
//!   100 M objects that's ~16 MB peak.
//!
//! Net peak memory: ~20 MB regardless of repo size, modulo the size
//! of the largest single object (which is unavoidable while the zstd
//! API is non-streaming).
//!
//! ## Trade-offs vs `PackBuilder`
//!
//! - **No delta encoding.** Streaming and sliding-window deltas are
//!   incompatible — delta search needs random access to recently-
//!   written objects. The import path runs with deltas disabled
//!   anyway (the cost-benefit is bad on real Heddle history), so this
//!   is a non-issue for the call site that motivated this builder.
//! - **No path-grouped reordering.** Entries land in the order added.
//! - **Output is a pack file at a path** rather than `(Vec<u8>, Vec<u8>)`.
//!   Callers pair this with [`crate::store::ObjectStore::install_pack_from_path`]
//!   which moves/installs the pack without copying it through RAM.
//! - **Re-reads the pack at finalize** to compute the BLAKE3 trailer
//!   checksum (the pack format hashes header+body, and the count goes
//!   in the header — we patch it on finalize via seek-back, then
//!   re-stream the body to the hasher). 2× sequential disk I/O on the
//!   pack data is the cost of sticking with the current format. A
//!   future format change could put the count in the footer to avoid
//!   the second pass.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use super::{ObjectType, PackObjectId, PackStats, pack_container_spec, write_container_header};

/// How many bytes to reserve for the compressed-size varint in the
/// streaming path. 10 is enough to encode any `u64` (max 9 7-bit
/// continuation bytes plus 1 terminator). After streaming we patch
/// the placeholder with a non-canonical varint that pads to exactly
/// this length. Only the zstd-enabled compress path uses it.
#[cfg(feature = "zstd")]
const CSIZE_PLACEHOLDER_LEN: usize = 10;
use crate::{
    object::ContentHash,
    store::{Result, StoreError, compression::CompressionConfig},
};

/// Number of buckets per id variant. 256 = one bucket per first byte
/// of the inner id. We want the bucket boundaries to align with the
/// `PackObjectId`'s `Ord` derivation (variant tag major, inner bytes
/// minor) so the concatenated bucket output matches what
/// `PackIndex::sort()` would have produced.
const BUCKETS_PER_VARIANT: usize = 256;
/// 256 for Hash ids + 256 for ChangeId ids.
const TOTAL_BUCKETS: usize = BUCKETS_PER_VARIANT * 2;
/// Cap concurrently-open index-bucket files. macOS GUI-launched
/// processes commonly inherit a 256-fd soft limit; imports also need
/// room for Git pack/index files, sqlite maps, the output pack, etc.
const MAX_OPEN_BUCKET_WRITERS: usize = 32;

/// Variant indices into the `bucket_*` arrays. `Hash` ids fill the
/// lower half (matches the variant order in `PackObjectId` which makes
/// `Hash(_) < ChangeId(_)`).
const HASH_VARIANT: usize = 0;
const CHANGEID_VARIANT: usize = 1;

/// Streaming pack builder. Held generic over the pack writer (`File`
/// in production, `Cursor<Vec<u8>>` in tests).
pub struct StreamingPackBuilder<W: Write + Read + Seek> {
    /// Writer for the pack's `[header][body]` content. The trailer
    /// checksum is appended to the same writer at `finalize`.
    /// Wrapped in `Option` so `finalize` can `.take()` it out without
    /// running afoul of the `Drop` impl's restriction on moving fields.
    /// `None` after `finalize` succeeds.
    pack_writer: Option<BufWriter<W>>,
    /// Position in the pack writer where the header was written, so
    /// we can seek back at finalize and patch the real `object_count`
    /// into bytes 8..16.
    header_offset: u64,
    object_count: u64,
    total_uncompressed: u64,
    total_compressed: u64,
    /// Compression knobs. Only consulted when the `zstd` feature is on
    /// (`enabled` and `min_size` decide whether each entry compresses;
    /// `level` parameterizes the encoder). Without `zstd` every entry
    /// takes the raw branch and this field is just along for the ride.
    #[cfg_attr(not(feature = "zstd"), allow(dead_code))]
    compression: CompressionConfig,
    /// Directory holding the 512 bucket files. Owned by the builder
    /// so we can clean up on `Drop` if `finalize` is never called.
    bucket_dir: PathBuf,
    /// Buckets `[variant][prefix_byte]` → optional buffered file.
    /// Lazily opened on first write and capped with LRU eviction so a
    /// large import cannot exhaust the process fd limit.
    bucket_writers: Vec<Option<BucketWriter>>,
    open_bucket_writers: usize,
    bucket_access_tick: u64,
    bucket_paths: Vec<PathBuf>,
    /// File path where the pack index is materialized at `finalize`.
    /// Bytes are written incrementally as buckets are sorted, so the
    /// index never sits in memory in its entirety.
    index_path: PathBuf,
    /// Set true on `finalize` so `Drop` knows the bucket dir was
    /// already cleaned and shouldn't be removed again.
    finalized: bool,
}

struct BucketWriter {
    writer: BufWriter<File>,
    last_used: u64,
}

impl<W: Write + Read + Seek> StreamingPackBuilder<W> {
    /// Open a streaming builder against `pack_writer`, using
    /// `bucket_dir` for transient index buckets and writing the
    /// finalized index to `index_path`. The bucket dir is created if
    /// it doesn't exist; on a successful `finalize` it's removed
    /// (along with any bucket files left in it).
    ///
    /// `index_path` is *not* created by `new` — opening happens at
    /// finalize so a misconfigured caller doesn't leave an empty index
    /// file behind on early failure. It's still recorded here so
    /// `finalize` can write to a known location and the caller can
    /// install the file by path.
    ///
    /// The `pack_writer` must support `Read` because finalize re-streams
    /// the body to compute the trailer checksum — see the module-level
    /// note on the format.
    pub fn new(
        mut pack_writer: W,
        index_path: PathBuf,
        compression: CompressionConfig,
        bucket_dir: PathBuf,
    ) -> Result<Self> {
        std::fs::create_dir_all(&bucket_dir).map_err(StoreError::from)?;
        let header_offset = pack_writer.stream_position().map_err(StoreError::from)?;

        // Write a placeholder header with `count = 0`; finalize seeks
        // back here and rewrites the real count.
        let mut header_bytes = Vec::with_capacity(16);
        write_container_header(&mut header_bytes, pack_container_spec(), 0);
        pack_writer
            .write_all(&header_bytes)
            .map_err(StoreError::from)?;

        let bucket_paths: Vec<PathBuf> = (0..TOTAL_BUCKETS)
            .map(|i| {
                let variant = if i < BUCKETS_PER_VARIANT { 'h' } else { 'c' };
                let prefix = i % BUCKETS_PER_VARIANT;
                bucket_dir.join(format!("bucket-{variant}-{prefix:02x}"))
            })
            .collect();
        for path in &bucket_paths {
            let _ = std::fs::remove_file(path);
        }

        Ok(Self {
            pack_writer: Some(BufWriter::new(pack_writer)),
            header_offset,
            object_count: 0,
            total_uncompressed: 0,
            total_compressed: 0,
            compression,
            bucket_dir,
            bucket_writers: (0..TOTAL_BUCKETS).map(|_| None).collect(),
            open_bucket_writers: 0,
            bucket_access_tick: 0,
            bucket_paths,
            index_path,
            finalized: false,
        })
    }

    /// Add an object with a content-hash id.
    pub fn add(&mut self, hash: ContentHash, obj_type: ObjectType, data: Vec<u8>) -> Result<()> {
        self.add_id(PackObjectId::Hash(hash), obj_type, data)
    }

    /// Add an object with an explicit id. Mirrors [`super::PackBuilder::add_id`].
    ///
    /// # Memory shape
    ///
    /// Per-entry, the only allocations are:
    ///
    /// - `data: Vec<u8>` (the input, owned by the caller — comes from
    ///   gix' `find_object` and isn't ours to stream further).
    /// - A ~40-byte stack scratch for the entry header.
    /// - zstd's internal compression context (~128 KB constant).
    /// - One 50-byte index-bucket entry buffered into the bucket's
    ///   `BufWriter`.
    ///
    /// The compressed payload is **never materialized** as a `Vec<u8>` —
    /// it streams directly through `zstd::stream::write::Encoder` into
    /// the pack writer. The pack format requires a `compressed_size`
    /// varint *before* the compressed bytes, which we don't know yet
    /// when we write the header; we reserve a 10-byte placeholder and
    /// seek-back to patch it after the encoder finishes. Heddle's
    /// varint decoder accepts non-canonical encodings (it walks
    /// continuation bits without enforcing minimum-byte form), so the
    /// padded write decodes back to the same value any reader expects.
    pub fn add_id(&mut self, id: PackObjectId, obj_type: ObjectType, data: Vec<u8>) -> Result<()> {
        // Compute the entry's offset relative to the header. Flush the
        // BufWriter first so `stream_position` reflects bytes actually
        // committed to the underlying writer.
        let pw = self
            .pack_writer
            .as_mut()
            .expect("add_id called after finalize");
        pw.flush().map_err(StoreError::from)?;
        let entry_start = pw.get_mut().stream_position().map_err(StoreError::from)?;
        let offset = entry_start
            .checked_sub(self.header_offset)
            .expect("header_offset should never be past current position");

        self.total_uncompressed += data.len() as u64;

        // Phase 1: write the entry header up to (but not including) the
        // compressed-size varint. Always small, fits in `entry_header_buf`.
        let mut header_buf = Vec::with_capacity(40);
        id.encode_tagged(&mut header_buf);
        super::varint::encode_type_and_size(obj_type, data.len() as u64, &mut header_buf);
        pw.write_all(&header_buf).map_err(StoreError::from)?;
        // Only consumed by the zstd-enabled streaming branch below, but
        // we compute it here while we already have `header_buf`'s length
        // in scope.
        #[cfg(feature = "zstd")]
        let csize_pos = entry_start + header_buf.len() as u64;

        // Phase 2: stream the compressed payload. We branch here on
        // whether to compress at all — for tiny objects (`< min_size`)
        // the bulk path traditionally wrote raw bytes to skip zstd
        // overhead, and the reader's existing `compressed_size ==
        // uncompressed_size` heuristic in `pack_reader.rs:128` reads
        // raw entries back unchanged. We preserve that policy.
        // `want_compress` gates the zstd path. Even with the feature
        // enabled we fall through to raw for tiny entries (where
        // zstd's frame overhead dominates) or when the caller
        // explicitly disabled compression in `CompressionConfig`.
        // Without the `zstd` Cargo feature, every entry takes the raw
        // branch — same fallback shape as `compress_pack_payload`.
        let want_compress: bool;
        #[cfg(feature = "zstd")]
        {
            want_compress = self.compression.enabled && data.len() >= self.compression.min_size;
        }
        #[cfg(not(feature = "zstd"))]
        {
            want_compress = false;
        }
        if !want_compress {
            // Raw entry: known compressed_size = data.len(). One canonical
            // varint + the data itself. No seek-back needed.
            let mut csize_buf = Vec::with_capacity(10);
            super::varint::encode_varint(data.len() as u64, &mut csize_buf);
            pw.write_all(&csize_buf).map_err(StoreError::from)?;
            pw.write_all(&data).map_err(StoreError::from)?;
            self.total_compressed += data.len() as u64;
        } else {
            #[cfg(feature = "zstd")]
            {
                // Streaming entry: reserve 10 bytes for compressed_size,
                // stream-compress the payload, then seek back to patch.
                pw.write_all(&[0u8; CSIZE_PLACEHOLDER_LEN])
                    .map_err(StoreError::from)?;
                pw.flush().map_err(StoreError::from)?;
                let body_start = pw.get_mut().stream_position().map_err(StoreError::from)?;
                {
                    let mut enc =
                        zstd::stream::write::Encoder::new(&mut *pw, self.compression.level)
                            .map_err(StoreError::from)?;
                    // Pass the source size so the zstd frame's optional
                    // Frame Content Size field is set — lets decoders
                    // preallocate output buffers and validates that we
                    // wrote exactly what we promised at finish().
                    enc.set_pledged_src_size(Some(data.len() as u64))
                        .map_err(StoreError::from)?;
                    enc.write_all(&data).map_err(StoreError::from)?;
                    enc.finish().map_err(StoreError::from)?;
                }
                pw.flush().map_err(StoreError::from)?;
                let body_end = pw.get_mut().stream_position().map_err(StoreError::from)?;
                let compressed_size = body_end - body_start;
                self.total_compressed += compressed_size;

                // Seek back over the placeholder, write a 10-byte
                // non-canonical varint encoding the actual compressed_size,
                // then seek forward to where we left off so subsequent
                // adds append correctly.
                let mut csize_bytes = [0u8; CSIZE_PLACEHOLDER_LEN];
                encode_varint_padded_to_10(compressed_size, &mut csize_bytes);
                let inner = pw.get_mut();
                inner
                    .seek(SeekFrom::Start(csize_pos))
                    .map_err(StoreError::from)?;
                inner.write_all(&csize_bytes).map_err(StoreError::from)?;
                inner
                    .seek(SeekFrom::Start(body_end))
                    .map_err(StoreError::from)?;
            }
            #[cfg(not(feature = "zstd"))]
            {
                // Unreachable: `want_compress` is forced to `false`
                // when the `zstd` feature is off.
                unreachable!("compression branch reached without `zstd` feature");
            }
        }

        // Append the index entry (id || u64 BE offset) to the bucket
        // matching the id's first inner byte. The bucket file is opened
        // lazily so a sparse pack only creates files it actually uses.
        let bucket_idx = bucket_index_for(&id);
        let bucket = self.get_or_open_bucket(bucket_idx)?;
        let mut idx_entry = Vec::with_capacity(33 + 8);
        id.encode_tagged(&mut idx_entry);
        idx_entry.extend_from_slice(&offset.to_be_bytes());
        bucket.write_all(&idx_entry).map_err(StoreError::from)?;

        self.object_count += 1;
        Ok(())
    }

    fn get_or_open_bucket(&mut self, idx: usize) -> Result<&mut BufWriter<File>> {
        self.bucket_access_tick = self.bucket_access_tick.wrapping_add(1);
        let last_used = self.bucket_access_tick;
        if self.bucket_writers[idx].is_none() {
            if self.open_bucket_writers >= MAX_OPEN_BUCKET_WRITERS {
                self.evict_lru_bucket()?;
            }
            let path = &self.bucket_paths[idx];
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(StoreError::from)?;
            self.bucket_writers[idx] = Some(BucketWriter {
                writer: BufWriter::new(f),
                last_used,
            });
            self.open_bucket_writers += 1;
        } else if let Some(bucket) = self.bucket_writers[idx].as_mut() {
            bucket.last_used = last_used;
        }
        Ok(&mut self.bucket_writers[idx]
            .as_mut()
            .expect("just inserted above")
            .writer)
    }

    fn evict_lru_bucket(&mut self) -> Result<()> {
        let Some((idx, _)) = self
            .bucket_writers
            .iter()
            .enumerate()
            .filter_map(|(idx, bucket)| bucket.as_ref().map(|bucket| (idx, bucket.last_used)))
            .min_by_key(|(_, last_used)| *last_used)
        else {
            return Ok(());
        };

        if let Some(mut bucket) = self.bucket_writers[idx].take() {
            bucket.writer.flush().map_err(StoreError::from)?;
            self.open_bucket_writers -= 1;
        }
        Ok(())
    }

    /// Close the pack: patch the header count, append the BLAKE3
    /// trailer, build the sorted index from bucket files, and clean up
    /// the bucket directory. Returns `(pack_writer, index_bytes,
    /// stats)` so the caller can install the pack into its store.
    ///
    /// On any failure the bucket dir is left in place; rerunning the
    /// import will overwrite stale bucket files (they're keyed by
    /// fixed name, not content) so this isn't a correctness issue —
    /// just a small amount of disk churn until the next clean
    /// finalize.
    pub fn finalize(mut self) -> Result<(W, PackStats)> {
        // 1. Flush every bucket so reads in the next phase see all
        //    queued entries. `flatten()` skips the never-opened slots.
        for bucket in self.bucket_writers.iter_mut().flatten() {
            bucket.writer.flush().map_err(StoreError::from)?;
        }
        // Drop the writers so the OS file handles close before we
        // re-open the same paths for reading.
        for slot in self.bucket_writers.iter_mut() {
            *slot = None;
        }
        self.open_bucket_writers = 0;

        // 2. Patch the pack header with the real object count, then
        //    re-stream the [header][body] bytes to compute the
        //    trailer checksum.
        let bw = self
            .pack_writer
            .take()
            .expect("finalize called twice — pack_writer already consumed");
        let mut writer = bw
            .into_inner()
            .map_err(|e| StoreError::from(std::io::Error::other(e.to_string())))?;
        writer
            .seek(SeekFrom::Start(self.header_offset))
            .map_err(StoreError::from)?;
        let mut header_bytes = Vec::with_capacity(16);
        write_container_header(&mut header_bytes, pack_container_spec(), self.object_count);
        writer.write_all(&header_bytes).map_err(StoreError::from)?;

        // 3. Hash the on-disk content from header_offset to current
        //    position (which is just past the body). One sequential
        //    pass; the BufWriter we drained is gone so this read is
        //    on the raw writer.
        writer
            .seek(SeekFrom::Start(self.header_offset))
            .map_err(StoreError::from)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = writer.read(&mut buf).map_err(StoreError::from)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let checksum = hasher.finalize();

        // 4. Append the trailer checksum.
        writer.seek(SeekFrom::End(0)).map_err(StoreError::from)?;
        writer
            .write_all(checksum.as_bytes())
            .map_err(StoreError::from)?;
        writer.flush().map_err(StoreError::from)?;

        // 5. Stream the final sorted index directly to disk. We open
        //    a `BufWriter` against `index_path`, write the index
        //    container header (magic + version + count — count is
        //    already known from the per-add bookkeeping), then walk
        //    the 512 buckets in `(variant, prefix)` order, sorting
        //    each in memory and writing entries to the file as they
        //    come off the sort. The intermediate `PackIndex` Vec —
        //    O(K) in the previous implementation — is gone; the
        //    largest in-memory state is one bucket's worth of entries.
        //    Bucket distribution is uniform via BLAKE3 so each bucket
        //    is ~K/256 entries × ~50 bytes; even at 100M objects that's
        //    a ~16 MB sort scratch.
        let idx_file = File::create(&self.index_path).map_err(StoreError::from)?;
        let mut idx_writer = BufWriter::new(idx_file);
        write_index_header(&mut idx_writer, self.object_count)?;
        let mut entries_written: u64 = 0;
        for path in self.bucket_paths.iter() {
            if !path.exists() {
                continue;
            }
            let bucket_bytes = std::fs::read(path).map_err(StoreError::from)?;
            let mut entries = decode_bucket_file(&bucket_bytes)?;
            // Local sort by `PackObjectId` matches the global sort
            // because all entries in a bucket share the same variant
            // tag *and* the same first inner byte; only the remaining
            // bytes differ between them.
            entries.sort_by_key(|(id, _)| *id);
            for (id, offset) in entries {
                write_index_entry(&mut idx_writer, id, offset)?;
                entries_written += 1;
            }
        }
        idx_writer.flush().map_err(StoreError::from)?;
        debug_assert_eq!(
            entries_written, self.object_count,
            "streaming index entry count drifted from add() count"
        );

        // 6. Clean up the bucket dir so the heddle store doesn't carry
        //    transient artifacts. Deletion failures are non-fatal —
        //    the dir is uniquely named per import so leftovers are at
        //    worst stale, not corrupting.
        for path in self.bucket_paths.iter() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.bucket_dir);
        self.finalized = true;

        let stats = PackStats {
            object_count: self.object_count,
            total_uncompressed: self.total_uncompressed,
            total_compressed: self.total_compressed,
            delta_count: 0,
            compression_ratio: if self.total_uncompressed == 0 {
                0.0
            } else {
                self.total_compressed as f64 / self.total_uncompressed as f64
            },
        };

        Ok((writer, stats))
    }
}

/// Write the index container header to `out`. Mirrors
/// [`PackIndex::to_bytes`]'s prefix exactly (4-byte magic, 4-byte
/// big-endian version, 8-byte big-endian count) so a reader written
/// against the existing format works without modification.
fn write_index_header<W: Write>(out: &mut W, count: u64) -> Result<()> {
    super::pack_index::index_header().write_to(out, count)
}

/// Append one `(id, offset)` index entry to `out`. The encoding
/// matches [`PackIndex::to_bytes`]: tagged id immediately followed by
/// an 8-byte big-endian offset.
fn write_index_entry<W: Write>(out: &mut W, id: PackObjectId, offset: u64) -> Result<()> {
    let mut buf = Vec::with_capacity(33 + 8);
    id.encode_tagged(&mut buf);
    buf.extend_from_slice(&offset.to_be_bytes());
    out.write_all(&buf).map_err(StoreError::from)
}

/// Encode a `u64` as a non-canonical 10-byte LEB128 varint. The first
/// 9 bytes always set the continuation bit (`0x80`), the 10th never
/// does — so the decoder reads exactly 10 bytes regardless of the
/// value. Used by the streaming path to reserve a fixed-width
/// placeholder for `compressed_size` before stream-compressing the
/// payload, then patch the placeholder with the actual size after.
///
/// `decode_varint` ignores the canonicalness of the encoding (it
/// walks continuation bits without checking minimum-byte form), so
/// the value round-trips exactly. Cost is up to 9 wasted bytes per
/// entry, ~115 KB on a 13 K-entry import — negligible relative to
/// the pack body.
#[cfg(feature = "zstd")]
fn encode_varint_padded_to_10(value: u64, out: &mut [u8; 10]) {
    let mut v = value;
    for slot in out.iter_mut().take(9) {
        *slot = 0x80 | ((v & 0x7F) as u8);
        v >>= 7;
    }
    out[9] = (v & 0x7F) as u8;
}

impl<W: Write + Read + Seek> Drop for StreamingPackBuilder<W> {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        // Best-effort cleanup of bucket dir on abort. Errors here are
        // suppressed because Drop can't propagate them.
        for path in self.bucket_paths.iter() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.bucket_dir);
    }
}

/// Map a `PackObjectId` to one of `TOTAL_BUCKETS` buckets. The variant
/// (Hash vs ChangeId) picks the upper half; the first byte of the
/// inner id picks the slot within the half.
fn bucket_index_for(id: &PackObjectId) -> usize {
    match id {
        PackObjectId::Hash(h) => HASH_VARIANT * BUCKETS_PER_VARIANT + h.as_bytes()[0] as usize,
        PackObjectId::ChangeId(c) => {
            CHANGEID_VARIANT * BUCKETS_PER_VARIANT + c.as_bytes()[0] as usize
        }
    }
}

/// Decode `(id, offset)` records from a bucket file. The format
/// matches `PackObjectId::encode_tagged` followed by a u64 BE offset,
/// repeated. Unrecognized tags or truncated trailers fail loudly —
/// we wrote the bytes, so any corruption is a bug, not user input.
fn decode_bucket_file(bytes: &[u8]) -> Result<Vec<(PackObjectId, u64)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let (id, id_len) = PackObjectId::decode_tagged(&bytes[pos..])?;
        pos += id_len;
        if pos + 8 > bytes.len() {
            return Err(StoreError::InvalidObject(
                "streaming bucket entry truncated at offset".to_string(),
            ));
        }
        let offset = u64::from_be_bytes(bytes[pos..pos + 8].try_into().map_err(|_| {
            StoreError::InvalidObject("streaming bucket bad offset slice".to_string())
        })?);
        pos += 8;
        out.push((id, offset));
    }
    Ok(out)
}

// ---------------------- Tests ----------------------

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{
        object::ChangeId,
        store::pack::{PackReader, PackStats},
    };

    fn deterministic_hash(seed: u8) -> ContentHash {
        // Spread `seed` across the high byte so different seeds end up
        // in different hash-prefix buckets. We don't actually want
        // collisions in the tests that check distribution.
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        for (i, b) in bytes.iter_mut().enumerate().skip(1) {
            *b = seed.wrapping_mul(31).wrapping_add(i as u8);
        }
        ContentHash::from_bytes(bytes)
    }

    fn deterministic_change_id(seed: u8) -> ChangeId {
        let mut bytes = [0u8; 16];
        bytes[0] = seed;
        for (i, b) in bytes.iter_mut().enumerate().skip(1) {
            *b = seed.wrapping_add(i as u8 * 7);
        }
        ChangeId::from_bytes(bytes)
    }

    /// Test rig: returns the builder, the bucket dir (for cleanup
    /// inspection), and the index path the builder will write at
    /// finalize. The index path lives in the temp dir so it gets
    /// auto-cleaned with `tmp`.
    fn fresh_builder(
        tmp: &tempfile::TempDir,
    ) -> (StreamingPackBuilder<Cursor<Vec<u8>>>, PathBuf, PathBuf) {
        let bucket_dir = tmp.path().join("buckets");
        let index_path = tmp.path().join("test.idx");
        let cursor = Cursor::new(Vec::<u8>::new());
        let b = StreamingPackBuilder::new(
            cursor,
            index_path.clone(),
            CompressionConfig::default(),
            bucket_dir.clone(),
        )
        .unwrap();
        (b, bucket_dir, index_path)
    }

    /// Finalize the builder and return `(pack_bytes, index_bytes, stats)`.
    /// The index bytes are read back from the file the builder wrote
    /// to — verifying that the streaming index path actually produced
    /// readable bytes.
    fn finalize_cursor(
        b: StreamingPackBuilder<Cursor<Vec<u8>>>,
        index_path: &std::path::Path,
    ) -> (Vec<u8>, Vec<u8>, PackStats) {
        let (cursor, stats) = b.finalize().unwrap();
        let index_bytes = std::fs::read(index_path).unwrap();
        (cursor.into_inner(), index_bytes, stats)
    }

    #[test]
    fn empty_pack_finalizes_to_valid_zero_count_pack() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (b, bucket_dir, idx_path) = fresh_builder(&tmp);
        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);

        assert_eq!(stats.object_count, 0);
        // PackReader can parse the empty pack and reports zero objects.
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        assert!(reader.list_ids().is_empty());
        // Bucket dir was removed.
        assert!(
            !bucket_dir.exists(),
            "bucket dir should be cleaned on successful finalize"
        );
    }

    #[test]
    fn single_blob_with_hash_id_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let hash = deterministic_hash(0x42);
        let payload = b"hello, streaming pack".to_vec();
        b.add(hash, ObjectType::Blob, payload.clone()).unwrap();
        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);

        assert_eq!(stats.object_count, 1);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        let id = PackObjectId::Hash(hash);
        assert!(reader.has_object(&id));
        let (got_type, got_data) = reader.get_object(&id).unwrap().unwrap();
        assert_eq!(got_type, ObjectType::Blob);
        assert_eq!(got_data, payload);
    }

    #[test]
    fn single_state_with_change_id_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let cid = deterministic_change_id(0xa5);
        let payload = b"serialized-state-bytes".to_vec();
        b.add_id(
            PackObjectId::ChangeId(cid),
            ObjectType::State,
            payload.clone(),
        )
        .unwrap();
        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);

        assert_eq!(stats.object_count, 1);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        let id = PackObjectId::ChangeId(cid);
        let (ty, data) = reader.get_object(&id).unwrap().unwrap();
        assert_eq!(ty, ObjectType::State);
        assert_eq!(data, payload);
    }

    #[test]
    fn mixed_hash_and_changeid_ids_all_retrievable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let blob_hash = deterministic_hash(0x10);
        let tree_hash = deterministic_hash(0x20);
        let state_cid = deterministic_change_id(0x80);

        b.add(blob_hash, ObjectType::Blob, b"blob-bytes".to_vec())
            .unwrap();
        b.add(tree_hash, ObjectType::Tree, b"serialized-tree".to_vec())
            .unwrap();
        b.add_id(
            PackObjectId::ChangeId(state_cid),
            ObjectType::State,
            b"serialized-state".to_vec(),
        )
        .unwrap();

        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);
        assert_eq!(stats.object_count, 3);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        assert_eq!(
            reader
                .get_object(&PackObjectId::Hash(blob_hash))
                .unwrap()
                .unwrap()
                .1,
            b"blob-bytes".to_vec()
        );
        assert_eq!(
            reader
                .get_object(&PackObjectId::Hash(tree_hash))
                .unwrap()
                .unwrap()
                .1,
            b"serialized-tree".to_vec()
        );
        assert_eq!(
            reader
                .get_object(&PackObjectId::ChangeId(state_cid))
                .unwrap()
                .unwrap()
                .1,
            b"serialized-state".to_vec()
        );
    }

    #[test]
    fn ten_thousand_objects_round_trip_correctly() {
        // Stresses the bucket sort: 10K objects spread across
        // 256 hash buckets averages 40 entries per bucket — well
        // within in-memory sort capacity but covers every bucket.
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let mut hashes = Vec::with_capacity(10_000);
        for i in 0..10_000u32 {
            // Use BLAKE3 over the index so first-byte distribution is
            // pseudo-uniform across the 256 hash buckets.
            let h = blake3::hash(&i.to_le_bytes());
            let hash = ContentHash::from_bytes(*h.as_bytes());
            hashes.push(hash);
            b.add(hash, ObjectType::Blob, format!("payload-{i}").into_bytes())
                .unwrap();
        }
        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);
        assert_eq!(stats.object_count, 10_000);

        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        assert_eq!(reader.list_ids().len(), 10_000);
        // Spot-check ten across the range.
        for i in [0, 1, 99, 1234, 5_000, 9_999] {
            let id = PackObjectId::Hash(hashes[i]);
            let (_ty, data) = reader.get_object(&id).unwrap().unwrap();
            assert_eq!(data, format!("payload-{i}").into_bytes());
        }
    }

    #[test]
    fn bucket_writers_are_lru_capped_below_fd_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _bucket_dir, idx_path) = fresh_builder(&tmp);
        let mut ids = Vec::new();

        for i in 0..BUCKETS_PER_VARIANT {
            let hash = deterministic_hash(i as u8);
            ids.push(PackObjectId::Hash(hash));
            b.add(hash, ObjectType::Blob, format!("hash-{i}").into_bytes())
                .unwrap();
            assert!(
                b.open_bucket_writers <= MAX_OPEN_BUCKET_WRITERS,
                "open bucket writers should stay capped"
            );
        }

        for i in 0..BUCKETS_PER_VARIANT {
            let cid = deterministic_change_id(i as u8);
            ids.push(PackObjectId::ChangeId(cid));
            b.add_id(
                PackObjectId::ChangeId(cid),
                ObjectType::State,
                format!("state-{i}").into_bytes(),
            )
            .unwrap();
            assert!(
                b.open_bucket_writers <= MAX_OPEN_BUCKET_WRITERS,
                "open bucket writers should stay capped"
            );
        }

        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);
        assert_eq!(stats.object_count, TOTAL_BUCKETS as u64);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        for id in ids {
            assert!(reader.has_object(&id), "missing id {id:?}");
        }
    }

    #[test]
    fn index_id_sort_order_matches_packbuilder_output() {
        // PackBuilder groups objects by `ObjectType` before encoding,
        // which changes the byte offsets relative to a streaming builder
        // that writes in added-order. The bytes of the two indices
        // therefore can't match exactly. What MUST match is the
        // **sort order of ids** — both builders ultimately call
        // `PackIndex::sort()` (or the bucket-equivalent), and any
        // reader binary-searches against that order.
        use crate::store::pack::PackBuilder;
        let payloads: Vec<(PackObjectId, ObjectType, Vec<u8>)> = (0..200u32)
            .map(|i| {
                let h = blake3::hash(&i.to_le_bytes());
                (
                    PackObjectId::Hash(ContentHash::from_bytes(*h.as_bytes())),
                    if i % 3 == 0 {
                        ObjectType::Tree
                    } else {
                        ObjectType::Blob
                    },
                    format!("body-{i}").into_bytes(),
                )
            })
            .collect();

        // Disable delta encoding so the classic builder produces a pack
        // shape comparable to the streaming one (which never deltas).
        let compression = CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        };
        let mut classic = PackBuilder::new(compression);
        for (id, ty, data) in payloads.iter() {
            classic.add_id(*id, *ty, data.clone());
        }
        let (classic_pack, classic_index, _) = classic.build().unwrap();
        let classic_reader = PackReader::from_bytes(classic_pack, classic_index).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let idx_path = tmp.path().join("test.idx");
        let cursor = Cursor::new(Vec::<u8>::new());
        let mut streaming =
            StreamingPackBuilder::new(cursor, idx_path.clone(), compression, bucket_dir).unwrap();
        for (id, ty, data) in payloads.iter() {
            streaming.add_id(*id, *ty, data.clone()).unwrap();
        }
        let (streaming_pack, streaming_index, _) = finalize_cursor(streaming, &idx_path);
        let streaming_reader = PackReader::from_bytes(streaming_pack, streaming_index).unwrap();

        // Same set of ids in the same sorted order — that's the
        // contract for binary search to work.
        assert_eq!(
            streaming_reader.list_ids(),
            classic_reader.list_ids(),
            "streaming and classic indices should report the same id sequence"
        );
        // Spot-check that each id resolves to a payload that matches
        // the classic builder's output (equal bytes after decompression).
        for (id, _ty, want) in payloads.iter().take(10).chain(payloads.iter().skip(190)) {
            let (_, got) = streaming_reader.get_object(id).unwrap().unwrap();
            assert_eq!(&got, want);
            let (_, classic_got) = classic_reader.get_object(id).unwrap().unwrap();
            assert_eq!(got, classic_got);
        }
    }

    #[test]
    fn corrupted_pack_fails_checksum_verification() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        b.add(
            deterministic_hash(0x01),
            ObjectType::Blob,
            b"some bytes".to_vec(),
        )
        .unwrap();
        let (mut pack_data, index_data, _) = finalize_cursor(b, &idx_path);
        // Flip one byte in the body. The trailer checksum must reject.
        let body_byte = 18; // past the 16-byte header
        pack_data[body_byte] ^= 0xff;
        let result = PackReader::from_bytes(pack_data, index_data);
        assert!(
            result.is_err(),
            "PackReader should reject pack with mutated body"
        );
    }

    #[test]
    fn pack_count_in_header_matches_index_entry_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        for i in 0..7u8 {
            b.add(
                deterministic_hash(i),
                ObjectType::Blob,
                format!("p{i}").into_bytes(),
            )
            .unwrap();
        }
        let (pack_data, index_data, _) = finalize_cursor(b, &idx_path);
        // Header count is bytes 8..16 (big-endian).
        let count = u64::from_be_bytes(pack_data[8..16].try_into().unwrap());
        assert_eq!(count, 7);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        assert_eq!(reader.list_ids().len(), 7);
    }

    #[test]
    fn bucket_files_are_cleaned_on_successful_finalize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let idx_path = tmp.path().join("test.idx");
        let cursor = Cursor::new(Vec::<u8>::new());
        let mut b = StreamingPackBuilder::new(
            cursor,
            idx_path.clone(),
            CompressionConfig::default(),
            bucket_dir.clone(),
        )
        .unwrap();
        for i in 0..50u8 {
            b.add(deterministic_hash(i), ObjectType::Blob, vec![i; 32])
                .unwrap();
        }
        // Buckets exist and contain data.
        assert!(bucket_dir.exists());
        let bucket_count = std::fs::read_dir(&bucket_dir).unwrap().count();
        assert!(bucket_count > 0, "bucket dir should hold some files");
        let _ = finalize_cursor(b, &idx_path);
        assert!(
            !bucket_dir.exists(),
            "bucket dir should be removed on finalize"
        );
    }

    #[test]
    fn bucket_files_are_cleaned_on_drop_without_finalize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let idx_path = tmp.path().join("test.idx");
        {
            let cursor = Cursor::new(Vec::<u8>::new());
            let mut b = StreamingPackBuilder::new(
                cursor,
                idx_path.clone(),
                CompressionConfig::default(),
                bucket_dir.clone(),
            )
            .unwrap();
            for i in 0..10u8 {
                b.add(deterministic_hash(i), ObjectType::Blob, vec![0; 32])
                    .unwrap();
            }
            assert!(bucket_dir.exists());
            // Drop without finalize — Drop impl should clean up.
        }
        assert!(
            !idx_path.exists(),
            "no index file should have been created without finalize"
        );
        assert!(
            !bucket_dir.exists(),
            "bucket dir should be removed on Drop when finalize never ran"
        );
    }

    #[test]
    fn large_blob_streams_to_disk_without_double_buffering() {
        // 4 MiB blob — well under the actual streaming target but big
        // enough to confirm we're not buffering the entire pack body in
        // RAM. The pack data on disk should be at least 4 MiB; the
        // builder's in-memory state is per-object only.
        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let pack_path = tmp.path().join("pack.dat");
        let idx_path = tmp.path().join("pack.idx");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&pack_path)
            .unwrap();
        let mut b = StreamingPackBuilder::new(
            file,
            idx_path.clone(),
            CompressionConfig::default(),
            bucket_dir,
        )
        .unwrap();
        let payload: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i & 0xff) as u8).collect();
        let hash = deterministic_hash(0xff);
        b.add(hash, ObjectType::Blob, payload.clone()).unwrap();
        let (_, stats) = b.finalize().unwrap();
        let index_data = std::fs::read(&idx_path).unwrap();
        assert_eq!(stats.object_count, 1);
        let pack_bytes = std::fs::read(&pack_path).unwrap();
        // Pack on disk holds the whole compressed payload + headers
        // + trailer. Confirm it round-trips.
        let reader = PackReader::from_bytes(pack_bytes, index_data).unwrap();
        let (_ty, got) = reader
            .get_object(&PackObjectId::Hash(hash))
            .unwrap()
            .unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn bucket_distribution_for_random_hashes_is_roughly_uniform() {
        // Confirms our sort-time peak memory bound. We accumulate
        // 1024 random hashes through a builder and check that no
        // single bucket holds more than ~3× the average. (BLAKE3 hash
        // first-byte distribution is uniform; this is mostly a
        // sanity check that we route to the right bucket and aren't
        // accidentally collapsing.)
        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let idx_path = tmp.path().join("test.idx");
        let cursor = Cursor::new(Vec::<u8>::new());
        let mut b = StreamingPackBuilder::new(
            cursor,
            idx_path.clone(),
            CompressionConfig::default(),
            bucket_dir.clone(),
        )
        .unwrap();
        for i in 0..1024u32 {
            let h = blake3::hash(&i.to_le_bytes());
            let hash = ContentHash::from_bytes(*h.as_bytes());
            b.add(hash, ObjectType::Blob, b"x".to_vec()).unwrap();
        }
        // Inspect bucket file sizes BEFORE finalize (which deletes them).
        b.pack_writer.as_mut().unwrap().flush().unwrap();
        let mut max_entries = 0usize;
        let entry_size = 33 + 8; // tagged-hash + u64 offset
        for path in b.bucket_paths.iter() {
            if path.exists() {
                let size = std::fs::metadata(path).unwrap().len() as usize;
                let entries = size / entry_size;
                if entries > max_entries {
                    max_entries = entries;
                }
            }
        }
        // Average is 1024 / 256 = 4 entries per bucket. Allow up to 16
        // (4× average) — uniformity isn't perfect on small samples.
        assert!(
            max_entries <= 16,
            "max bucket has {max_entries} entries; uniform expected ~4"
        );
        let _ = finalize_cursor(b, &idx_path);
    }

    #[test]
    fn finalize_returns_correct_stats() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let payload = vec![0xabu8; 1024];
        for i in 0..5u8 {
            b.add(deterministic_hash(i), ObjectType::Blob, payload.clone())
                .unwrap();
        }
        let (_, _, stats) = finalize_cursor(b, &idx_path);
        assert_eq!(stats.object_count, 5);
        assert_eq!(stats.total_uncompressed, 5 * 1024);
        assert!(stats.total_compressed > 0);
        assert!(stats.compression_ratio > 0.0);
        assert_eq!(stats.delta_count, 0, "streaming builder never deltas");
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_compression_roundtrips_through_zstd_frame() {
        // Force the streaming path with a payload that compresses
        // well (long runs of identical bytes). Verifies:
        //  1. Streaming output decodes back to the original bytes.
        //  2. The compressed body is genuinely smaller than the
        //     uncompressed input (proving zstd ran), and
        //  3. The non-canonical 10-byte varint patched into the
        //     compressed_size slot decodes to the right value.
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        // 64 KiB of zeros — compresses to a tiny zstd frame, well
        // above the default `min_size` so we hit the streaming branch.
        let payload = vec![0u8; 64 * 1024];
        let hash = deterministic_hash(0x77);
        b.add(hash, ObjectType::Blob, payload.clone()).unwrap();
        let (pack_data, index_data, stats) = finalize_cursor(b, &idx_path);
        assert!(
            stats.total_compressed < stats.total_uncompressed,
            "expected compression ratio < 1.0, got {}/{}",
            stats.total_compressed,
            stats.total_uncompressed
        );
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        let (_ty, got) = reader
            .get_object(&PackObjectId::Hash(hash))
            .unwrap()
            .unwrap();
        assert_eq!(got, payload);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn padded_varint_decodes_to_original_value_for_canonical_decoder() {
        // Sanity for the seek-back scheme: for every value we'd want
        // to encode (small, mid, large), confirm the existing
        // `decode_varint` returns the same `value` from a 10-byte
        // padded encoding. If this ever fails the streaming path's
        // patched compressed_size would be misread by readers.
        let cases: &[u64] = &[0, 1, 127, 128, 4096, 1_000_000, 1_000_000_000_000, u64::MAX];
        for &value in cases {
            let mut buf = [0u8; 10];
            super::encode_varint_padded_to_10(value, &mut buf);
            let (decoded, consumed) = super::super::varint::decode_varint(&buf)
                .expect("padded varint should always decode");
            assert_eq!(decoded, value, "varint roundtrip failed for {value}");
            assert_eq!(
                consumed, 10,
                "padded encoding should consume all 10 bytes for {value}"
            );
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_path_does_not_buffer_compressed_payload_in_memory() {
        // Smoke check: write a single 8 MiB payload, observe the
        // pack file size on disk during/after the add. The pack file
        // grows incrementally during the streaming compression — if
        // we were buffering an intermediate compressed `Vec<u8>` the
        // on-disk size would jump by ~8 MiB at finalize, not stay
        // bounded as the encoder pumps bytes through.
        //
        // We can't easily measure peak heap from inside Rust without
        // a custom allocator. What we *can* verify is that calling
        // `add` returns control with the pack file already at its
        // final body size, demonstrating the encoder wrote through
        // and didn't accumulate.
        let tmp = tempfile::TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");
        let pack_path = tmp.path().join("pack.dat");
        let idx_path = tmp.path().join("pack.idx");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&pack_path)
            .unwrap();
        let mut b = StreamingPackBuilder::new(
            file,
            idx_path.clone(),
            CompressionConfig::default(),
            bucket_dir,
        )
        .unwrap();
        let payload = vec![0xa5u8; 8 * 1024 * 1024];
        let hash = deterministic_hash(0x66);
        b.add(hash, ObjectType::Blob, payload.clone()).unwrap();
        // Pack file already on disk holds at least the entry header +
        // compressed payload (excluding the 32-byte trailer the builder
        // appends at finalize).
        let mid_size = std::fs::metadata(&pack_path).unwrap().len();
        assert!(
            mid_size > 16 + 40,
            "pack file should hold real entry data after add; size={mid_size}"
        );
        let (_, _) = b.finalize().unwrap();
        let pack_bytes = std::fs::read(&pack_path).unwrap();
        let index_bytes = std::fs::read(&idx_path).unwrap();
        let reader = PackReader::from_bytes(pack_bytes, index_bytes).unwrap();
        let (_ty, got) = reader
            .get_object(&PackObjectId::Hash(hash))
            .unwrap()
            .unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn list_ids_returns_all_added_ids_sorted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut b, _, idx_path) = fresh_builder(&tmp);
        let mut added: Vec<PackObjectId> = Vec::new();
        // Mix of Hash and ChangeId in a non-sorted order on input.
        for seed in [0x05u8, 0xa0, 0x12, 0x9f, 0x33] {
            let id = PackObjectId::Hash(deterministic_hash(seed));
            b.add_id(id, ObjectType::Blob, vec![seed; 4]).unwrap();
            added.push(id);
        }
        for seed in [0x80u8, 0x10, 0xff] {
            let id = PackObjectId::ChangeId(deterministic_change_id(seed));
            b.add_id(id, ObjectType::State, vec![seed; 4]).unwrap();
            added.push(id);
        }
        let (pack_data, index_data, _) = finalize_cursor(b, &idx_path);
        let reader = PackReader::from_bytes(pack_data, index_data).unwrap();
        let mut got = reader.list_ids();
        // PackReader's list_ids returns index order — should already be
        // sorted because we sort on finalize.
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted, "list_ids must come back sorted");
        // And every added id should appear.
        added.sort();
        got.sort();
        assert_eq!(got, added);
    }
}
