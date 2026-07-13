// SPDX-License-Identifier: Apache-2.0
//! Rebuildable sidecar summary for list-heavy local ref reads.

use std::collections::{BTreeMap, BTreeSet};

use objects::{
    error::{HeddleError, Result},
    object::{MarkerName, StateId, ThreadName},
};
use serde::Serialize;

use super::{RefManager, packed_refs::PackedRefs, parse_state_id_text, refs_storage::RefsLock};

const REF_SUMMARY_VERSION: &str = "heddle-ref-summary-v2";

#[derive(Debug, Clone, Serialize)]
pub struct RefSummaryIndexInspection {
    pub present: bool,
    pub valid: bool,
    pub bytes: u64,
    pub threads: usize,
    pub markers: usize,
    pub remotes: usize,
    pub remote_threads: usize,
    pub packed_threads: usize,
    pub packed_markers: usize,
    pub error: Option<String>,
}

impl RefSummaryIndexInspection {
    pub fn absent() -> Self {
        Self {
            present: false,
            valid: false,
            bytes: 0,
            threads: 0,
            markers: 0,
            remotes: 0,
            remote_threads: 0,
            packed_threads: 0,
            packed_markers: 0,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefSummarySource {
    Loose,
    Packed,
    LooseAndPacked,
}

impl RefSummarySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loose => "loose",
            Self::Packed => "packed",
            Self::LooseAndPacked => "loose+packed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "loose" => Ok(Self::Loose),
            "packed" => Ok(Self::Packed),
            "loose+packed" => Ok(Self::LooseAndPacked),
            other => Err(HeddleError::InvalidObject(format!(
                "invalid ref summary source {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct RefSummaryEntry {
    name: String,
    state_id: StateId,
    source: RefSummarySource,
}

/// A single loose-ref delta carried by a publish plan, used to update the
/// summary index incrementally instead of rescanning the whole refs dir.
///
/// The plan already knows exactly which thread/marker changed and its new
/// state-id, so a publish that touches `k` refs costs `O(k)` index edits +
/// one packed-refs load — not an `O(refs)` full-dir rescan. HEAD is not part of
/// the summary index, so HEAD plans carry no target; remote threads are never
/// produced by `publish_ref_plans`, so they stay untouched in the index.
#[derive(Debug, Clone)]
pub(super) enum SummaryDelta {
    /// Loose thread set to a new state-id (the loose file now exists on disk).
    SetThread { name: String, state_id: StateId },
    /// Loose marker set to a new state-id.
    SetMarker { name: String, state_id: StateId },
    /// Thread removed from both loose and packed storage (delete plans always
    /// carry a packed removal), so the entry leaves the index entirely.
    DeleteThread { name: String },
    /// Marker removed from both loose and packed storage.
    DeleteMarker { name: String },
}

#[derive(Debug, Clone)]
struct RemoteThreadSummaryEntry {
    name: String,
    state_id: StateId,
}

#[derive(Debug, Clone)]
struct RemoteSummaryEntry {
    name: String,
    threads: Vec<RemoteThreadSummaryEntry>,
}

#[derive(Debug, Clone)]
pub(super) struct RefSummaryIndex {
    threads: Vec<RefSummaryEntry>,
    markers: Vec<RefSummaryEntry>,
    remotes: Vec<RemoteSummaryEntry>,
}

impl RefSummaryIndex {
    fn parse(contents: &str) -> Result<Self> {
        let mut lines = contents.lines();
        let header = lines
            .next()
            .ok_or_else(|| HeddleError::InvalidObject("empty ref summary index".to_string()))?;
        if header != REF_SUMMARY_VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported ref summary version: {header}"
            )));
        }

        let mut threads = Vec::new();
        let mut markers = Vec::new();
        let mut remotes = BTreeMap::<String, Vec<RemoteThreadSummaryEntry>>::new();
        let mut remote_names = BTreeSet::<String>::new();

        for line in lines {
            if line.is_empty() {
                continue;
            }

            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["thread", name, state_id, source] => threads.push(RefSummaryEntry {
                    name: (*name).to_string(),
                    state_id: parse_summary_state_id(state_id)?,
                    source: RefSummarySource::parse(source)?,
                }),
                ["marker", name, state_id, source] => markers.push(RefSummaryEntry {
                    name: (*name).to_string(),
                    state_id: parse_summary_state_id(state_id)?,
                    source: RefSummarySource::parse(source)?,
                }),
                ["remote", remote] => {
                    remote_names.insert((*remote).to_string());
                    remotes.entry((*remote).to_string()).or_default();
                }
                ["remote_thread", remote, name, state_id] => {
                    remote_names.insert((*remote).to_string());
                    remotes.entry((*remote).to_string()).or_default().push(
                        RemoteThreadSummaryEntry {
                            name: (*name).to_string(),
                            state_id: parse_summary_state_id(state_id)?,
                        },
                    );
                }
                _ => {
                    return Err(HeddleError::InvalidObject(format!(
                        "invalid ref summary line: {line}"
                    )));
                }
            }
        }

        let remotes = remote_names
            .into_iter()
            .map(|name| RemoteSummaryEntry {
                threads: remotes.remove(&name).unwrap_or_default(),
                name,
            })
            .collect();

        Ok(Self {
            threads,
            markers,
            remotes,
        })
    }

