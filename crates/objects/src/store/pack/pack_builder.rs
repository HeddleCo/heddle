// SPDX-License-Identifier: Apache-2.0
//! Pack builder for creating packfiles.

use std::collections::{HashMap, VecDeque};

use super::{
    ObjectType, PackObjectId, PackObjectRecord, PackStats, append_container_checksum,
    compress_pack_payload, encode_tagged_entry, pack_container_spec, pack_index::PackIndex,
    write_container_header,
};
use crate::{
    delta::DeltaEncoder,
    object::ContentHash,
    store::{Result, compression::CompressionConfig},
};

const MIN_DELTA_SIZE: usize = 64;
/// Maximum depth for delta chains (matches Git's default).
const MAX_DELTA_CHAIN_DEPTH: usize = 50;
/// Number of recent objects to try as delta bases (Git default: 10).
const WINDOW_SIZE: usize = 10;

type GroupedPackMap = HashMap<ObjectType, Vec<PackObjectRecord>>;

/// Pack builder for creating packfiles.
pub struct PackBuilder {
    objects: Vec<PackObjectRecord>,
    compression: CompressionConfig,
}

/// A recent object in the sliding window, with cached hash index for fast delta estimation.
struct WindowEntry {
    hash: ContentHash,
    data: Vec<u8>,
    index: HashMap<[u8; 4], Vec<usize>>,
    chain_depth: usize,
}

impl PackBuilder {
    /// Create a new pack builder.
    pub fn new(compression: CompressionConfig) -> Self {
        Self {
            objects: Vec::new(),
            compression,
        }
    }

    /// Add an object to the pack.
    pub fn add(&mut self, hash: ContentHash, obj_type: ObjectType, data: Vec<u8>) {
        self.add_id(PackObjectId::Hash(hash), obj_type, data);
    }

    pub fn add_id(&mut self, id: PackObjectId, obj_type: ObjectType, data: Vec<u8>) {
        self.objects.push(PackObjectRecord {
            id,
            obj_type,
            data,
            delta_base: None,
            path_hint: None,
        });
    }

    /// Add an object with a path hint for better delta grouping.
    ///
    /// Objects sharing the same path (e.g. successive versions of `src/main.rs`)
    /// will be sorted together for delta encoding, producing much better
    /// compression ratios than size-only ordering.
    pub fn add_with_path(
        &mut self,
        hash: ContentHash,
        obj_type: ObjectType,
        data: Vec<u8>,
        path: Option<String>,
    ) {
        self.add_with_path_id(PackObjectId::Hash(hash), obj_type, data, path);
    }

    pub fn add_with_path_id(
        &mut self,
        id: PackObjectId,
        obj_type: ObjectType,
        data: Vec<u8>,
        path: Option<String>,
    ) {
        self.objects.push(PackObjectRecord {
            id,
            obj_type,
            data,
            delta_base: None,
            path_hint: path,
        });
    }

    /// Build the packfile and index.
    ///
    /// Returns the pack data, index data, and statistics.
    pub fn build(self) -> Result<(Vec<u8>, Vec<u8>, PackStats)> {
        let mut pack_data = Vec::new();
        let mut index = PackIndex::new();

        write_container_header(
            &mut pack_data,
            pack_container_spec(),
            self.objects.len() as u64,
        );

        let mut total_uncompressed = 0u64;
        let mut total_compressed = 0u64;
        let mut delta_count = 0u64;

        let object_count = self.objects.len() as u64;
        let grouped = Self::group_by_type(self.objects);

        for (obj_type, mut objects) in grouped {
            if objects.len() < 2
                || obj_type == ObjectType::State
                || self.compression.max_delta_size == 0
            {
                // Single objects, states, or transfer-tuned packs with delta disabled:
                // write entries directly without running the sliding delta search.
                for record in objects {
                    let offset = pack_data.len() as u64;
                    index.add(record.id, offset);

                    total_uncompressed += record.data.len() as u64;
                    let compressed = compress_pack_payload(&record.data, &self.compression)?;
                    total_compressed += compressed.len() as u64;

                    Self::write_entry(&mut pack_data, &record, obj_type, &compressed)?;
                }
            } else {
                Self::sort_for_delta_window(&mut objects);
                Self::encode_with_sliding_window(
                    &mut pack_data,
                    &mut index,
                    &mut total_uncompressed,
                    &mut total_compressed,
                    &mut delta_count,
                    obj_type,
                    objects,
                    &self.compression,
                )?;
            }
        }

        index.sort();

        append_container_checksum(&mut pack_data);

        let stats = PackStats {
            object_count,
            total_uncompressed,
            total_compressed,
            delta_count,
            compression_ratio: total_compressed as f64 / total_uncompressed as f64,
        };

        Ok((pack_data, index.to_bytes(), stats))
    }

    /// Sort objects for optimal delta window traversal.
    ///
    /// Sorts by: file extension → basename → size descending.
    /// This ensures files with the same extension are adjacent (like Git sorting
    /// `.rs` files together), within that group same-named files are adjacent,
    /// and within that the largest comes first (best delta base candidate).
    fn sort_for_delta_window(objects: &mut [PackObjectRecord]) {
        objects.sort_by(|a, b| {
            let key_a = Self::sort_key(&a.path_hint);
            let key_b = Self::sort_key(&b.path_hint);
            key_a.cmp(&key_b).then(b.data.len().cmp(&a.data.len()))
        });
    }