    fn to_text(&self) -> String {
        let mut out = String::from(REF_SUMMARY_VERSION);
        out.push('\n');

        for entry in &self.threads {
            out.push_str("thread\t");
            out.push_str(&entry.name);
            out.push('\t');
            out.push_str(&entry.state_id.to_string_full());
            out.push('\t');
            out.push_str(entry.source.as_str());
            out.push('\n');
        }

        for entry in &self.markers {
            out.push_str("marker\t");
            out.push_str(&entry.name);
            out.push('\t');
            out.push_str(&entry.state_id.to_string_full());
            out.push('\t');
            out.push_str(entry.source.as_str());
            out.push('\n');
        }

        for remote in &self.remotes {
            out.push_str("remote\t");
            out.push_str(&remote.name);
            out.push('\n');
            for thread in &remote.threads {
                out.push_str("remote_thread\t");
                out.push_str(&remote.name);
                out.push('\t');
                out.push_str(&thread.name);
                out.push('\t');
                out.push_str(&thread.state_id.to_string_full());
                out.push('\n');
            }
        }

        out
    }

    fn inspection(&self, bytes: u64) -> RefSummaryIndexInspection {
        RefSummaryIndexInspection {
            present: true,
            valid: true,
            bytes,
            threads: self.threads.len(),
            markers: self.markers.len(),
            remotes: self.remotes.len(),
            remote_threads: self.remotes.iter().map(|remote| remote.threads.len()).sum(),
            packed_threads: self
                .threads
                .iter()
                .filter(|entry| entry.source != RefSummarySource::Loose)
                .count(),
            packed_markers: self
                .markers
                .iter()
                .filter(|entry| entry.source != RefSummarySource::Loose)
                .count(),
            error: None,
        }
    }

    /// Apply one loose-ref delta in place, recomputing the entry's `source`
    /// from the supplied packed-refs view so a loose set over a packed ref is
    /// recorded as `LooseAndPacked` (matching the from-storage rebuild).
    ///
    /// Entries stay sorted by name: set inserts at the sorted position (or
    /// updates in place), delete removes. Remotes are never touched here.
    fn apply_delta(&mut self, delta: &SummaryDelta, packed: &PackedRefs) {
        match delta {
            SummaryDelta::SetThread { name, state_id } => {
                let source = if packed.get_thread(name).is_some() {
                    RefSummarySource::LooseAndPacked
                } else {
                    RefSummarySource::Loose
                };
                upsert_entry(&mut self.threads, name, *state_id, source);
            }
            SummaryDelta::SetMarker { name, state_id } => {
                let source = if packed.get_marker(name).is_some() {
                    RefSummarySource::LooseAndPacked
                } else {
                    RefSummarySource::Loose
                };
                upsert_entry(&mut self.markers, name, *state_id, source);
            }
            SummaryDelta::DeleteThread { name } => {
                self.threads.retain(|entry| entry.name != *name);
            }
            SummaryDelta::DeleteMarker { name } => {
                self.markers.retain(|entry| entry.name != *name);
            }
        }
    }