    /// Extract a sort key from a path: (extension, basename_without_extension).
    /// Objects without paths sort last.
    fn sort_key(path: &Option<String>) -> (String, String) {
        match path {
            Some(p) => {
                let filename = p.rsplit('/').next().unwrap_or(p);
                if let Some(dot_pos) = filename.rfind('.') {
                    let ext = filename[dot_pos + 1..].to_string();
                    let stem = filename[..dot_pos].to_string();
                    (ext, stem)
                } else {
                    (String::new(), filename.to_string())
                }
            }
            None => ("\u{FFFF}".to_string(), String::new()),
        }
    }

    /// Encode objects using a sliding window for delta base selection.
    ///
    /// For each object, tries delta encoding against the W most recent objects
    /// in the window, picking the base that produces the smallest delta. This
    /// is the same approach Git uses with `--window=10`.
    #[allow(clippy::too_many_arguments)]
    fn encode_with_sliding_window(
        pack_data: &mut Vec<u8>,
        index: &mut PackIndex,
        total_uncompressed: &mut u64,
        total_compressed: &mut u64,
        delta_count: &mut u64,
        obj_type: ObjectType,
        objects: Vec<PackObjectRecord>,
        compression: &CompressionConfig,
    ) -> Result<()> {
        let mut window: VecDeque<WindowEntry> = VecDeque::with_capacity(WINDOW_SIZE);

        for record in objects {
            let hash = match record.id {
                PackObjectId::Hash(hash) => hash,
                PackObjectId::ChangeId(_) => {
                    let offset = pack_data.len() as u64;
                    index.add(record.id, offset);
                    *total_uncompressed += record.data.len() as u64;
                    let compressed = compress_pack_payload(&record.data, compression)?;
                    *total_compressed += compressed.len() as u64;
                    Self::write_entry(pack_data, &record, obj_type, &compressed)?;
                    continue;
                }
            };
            let data = record.data;
            let offset = pack_data.len() as u64;
            index.add(PackObjectId::Hash(hash), offset);
            *total_uncompressed += data.len() as u64;

            // Try delta against each window entry, pick the best
            let mut best_base_idx: Option<usize> = None;
            let mut best_delta_estimate = usize::MAX;

            if data.len() >= MIN_DELTA_SIZE {
                for (i, entry) in window.iter().enumerate() {
                    // Skip if this base is already at max chain depth
                    if entry.chain_depth >= MAX_DELTA_CHAIN_DEPTH {
                        continue;
                    }
                    // Skip if base is too small
                    if entry.data.len() < MIN_DELTA_SIZE {
                        continue;
                    }

                    let estimate = DeltaEncoder::estimate_delta_size_with_index(
                        &entry.index,
                        &entry.data,
                        &data,
                    );

                    if estimate < best_delta_estimate {
                        best_delta_estimate = estimate;
                        best_base_idx = Some(i);
                    }
                }
            }

            // Decide: delta or raw?
            let (final_data, entry_type, base_hash, chain_depth) =
                if let Some(base_idx) = best_base_idx {
                    let base_entry = &window[base_idx];
                    let delta =
                        DeltaEncoder::encode_with_index(&base_entry.index, &base_entry.data, &data);
                    let delta_compressed = compress_pack_payload(&delta, compression)?;

                    if delta_compressed.len() < data.len() {
                        *delta_count += 1;
                        let bh = base_entry.hash;
                        let depth = base_entry.chain_depth + 1;
                        (delta_compressed, ObjectType::Delta, Some(bh), depth)
                    } else {
                        let compressed = compress_pack_payload(&data, compression)?;
                        (compressed, obj_type, None, 0)
                    }
                } else {
                    let compressed = compress_pack_payload(&data, compression)?;
                    (compressed, obj_type, None, 0)
                };

            *total_compressed += final_data.len() as u64;

            let record = PackObjectRecord {
                id: PackObjectId::Hash(hash),
                obj_type,
                data: data.clone(),
                delta_base: base_hash.map(PackObjectId::Hash),
                path_hint: None,
            };
            Self::write_entry(pack_data, &record, entry_type, &final_data)?;

            // Add to window (build index once, reuse for all future comparisons)
            let entry_index = DeltaEncoder::build_index(&data);
            if window.len() >= WINDOW_SIZE {
                window.pop_front();
            }
            window.push_back(WindowEntry {
                hash,
                data,
                index: entry_index,
                chain_depth,
            });
        }

        Ok(())
    }

    fn group_by_type(objects: Vec<PackObjectRecord>) -> GroupedPackMap {
        let mut groups: GroupedPackMap = HashMap::new();

        for record in objects {
            groups.entry(record.obj_type).or_default().push(record);
        }

        groups
    }

    /// Write a pack entry with varint-encoded sizes.
    ///
    /// Format per entry:
    /// ```text
    /// [tagged_id][type+uncompressed_size: varint][compressed_size: varint]
    /// [tagged_base_id (delta only)][compressed_data]
    /// ```
    ///
    /// The compressed data is raw zstd — no wrapper header — since the
    /// entry already records both sizes.
    fn write_entry(
        pack: &mut Vec<u8>,
        record: &PackObjectRecord,
        obj_type: ObjectType,
        compressed: &[u8],
    ) -> Result<()> {
        encode_tagged_entry(pack, record, obj_type, compressed)
    }
}