    pub(super) fn thread_names(&self) -> Vec<ThreadName> {
        self.threads
            .iter()
            .map(|entry| ThreadName::new(&entry.name))
            .collect()
    }

    pub(super) fn marker_names(&self) -> Vec<MarkerName> {
        self.markers
            .iter()
            .map(|entry| MarkerName::new(&entry.name))
            .collect()
    }

    pub(super) fn remote_names(&self) -> Vec<String> {
        self.remotes
            .iter()
            .map(|remote| remote.name.clone())
            .collect()
    }

    pub(super) fn remote_thread_names(&self, remote: &str) -> Vec<ThreadName> {
        self.remotes
            .iter()
            .find(|entry| entry.name == remote)
            .map(|entry| {
                entry
                    .threads
                    .iter()
                    .map(|thread| ThreadName::new(&thread.name))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl RefManager {
    pub fn inspect_ref_summary_index(&self) -> Result<RefSummaryIndexInspection> {
        let path = self.ref_summary_index_path();
        if !path.exists() {
            return Ok(RefSummaryIndexInspection::absent());
        }

        let bytes = file_len_or_zero(&path);
        match self.read_string(&path) {
            Ok(contents) => match RefSummaryIndex::parse(&contents) {
                Ok(summary) => Ok(summary.inspection(bytes)),
                Err(error) => Ok(RefSummaryIndexInspection {
                    present: true,
                    valid: false,
                    bytes,
                    threads: 0,
                    markers: 0,
                    remotes: 0,
                    remote_threads: 0,
                    packed_threads: 0,
                    packed_markers: 0,
                    error: Some(error.to_string()),
                }),
            },
            Err(error) => Ok(RefSummaryIndexInspection {
                present: true,
                valid: false,
                bytes,
                threads: 0,
                markers: 0,
                remotes: 0,
                remote_threads: 0,
                packed_threads: 0,
                packed_markers: 0,
                error: Some(error.to_string()),
            }),
        }
    }

    pub fn rebuild_ref_summary_index(&self) -> Result<RefSummaryIndexInspection> {
        let lock = self.lock_refs()?;
        self.rebuild_ref_summary_index_with_lock(&lock)
    }

    pub(super) fn rebuild_ref_summary_index_with_lock(
        &self,
        _lock: &RefsLock,
    ) -> Result<RefSummaryIndexInspection> {
        let summary = self.build_ref_summary_index_from_storage()?;
        let path = self.ref_summary_index_path();
        self.write_string(&path, &summary.to_text())?;
        Ok(summary.inspection(file_len_or_zero(&path)))
    }

    pub(super) fn invalidate_ref_summary_index(&self) {
        let _ = std::fs::remove_file(self.ref_summary_index_path());
    }

    /// Incrementally fold a publish's loose-ref deltas into the on-disk summary
    /// index instead of rescanning the whole refs dir (the `O(refs²)` cost that
    /// made `heddle adopt` quadratic — the index was rebuilt from a full
    /// `read_dir` + per-ref `read_state_id_at` on *every* publish over a
    /// directory growing to `N` refs).
    ///
    /// When a current, valid index exists we apply each delta (`O(deltas)` edits
    /// plus one packed-refs load) and rewrite. When the index is absent or corrupt
    /// (or `deltas` is empty, a HEAD-only publish) we fall back to a full
    /// from-storage rebuild so the sidecar is never left half-built. The result
    /// is byte-identical to a full rebuild after the same sequence of writes.
    pub(super) fn update_ref_summary_index_with_deltas(
        &self,
        _lock: &RefsLock,
        deltas: &[SummaryDelta],
    ) -> Result<()> {
        let mut summary = match self.read_ref_summary_index() {
            Ok(Some(summary)) => summary,
            // Absent or unreadable/corrupt: fall back to a clean full rebuild.
            Ok(None) | Err(_) => {
                return self.rebuild_ref_summary_index_with_lock(_lock).map(|_| ());
            }
        };

        if deltas.is_empty() {
            return Ok(());
        }

        let packed = self.load_packed_refs_cached()?;
        for delta in deltas {
            summary.apply_delta(delta, &packed);
        }

        let path = self.ref_summary_index_path();
        self.write_string(&path, &summary.to_text())
    }

    pub(super) fn list_threads_from_storage(&self) -> Result<Vec<ThreadName>> {
        let loose = self.scan_loose_threads()?;
        let packed = self.load_packed_refs_cached()?;
        let mut all: Vec<ThreadName> = loose.keys().map(|k| ThreadName::new(k.as_str())).collect();
        for name in packed.list_threads() {
            if !loose.contains_key(&name) {
                all.push(ThreadName::new(name));
            }
        }
        all.sort();
        Ok(all)
    }

    pub(super) fn list_markers_from_storage(&self) -> Result<Vec<MarkerName>> {
        let loose = self.scan_loose_markers()?;
        let packed = self.load_packed_refs_cached()?;
        let mut all: Vec<MarkerName> = loose.keys().map(|k| MarkerName::new(k.as_str())).collect();
        for name in packed.list_markers() {
            if !loose.contains_key(&name) {
                all.push(MarkerName::new(name));
            }
        }
        all.sort();
        Ok(all)
    }

    pub(super) fn list_remotes_from_storage(&self) -> Result<Vec<String>> {
        let remotes_dir = self.remotes_dir();
        if !remotes_dir.exists() {
            return Ok(Vec::new());
        }
        let mut remotes = Vec::new();
        for entry in std::fs::read_dir(remotes_dir)? {
            let entry = entry?;
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                remotes.push(name.to_string());
            }
        }
        remotes.sort();
        Ok(remotes)
    }

    pub(super) fn list_remote_threads_from_storage(&self, remote: &str) -> Result<Vec<ThreadName>> {
        self.list_refs_recursive(&self.remotes_dir().join(remote), "")
    }

    pub(super) fn try_read_ref_summary_index(&self) -> Option<RefSummaryIndex> {
        self.read_ref_summary_index().ok().flatten()
    }

    fn read_ref_summary_index(&self) -> Result<Option<RefSummaryIndex>> {
        let path = self.ref_summary_index_path();
        if !path.exists() {
            return Ok(None);
        }
        let contents = self.read_string(&path)?;
        Ok(Some(RefSummaryIndex::parse(&contents)?))
    }

    fn build_ref_summary_index_from_storage(&self) -> Result<RefSummaryIndex> {
        let loose_threads = self.scan_loose_threads()?;
        let loose_markers = self.scan_loose_markers()?;
        let packed = self.load_packed_refs_cached()?;

        let mut threads: Vec<RefSummaryEntry> = loose_threads
            .iter()
            .map(|(name, state_id)| RefSummaryEntry {
                name: name.clone(),
                state_id: *state_id,
                source: if packed.get_thread(name).is_some() {
                    RefSummarySource::LooseAndPacked
                } else {
                    RefSummarySource::Loose
                },
            })
            .collect();
        for name in packed.list_threads() {
            if let Some(state_id) = packed.get_thread(&name)
                && !loose_threads.contains_key(&name)
            {
                threads.push(RefSummaryEntry {
                    name,
                    state_id,
                    source: RefSummarySource::Packed,
                });
            }
        }
        threads.sort_by(|left, right| left.name.cmp(&right.name));

        let mut markers: Vec<RefSummaryEntry> = loose_markers
            .iter()
            .map(|(name, state_id)| RefSummaryEntry {
                name: name.clone(),
                state_id: *state_id,
                source: if packed.get_marker(name).is_some() {
                    RefSummarySource::LooseAndPacked
                } else {
                    RefSummarySource::Loose
                },
            })
            .collect();
        for name in packed.list_markers() {
            if let Some(state_id) = packed.get_marker(&name)
                && !loose_markers.contains_key(&name)
            {
                markers.push(RefSummaryEntry {
                    name,
                    state_id,
                    source: RefSummarySource::Packed,
                });
            }
        }
        markers.sort_by(|left, right| left.name.cmp(&right.name));

        let remotes = self
            .list_remotes_from_storage()?
            .into_iter()
            .map(|name| {
                let threads = self
                    .scan_remote_threads(&name)?
                    .into_iter()
                    .map(|(thread, state_id)| RemoteThreadSummaryEntry {
                        name: thread,
                        state_id,
                    })
                    .collect();
                Ok(RemoteSummaryEntry { name, threads })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(RefSummaryIndex {
            threads,
            markers,
            remotes,
        })
    }

    fn scan_loose_threads(&self) -> Result<BTreeMap<String, StateId>> {
        let mut loose = BTreeMap::new();
        for name in self.list_refs_recursive(&self.threads_dir(), "")? {
            let name_str = name.to_string();
            let Some(decoded) = self
                .decode_flat_thread_entry(&name_str)
                .or_else(|| (!name_str.starts_with("__heddle_flat/")).then_some(name_str))
            else {
                continue;
            };
            let tname = ThreadName::new(&decoded);
            if let Some(state_id) =
                self.read_state_id_at(&self.thread_path(&tname)?, "thread", &decoded)?
            {
                loose.insert(decoded, state_id);
            }
        }
        Ok(loose)
    }

    fn scan_loose_markers(&self) -> Result<BTreeMap<String, StateId>> {
        let mut markers = BTreeMap::new();
        for name in self.list_refs_recursive(&self.markers_dir(), "")? {
            let name_str = name.to_string();
            if let Some(state_id) =
                self.read_state_id_at(&self.marker_path(&name_str)?, "marker", &name_str)?
            {
                markers.insert(name_str, state_id);
            }
        }
        Ok(markers)
    }

    fn scan_remote_threads(&self, remote: &str) -> Result<BTreeMap<String, StateId>> {
        let mut threads = BTreeMap::new();
        for name in self.list_remote_threads_from_storage(remote)? {
            let name_str = name.to_string();
            if let Some(state_id) = self.read_state_id_at(
                &self.remote_thread_path(remote, &name_str)?,
                "remote thread",
                &format!("{remote}/{name_str}"),
            )? {
                threads.insert(name_str, state_id);
            }
        }
        Ok(threads)
    }
}

/// Insert `name` into the sorted `entries`, or update it in place if present.
/// Keeps `entries` sorted by name (the from-storage rebuild sorts the same way).
fn upsert_entry(
    entries: &mut Vec<RefSummaryEntry>,
    name: &str,
    state_id: StateId,
    source: RefSummarySource,
) {
    match entries.binary_search_by(|entry| entry.name.as_str().cmp(name)) {
        Ok(idx) => {
            entries[idx].state_id = state_id;
            entries[idx].source = source;
        }
        Err(idx) => entries.insert(
            idx,
            RefSummaryEntry {
                name: name.to_string(),
                state_id,
                source,
            },
        ),
    }
}

fn file_len_or_zero(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn parse_summary_state_id(contents: &str) -> Result<StateId> {
    parse_state_id_text(contents).map_err(|error| HeddleError::InvalidObject(error.to_string()))
}
