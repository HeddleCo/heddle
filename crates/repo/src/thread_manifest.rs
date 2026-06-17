// SPDX-License-Identifier: Apache-2.0
//! Per-thread sidecar that records the result of materializing a
//! thread's captured tree onto disk.
//!
//! The manifest is the **stat-cache** that makes `heddle capture`
//! against a materialized thread fast: instead of re-hashing every
//! file, capture stats each file and compares
//! `(inode, mtime_ns, ctime_ns, mode)` to the manifest's record.
//! Matches → unchanged, reuse the stored hash. Misses → re-hash and
//! write the new blob. Same pattern git's index uses.
//!
//! Lives at `<heddle_dir>/threads/<thread>/manifest.toml`, outside
//! the thread's worktree root so `rm -rf .` inside the worktree
//! doesn't destroy it. Rewritten atomically (temp + rename) on
//! every successful capture.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use objects::{
    fs_atomic::{enrich_fs_error, enrich_rename_error},
    object::{ChangeId, ContentHash},
};
use serde::{Deserialize, Serialize};

/// Current schema version. Bumped only when the on-disk layout
/// changes incompatibly; readers should refuse to interpret a
/// manifest with a future version they don't recognise.
///
/// v2 adds [`ManifestFile::size`] — the prior schema had no size
/// field, so a v1 manifest read as v2 would default `size` to 0 and
/// silently mismatch every entry.
///
/// v3 adds [`ThreadManifest::worktree_path`] — the absolute path the
/// manifest's stats describe. Without it, `Repository::snapshot`
/// can't tell whether it's running inside the materialized worktree
/// (use the stat-cache + refresh the manifest after) or some other
/// directory the same thread is checked out at (do neither — the
/// stats won't match anyway, and a refresh would corrupt the
/// manifest by writing the wrong directory's stats into it).
///
/// Bumping the version forces a clean rematerialize for any thread
/// carrying a pre-current manifest.
pub const SCHEMA_VERSION: u32 = 3;

/// On-disk per-thread materialization record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadManifest {
    /// Manifest format version. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The state this manifest is a materialization of.
    pub state_id: ChangeId,
    /// Tree hash captured at materialize time. Future-proofing for a
    /// `refresh` operation that wants to know which tree the on-disk
    /// state corresponds to without resolving the thread head again.
    pub tree_hash: ContentHash,
    /// UNIX-epoch seconds at materialize time. Diagnostic only;
    /// nothing in the capture path consults this.
    pub materialized_at: u64,
    /// Absolute path to the worktree directory the per-file stat
    /// records describe. Used by `Repository::snapshot` to decide
    /// whether the running capture is happening inside this
    /// materialized worktree (refresh + use cache) or somewhere
    /// else (skip both). Canonicalized at write time so symlink /
    /// `./` traversal differences don't cause a false miss.
    pub worktree_path: PathBuf,
    /// `true` when the *last* materialize of this thread was *withheld*: the
    /// state's visibility tier was not visible to the materializing audience,
    /// so only the operator-local courtesy stub was written and the tracked
    /// bytes withheld (`files` is therefore empty while `tree_hash` still names
    /// the real, unserved state's tree).
    ///
    /// **Diagnostic only.** This is a per-thread field on a single
    /// `manifest.toml`, so it reflects only whichever worktree was materialized
    /// last; a sibling worktree of the same thread clobbers it. The
    /// authoritative, per-worktree-root non-capturable signal that
    /// `capture_thread_from_disk` actually consults is the withheld *marker*
    /// (see [`mark_withheld_checkout`] / [`is_withheld_checkout`]), keyed on the
    /// worktree root so an under-tier checkout of one worktree never suppresses
    /// an authorized sibling worktree's capture. `#[serde(default)]` keeps
    /// pre-existing manifests readable as `false`. See heddle#316.
    #[serde(default)]
    pub withheld: bool,
    /// Per-file stat-cache. Key is the worktree-relative path with
    /// forward-slash separators (so a manifest moves between macOS
    /// and Linux without rewriting). Value is the snapshot of
    /// `(hash, size, inode, mtime_ns, ctime_ns, mode)` we recorded
    /// when the file landed in the worktree.
    #[serde(default)]
    pub files: BTreeMap<String, ManifestFile>,
}

impl ThreadManifest {
    /// Construct an empty manifest pointing at `state_id`. Caller is
    /// expected to populate `files` before persisting.
    ///
    /// `worktree_path` should be the path the manifest's stat records
    /// will describe. Production callers canonicalize the path
    /// (`std::fs::canonicalize`) so a later `snapshot` from inside
    /// the worktree matches regardless of symlink / `./` indirection.
    pub fn new(state_id: ChangeId, tree_hash: ContentHash, worktree_path: PathBuf) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state_id,
            tree_hash,
            materialized_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            worktree_path,
            withheld: false,
            files: BTreeMap::new(),
        }
    }
}

/// Per-file record inside a [`ThreadManifest`]. The stat fields are
/// stored together so a `stat`-cache hit is a single comparison and
/// the user can never accidentally bypass it by `touch`ing a file
/// with a frozen clock.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Content hash at materialize time. Reused as the file's blob
    /// hash on capture when the stat-cache matches.
    pub hash: ContentHash,
    /// File size in bytes. Cheapest possible "content changed"
    /// detector — independent of mtime granularity. CI runners
    /// frequently mount filesystems (ext4 with various options,
    /// overlayfs over tmpfs) where two back-to-back writes share an
    /// mtime, so size is the safety net that catches "wrote
    /// different-length bytes" even when the timestamps collapse.
    pub size: u64,
    /// File inode. On reflink filesystems (APFS, btrfs, XFS-reflink),
    /// the inode is the per-clonefile identity — a re-clonefile of
    /// the same canonical blob gets a *different* inode, so this
    /// catches "the file was clobbered and re-cloned" out of band.
    pub inode: u64,
    /// Modification time in nanoseconds since UNIX epoch.
    pub mtime_ns: i64,
    /// Status-change time in nanoseconds since UNIX epoch. Catches
    /// metadata-only changes that don't bump `mtime` (e.g. `chmod`).
    pub ctime_ns: i64,
    /// Unix mode bits (including type bits). `0o100644` for a regular
    /// file, `0o100755` for executable, etc.
    pub mode: u32,
}

impl ManifestFile {
    /// `true` iff `other` describes the same file content + metadata
    /// as `self`. Hot-path comparison for the capture stat-cache.
    #[inline]
    pub fn matches(&self, other: &ManifestFile) -> bool {
        self.size == other.size
            && self.inode == other.inode
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.mode == other.mode
    }
}

/// The single per-thread directory under `<heddle_dir>/threads/`, keyed
/// off the thread name through a **prefix-safe single-segment encoding**
/// ([`encode_thread_segment`]). A slashed id never becomes a directory
/// prefix of another: `feature/foo` maps to `threads/feature%2Ffoo`, NOT
/// `threads/feature/foo`, so it can neither nest under nor swallow a
/// `feature` (or `feature/f`) thread. Because the encoding is injective
/// and yields exactly one path component, two distinct ids always land in
/// disjoint sibling directories, and none can ever be an ancestor of
/// another (closing the prefix-nesting + recursive-drop class, heddle#572
/// r2).
///
/// This is the ONE derivation every thread-path consumer keys off — the
/// `manifest.toml` sidecar, the worktree checkout root
/// (`<dir>/<repo-name>`) for all three workspace modes, the harness
/// subagent/root-actor paths, and
/// the promote default — so the manifest and the checkout can never
/// diverge or collide for a given thread.
pub fn thread_dir(heddle_dir: &Path, thread: &str) -> PathBuf {
    heddle_dir
        .join("threads")
        .join(encode_thread_segment(thread))
}

/// Managed checkout leaf for a materialized/virtualized thread.
///
/// The per-thread directory itself holds Heddle sidecars such as
/// `manifest.toml`; the checkout must live one level below it. Use the
/// source repository's top-level directory name for that leaf so a managed
/// checkout reads as `.heddle/threads/<thread>/<repo-name>` instead of a
/// generic leaf. Repositories whose root has no final component use the
/// neutral `checkout` fallback.
pub fn managed_checkout_leaf(repo_root: &Path) -> OsString {
    repo_root
        .file_name()
        .filter(|name| !name.to_string_lossy().is_empty())
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("checkout"))
}

/// Default managed checkout path for `thread`.
pub fn managed_checkout_path(heddle_dir: &Path, thread: &str, repo_root: &Path) -> PathBuf {
    thread_dir(heddle_dir, thread).join(managed_checkout_leaf(repo_root))
}

/// Encode a thread id into a single, filesystem-safe, prefix-free path
/// segment. Percent-encodes every byte that is unsafe as a lone path
/// component — the separators `/` and `\`, the Windows-hostile `:`, the
/// `%` escape itself, and anything outside the safe slug set — so the
/// mapping is injective and reversible ([`decode_thread_segment`]) and
/// each id occupies one disjoint leaf under `threads/`. The common safe
/// slug characters (alphanumerics and `_ - . @ + =`) pass through for
/// readability, so `v1.2`, `team@scope`, and `wip+1=2` stay legible while
/// `feature/foo` becomes `feature%2Ffoo`.
///
/// The whole-segment `.` / `..` results are escaped specially: a thread
/// id of exactly `.` (which [`crate::validate_thread_id`] permits) or `..`
/// (which it rejects, but unchecked/deserialized ids could still carry)
/// would otherwise be a current-/parent-dir component that escapes or
/// aliases `threads/`.
pub fn encode_thread_segment(thread: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(thread.len());
    for &b in thread.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@' | b'+' | b'=') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    match out.as_str() {
        "." => "%2E".to_string(),
        ".." => "%2E%2E".to_string(),
        _ => out,
    }
}

/// Inverse of [`encode_thread_segment`]: recover the thread id from its
/// on-disk directory segment. Returns `None` for a malformed segment (a
/// truncated/`%`-escape with non-hex digits, or bytes that don't form
/// valid UTF-8) so the manifest walk skips foreign directories rather than
/// inventing a bogus thread name.
pub fn decode_thread_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Where this thread's manifest lives on disk, given the repo's
/// `heddle_dir` and a thread name:
/// `<heddle_dir>/threads/<encoded>/manifest.toml`, a sibling of the
/// managed checkout leaf. Derives from [`thread_dir`] so it shares the
/// one prefix-safe encoding.
pub fn manifest_path(heddle_dir: &Path, thread: &str) -> PathBuf {
    thread_dir(heddle_dir, thread).join("manifest.toml")
}

/// Summary of a single materialized thread, gathered by walking
/// `<heddle_dir>/threads/`. Stable enough for `heddle daemon status`
/// and `heddle thread list` to render; expand as the daemon grows
/// new observability needs.
#[derive(Clone, Debug)]
pub struct MaterializedThreadSummary {
    /// Thread name (the directory under `threads/`).
    pub thread: String,
    /// Recorded state at materialize time.
    pub state_id: ChangeId,
    /// Recorded uncompressed tree hash at materialize time.
    pub tree_hash: ContentHash,
    /// UNIX-epoch seconds at materialize time.
    pub materialized_at: u64,
    /// Number of files tracked by the manifest.
    pub file_count: usize,
}

/// Walk `<heddle_dir>/threads/` and return one summary per thread
/// whose manifest parses. Threads with malformed or schema-mismatch
/// manifests are silently skipped — callers that care can re-read
/// individual manifests via [`read_manifest`].
///
/// A single-level scan: every thread occupies exactly one directory
/// `threads/<encoded>/` (the prefix-safe [`thread_dir`] encoding), so the
/// thread name is the *decoded* directory segment and there is nothing to
/// recurse into. Directories whose segment doesn't decode (foreign / hand-
/// placed) are skipped. This also keeps the scan out of each thread's
/// managed checkout, which has no thread `manifest.toml` of its own.
pub fn list_thread_manifests(heddle_dir: &Path) -> io::Result<Vec<MaterializedThreadSummary>> {
    let threads_dir = heddle_dir.join("threads");
    let entries = match fs::read_dir(&threads_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(enrich_fs_error(&threads_dir, "listing", e)),
    };
    let mut summaries = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(segment) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(name) = decode_thread_segment(&segment) else {
            continue;
        };
        // Read the manifest directly from this dir rather than re-deriving
        // its path from `name` — robust to any segment whose decode→encode
        // isn't byte-identical (e.g. a hand-placed lowercase escape).
        if let Ok(Some(m)) = read_manifest_at(&entry.path().join("manifest.toml")) {
            summaries.push(MaterializedThreadSummary {
                thread: name,
                state_id: m.state_id,
                tree_hash: m.tree_hash,
                materialized_at: m.materialized_at,
                file_count: m.files.len(),
            });
        }
    }
    summaries.sort_by(|a, b| a.thread.cmp(&b.thread));
    Ok(summaries)
}

/// Read the on-disk manifest for `thread`. Returns `Ok(None)` when no
/// manifest exists yet (thread has never been materialized through
/// this code path). Returns an error on malformed TOML or a
/// schema-version mismatch — callers should treat that as "rebuild
/// the manifest from scratch", not as a corruption hazard.
pub fn read_manifest(heddle_dir: &Path, thread: &str) -> io::Result<Option<ThreadManifest>> {
    read_manifest_at(&manifest_path(heddle_dir, thread))
}

/// Read + validate a `manifest.toml` at an explicit `path`. Shared by the
/// name-keyed [`read_manifest`] and the directory-scanning
/// [`list_thread_manifests`] so both apply the same not-found / malformed /
/// schema-mismatch handling.
pub fn read_manifest_at(path: &Path) -> io::Result<Option<ThreadManifest>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(enrich_fs_error(path, "reading", e)),
    };
    let manifest: ThreadManifest = toml::from_str(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed thread manifest at {}: {e}", path.display()),
        )
    })?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "thread manifest at {} uses schema {} but this binary speaks {}",
                path.display(),
                manifest.schema_version,
                SCHEMA_VERSION
            ),
        ));
    }
    Ok(Some(manifest))
}

/// Find the thread manifest whose recorded `worktree_path` equals
/// `canonical_worktree_root`, regardless of which thread name it lives under.
///
/// Keyed by the **canonical worktree root**, not the thread name, because the
/// checkout reconcile step needs to know which tracked leaves a *prior*
/// materialization left at a root so a now-withheld re-materialize can remove
/// exactly those — and that prior materialization may have been a different
/// thread checked out at the same path. Returns `Ok(None)` when no manifest
/// records this root (a first-ever materialize, or one whose manifest was
/// since clobbered by a sibling worktree of the same thread). Malformed or
/// schema-mismatched manifests are skipped rather than erroring — the reconcile
/// caller treats "no recoverable record" as "nothing to remove".
pub fn manifest_for_worktree_root(
    heddle_dir: &Path,
    canonical_worktree_root: &Path,
) -> io::Result<Option<ThreadManifest>> {
    let threads_dir = heddle_dir.join("threads");
    let entries = match fs::read_dir(&threads_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(enrich_fs_error(&threads_dir, "listing", e)),
    };
    // Single-level scan: every thread's `manifest.toml` lives exactly one
    // level deep at `threads/<encoded>/manifest.toml` (flat [`thread_dir`]
    // layout). Match on the recorded `worktree_path`, so the thread name is
    // irrelevant; skip anything unparseable or schema-stale — the reconcile
    // caller is conservative by design.
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("manifest.toml");
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(manifest) = toml::from_str::<ThreadManifest>(&text)
            && manifest.schema_version == SCHEMA_VERSION
            && manifest.worktree_path == canonical_worktree_root
        {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// Delete the on-disk manifest directory for `thread`. Used by
/// `heddle thread drop` to keep the materialized-thread inventory
/// (`heddle status` / `heddle daemon status`) in sync with the live
/// thread set. Idempotent: a missing directory is reported as
/// "deleted = false" rather than an error.
///
/// Removes the whole `<heddle_dir>/threads/<encoded>/` directory —
/// not just `manifest.toml` — so future per-thread sidecars
/// (verification artefacts, capture journals, etc.) clean up with
/// the same call.
///
/// The flat [`thread_dir`] encoding gives every thread its own
/// single-segment leaf with no shared parent namespace, so dropping one
/// thread can never recursively delete a *different* slash-namespaced
/// thread's checkout, and there are no empty intermediate parents left to
/// reap (the heddle#572 r2 recursive-drop hazard).
pub fn remove_thread_manifest_dir(heddle_dir: &Path, thread: &str) -> io::Result<bool> {
    let dir = thread_dir(heddle_dir, thread);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(enrich_fs_error(&dir, "removing", e)),
    }
}

/// On-disk location of the *withheld-checkout* marker for the worktree
/// materialized at `canonical_worktree_root`.
///
/// Keyed by the **canonical worktree root**, NOT the thread name. A single
/// thread can be materialized into more than one worktree at once (e.g. an
/// authorized checkout at tier-A into one dir and an under-tier checkout into
/// another). The per-thread `manifest.toml` is a single file that the second
/// materialize clobbers, so a `withheld` flag stored there cannot distinguish
/// "this worktree was withheld" from "some sibling worktree of the same thread
/// was withheld". Keying the marker on the worktree root makes each
/// materialization's withheld status independent — an under-tier checkout into
/// worktree B never suppresses a capture of authorized worktree A of the same
/// thread (heddle#316).
///
/// The root is hashed (not embedded) so an arbitrarily long / non-ASCII path
/// maps to a fixed-length, filesystem-safe filename.
fn withheld_marker_path(heddle_dir: &Path, canonical_worktree_root: &Path) -> PathBuf {
    let key = ContentHash::compute_typed(
        "withheld-checkout",
        canonical_worktree_root.as_os_str().as_encoded_bytes(),
    )
    .to_hex();
    heddle_dir
        .join("withheld-checkouts")
        .join(format!("{key}.marker"))
}

/// Record that the checkout materialized at `canonical_worktree_root` is
/// *withheld*: the state's visibility tier was not visible to the materializing
/// audience, so only the operator-local courtesy stub was written and the
/// tracked bytes withheld. `capture_thread_from_disk` of this specific worktree
/// must be a no-op. Keyed per worktree root (see [`withheld_marker_path`]), so a
/// sibling worktree of the same thread is unaffected. The marker body is the
/// human-readable root for diagnostics; presence is the signal.
pub fn mark_withheld_checkout(heddle_dir: &Path, canonical_worktree_root: &Path) -> io::Result<()> {
    let path = withheld_marker_path(heddle_dir, canonical_worktree_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| enrich_fs_error(parent, "creating", e))?;
    }
    fs::write(&path, canonical_worktree_root.to_string_lossy().as_bytes())
        .map_err(|e| enrich_fs_error(&path, "writing", e))
}

/// `true` iff the worktree at `canonical_worktree_root` was recorded withheld
/// by [`mark_withheld_checkout`] and not since cleared.
pub fn is_withheld_checkout(heddle_dir: &Path, canonical_worktree_root: &Path) -> bool {
    withheld_marker_path(heddle_dir, canonical_worktree_root).exists()
}

/// Clear any withheld marker for `canonical_worktree_root`. Called when the
/// same root is (re)materialized with real, served content, so a stale marker
/// left by a prior under-tier materialize of that root can't suppress a
/// now-authorized capture. Idempotent: a missing marker is a no-op success.
pub fn clear_withheld_checkout(
    heddle_dir: &Path,
    canonical_worktree_root: &Path,
) -> io::Result<()> {
    let path = withheld_marker_path(heddle_dir, canonical_worktree_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(enrich_fs_error(&path, "removing", e)),
    }
}

/// Serializable body of the per-worktree-root *materialized-leaves* record.
/// A thin wrapper so `toml` round-trips the leaf-path set (TOML needs a
/// top-level table, not a bare array). Storing the paths in a TOML string array
/// — rather than a newline-joined blob — keeps the record correct even for a
/// leaf path containing exotic bytes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MaterializedLeaves {
    /// Worktree-relative, forward-slash-joined tracked leaf paths the *last*
    /// materialize of this root left on disk. Empty after a withheld materialize
    /// (only the untracked courtesy stub remains).
    #[serde(default)]
    leaves: Vec<String>,
}

/// On-disk location of the *materialized-leaves* record for the worktree root at
/// `canonical_worktree_root`.
///
/// Keyed by the **canonical worktree root** (hashed), NOT the thread name —
/// exactly like [`withheld_marker_path`], and for the same reason. The single
/// per-thread `manifest.toml` is clobbered when a sibling worktree of the same
/// thread is materialized, so it cannot reliably answer "what tracked leaves did
/// a prior materialize leave at THIS root?". A record keyed on the root is
/// untouchable by a sibling materialize (which writes a different root's record
/// under a different hash), so the checkout reconcile can always source the
/// exact prior-leaf set to remove — closing the withheld-reduction leak that
/// otherwise reopened whenever the per-thread manifest was clobbered
/// (heddle#316 CLASS 1).
fn materialized_leaves_path(heddle_dir: &Path, canonical_worktree_root: &Path) -> PathBuf {
    let key = ContentHash::compute_typed(
        "materialized-leaves",
        canonical_worktree_root.as_os_str().as_encoded_bytes(),
    )
    .to_hex();
    heddle_dir
        .join("materialized-roots")
        .join(format!("{key}.leaves"))
}

/// Persist `leaves` — the worktree-relative tracked leaf paths a materialize
/// just left at `canonical_worktree_root` — as the clobber-proof per-root
/// record. Two routes funnel through here, and ONLY these two, so the record
/// always reflects the actual tracked content at the root:
///   * the checkout chokepoint ([`Repository::checkout_state_gated`]) on every
///     materialize — with an empty set on a withheld materialize, where only the
///     untracked stub remains; and
///   * [`write_manifest`], which derives the projection from `manifest.files` so
///     every manifest writer (capture refresh, post-snapshot refresh, the CLI
///     start `record`) keeps `.leaves` in lockstep with the manifest by
///     construction.
///
/// A later reconcile of the same root therefore knows precisely which prior
/// tracked leaves to remove even when a sibling worktree of the same thread has
/// clobbered the per-thread `manifest.toml`. Atomic temp+rename so a torn write
/// can't surface a half-record (heddle#316 CLASS 1).
pub fn write_materialized_leaves(
    heddle_dir: &Path,
    canonical_worktree_root: &Path,
    leaves: &BTreeSet<String>,
) -> io::Result<()> {
    let path = materialized_leaves_path(heddle_dir, canonical_worktree_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| enrich_fs_error(parent, "creating", e))?;
    }
    let body = MaterializedLeaves {
        leaves: leaves.iter().cloned().collect(),
    };
    let text = toml::to_string_pretty(&body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialising materialized-leaves record: {e}"),
        )
    })?;
    let tmp = path.with_extension("leaves.tmp");
    fs::write(&tmp, text).map_err(|e| enrich_fs_error(&tmp, "writing", e))?;
    fs::rename(&tmp, &path).map_err(|e| enrich_rename_error(&tmp, &path, e))
}

/// Read the clobber-proof per-root materialized-leaves record for
/// `canonical_worktree_root`. `Ok(None)` only when **no record file exists**
/// (a first-ever materialize of this root, or a root last materialized by a
/// binary predating this record) — distinct from a present-but-empty record,
/// which decodes to `Some(empty set)` (a withheld materialize left only the
/// untracked stub). A malformed record errors so a corrupt sidecar surfaces
/// rather than being silently treated as "nothing materialized here".
pub fn read_materialized_leaves(
    heddle_dir: &Path,
    canonical_worktree_root: &Path,
) -> io::Result<Option<BTreeSet<String>>> {
    let path = materialized_leaves_path(heddle_dir, canonical_worktree_root);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(enrich_fs_error(&path, "reading", e)),
    };
    let body: MaterializedLeaves = toml::from_str(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "malformed materialized-leaves record at {}: {e}",
                path.display()
            ),
        )
    })?;
    Ok(Some(body.leaves.into_iter().collect()))
}

/// Remove the clobber-proof per-root materialized-leaves record for
/// `canonical_worktree_root`. Idempotent: a missing record is a no-op success.
/// Used by the atomic `start` rollback to drop a record a failed-then-rolled-back
/// materialize would otherwise orphan in the shared heddle dir — the record is
/// keyed by canonical root, so the checkout-directory rewind never reaches it
/// (heddle#316 r11 P2).
pub fn clear_materialized_leaves(
    heddle_dir: &Path,
    canonical_worktree_root: &Path,
) -> io::Result<()> {
    let path = materialized_leaves_path(heddle_dir, canonical_worktree_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(enrich_fs_error(&path, "removing", e)),
    }
}

/// Atomically write `manifest` to disk for `thread`. Writes to a
/// sibling temp file first then renames into place so a torn write
/// can't leave a half-baked manifest visible to the next reader.
pub fn write_manifest(
    heddle_dir: &Path,
    thread: &str,
    manifest: &ThreadManifest,
) -> io::Result<()> {
    let path = manifest_path(heddle_dir, thread);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| enrich_fs_error(parent, "creating", e))?;
    }
    let text = toml::to_string_pretty(manifest).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialising thread manifest: {e}"),
        )
    })?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| enrich_fs_error(&tmp, "writing", e))?;
    fs::rename(&tmp, &path).map_err(|e| enrich_rename_error(&tmp, &path, e))?;

    // Keep the clobber-proof per-root materialized-leaves record in lockstep with
    // the manifest, BY CONSTRUCTION. The manifest is the source of truth for the
    // tracked set materialized at `worktree_path`; `.leaves` is its root-keyed
    // projection that the withheld reduction
    // ([`Repository::reconcile_materialized_root`]) reads to know which prior
    // tracked leaves to remove. Deriving + writing it from the SAME call that
    // writes the manifest is the single sync chokepoint: every manifest writer —
    // post-capture refresh, post-snapshot refresh, the CLI start `record`, and the
    // `materialize` paths — updates `.leaves` here, so the two records cannot
    // drift. A withheld manifest carries empty `files`, so the projection is the
    // empty set, exactly matching the withheld reduction's own write. The record
    // is keyed by the canonical worktree root (== `manifest.worktree_path`), so a
    // sibling worktree of the same thread can never erase it (heddle#316: the
    // per-root `.leaves` staleness class — capture used to rewrite `manifest.toml`
    // but never `.leaves`, leaking a captured-then-withheld leaf).
    let leaves: BTreeSet<String> = manifest.files.keys().cloned().collect();
    write_materialized_leaves(heddle_dir, &manifest.worktree_path, &leaves)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::object::ContentHash;
    use tempfile::TempDir;

    use super::*;

    fn h(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn cid() -> ChangeId {
        ChangeId::generate()
    }

    #[test]
    fn round_trip_empty() {
        let dir = TempDir::new().unwrap();
        let manifest = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "main", &manifest).unwrap();
        let loaded = read_manifest(dir.path(), "main").unwrap().expect("present");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.tree_hash, h(1));
        assert!(loaded.files.is_empty());
    }

    #[test]
    fn round_trip_with_files() {
        let dir = TempDir::new().unwrap();
        let mut manifest = ThreadManifest::new(cid(), h(7), PathBuf::from("/tmp/test-worktree"));
        manifest.files.insert(
            "Cargo.toml".to_string(),
            ManifestFile {
                hash: h(2),
                size: 128,
                inode: 4242,
                mtime_ns: 1_700_000_000_000_000_000,
                ctime_ns: 1_700_000_000_000_000_000,
                mode: 0o100644,
            },
        );
        manifest.files.insert(
            "src/main.rs".to_string(),
            ManifestFile {
                hash: h(3),
                size: 512,
                inode: 4243,
                mtime_ns: 1_700_000_001_000_000_000,
                ctime_ns: 1_700_000_001_000_000_000,
                mode: 0o100644,
            },
        );
        write_manifest(dir.path(), "feature-x", &manifest).unwrap();
        let loaded = read_manifest(dir.path(), "feature-x")
            .unwrap()
            .expect("present");
        assert_eq!(loaded.files.len(), 2);
        assert_eq!(loaded.files["Cargo.toml"].hash, h(2));
        assert_eq!(loaded.files["src/main.rs"].inode, 4243);
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_manifest(dir.path(), "no-such").unwrap().is_none());
    }

    #[test]
    fn thread_dir_and_manifest_share_one_derivation_for_slashed_ids() {
        let heddle = Path::new("/repo/.heddle");
        let dir = thread_dir(heddle, "feature/foo");
        // The slash is percent-encoded into ONE path segment, so the id can
        // never nest under (or swallow) another thread's directory.
        assert_eq!(dir, Path::new("/repo/.heddle/threads/feature%2Ffoo"));
        // The manifest is a child of the shared per-thread dir, so a managed
        // checkout leaf is its sibling.
        assert_eq!(
            manifest_path(heddle, "feature/foo"),
            dir.join("manifest.toml")
        );
        assert!(manifest_path(heddle, "feature/foo").starts_with(heddle.join("threads")));
    }

    #[test]
    fn managed_checkout_path_uses_repo_directory_name() {
        let heddle = Path::new("/workspace/repo/.heddle");
        let repo_root = Path::new("/workspace/repo");
        assert_eq!(
            managed_checkout_path(heddle, "feature/foo", repo_root),
            Path::new("/workspace/repo/.heddle/threads/feature%2Ffoo/repo")
        );
    }

    /// The close-the-class proof for the prefix-nesting bug: a thread id that
    /// is a path-component prefix of another (`feature/f` vs `feature/foo`, and
    /// the bare `feature` vs `feature/foo`) must map to DISJOINT directories
    /// where neither is an ancestor of the other. Before the flat encoding,
    /// `feature` lived at `threads/feature/` and `feature/foo` nested inside it
    /// at `threads/feature/foo/`, so `thread drop feature` recursively deleted
    /// `feature/foo`'s checkout (heddle#572 r2).
    #[test]
    fn prefix_thread_ids_get_disjoint_non_nested_dirs() {
        let heddle = Path::new("/repo/.heddle");
        let pairs = [
            ("feature/f", "feature/foo"),
            ("feature", "feature/foo"),
            ("foo", "foo/bar"),
        ];
        for (a, b) in pairs {
            let da = thread_dir(heddle, a);
            let db = thread_dir(heddle, b);
            assert_ne!(da, db, "distinct ids {a:?}/{b:?} must map to distinct dirs");
            assert!(
                !da.starts_with(&db) && !db.starts_with(&da),
                "neither {a:?}→{} nor {b:?}→{} may be a path prefix of the other",
                da.display(),
                db.display(),
            );
        }
    }

    /// `encode`/`decode` round-trips every thread id `validate_thread_id`
    /// accepts (plus the dangerous `.`/`..` whole-segment cases), and never
    /// emits a `/` or a path-traversal component.
    #[test]
    fn encode_decode_round_trips_and_stays_single_segment() {
        for id in [
            "feature/foo",
            "feature/f",
            "feature",
            "v1.2",
            "team@scope",
            "wip+1=2",
            "a_b-c.d",
            "main",
            ".",
            "..",
            "weird name with spaces",
            "team:scope",
            "100%done",
        ] {
            let seg = encode_thread_segment(id);
            assert!(!seg.contains('/'), "{id:?} → {seg:?} must be one segment");
            assert_ne!(
                seg, ".",
                "{id:?} must not encode to a current-dir component"
            );
            assert_ne!(
                seg, "..",
                "{id:?} must not encode to a parent-dir component"
            );
            assert_eq!(
                decode_thread_segment(&seg).as_deref(),
                Some(id),
                "{id:?} must round-trip through encode/decode"
            );
        }
    }

    #[test]
    fn list_returns_sorted_summaries_for_present_manifests() {
        let dir = TempDir::new().unwrap();
        let m1 = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        let mut m2 = ThreadManifest::new(cid(), h(2), PathBuf::from("/tmp/test-worktree"));
        m2.files.insert(
            "a.txt".to_string(),
            ManifestFile {
                hash: h(9),
                size: 7,
                inode: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                mode: 0o100644,
            },
        );
        write_manifest(dir.path(), "zebra", &m1).unwrap();
        write_manifest(dir.path(), "alpha", &m2).unwrap();
        let summaries = list_thread_manifests(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        // Sorted by name → alpha first.
        assert_eq!(summaries[0].thread, "alpha");
        assert_eq!(summaries[0].tree_hash, h(2));
        assert_eq!(summaries[0].file_count, 1);
        assert_eq!(summaries[1].thread, "zebra");
        assert_eq!(summaries[1].tree_hash, h(1));
        assert_eq!(summaries[1].file_count, 0);
    }

    #[test]
    fn list_empty_when_no_threads_dir() {
        let dir = TempDir::new().unwrap();
        let summaries = list_thread_manifests(dir.path()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn matches_compares_identity_fields() {
        let a = ManifestFile {
            hash: h(1),
            size: 64,
            inode: 100,
            mtime_ns: 200,
            ctime_ns: 300,
            mode: 0o100644,
        };
        let b = a;
        assert!(a.matches(&b));
        let mut c = a;
        c.mtime_ns = 999;
        assert!(!a.matches(&c));
        let mut d = a;
        d.mode = 0o100755;
        assert!(!a.matches(&d));
        let mut e = a;
        e.size = 65;
        assert!(!a.matches(&e), "size change must invalidate the cache");
    }

    /// Close-the-class proof for the recursive-drop hazard: dropping a
    /// slash-namespaced thread must remove ONLY its own flat directory and
    /// never touch a sibling that shares a conceptual namespace. Under the
    /// old slash-nesting layout `feature/keeper` and `feature/dropme` shared a
    /// `threads/feature/` parent, so dropping one could recursively delete the
    /// other; the flat encoding gives each a disjoint leaf (heddle#572 r2).
    #[test]
    fn remove_thread_manifest_dir_isolates_slash_namespaced_siblings() {
        let dir = TempDir::new().unwrap();
        let m = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "feature/keeper", &m).unwrap();
        write_manifest(dir.path(), "feature/dropme", &m).unwrap();

        assert!(remove_thread_manifest_dir(dir.path(), "feature/dropme").unwrap());
        assert!(
            !thread_dir(dir.path(), "feature/dropme").exists(),
            "the dropped thread's directory must be gone"
        );
        assert!(
            thread_dir(dir.path(), "feature/keeper").exists(),
            "a slash-namespaced sibling must be completely untouched by the drop"
        );
        assert!(
            read_manifest(dir.path(), "feature/keeper")
                .unwrap()
                .is_some(),
            "the sibling's manifest must survive"
        );

        // The `threads/` root is heddle-managed and must never be removed.
        assert!(remove_thread_manifest_dir(dir.path(), "feature/keeper").unwrap());
        assert!(
            dir.path().join("threads").is_dir(),
            "the `threads/` root is heddle-managed and must never be reaped"
        );
    }

    /// Dropping a non-slash thread name is the trivial case: the
    /// thread dir vanishes and there are no parents to reap (the
    /// only ancestor inside `threads/` is `threads/` itself).
    #[test]
    fn remove_thread_manifest_dir_handles_flat_names() {
        let dir = TempDir::new().unwrap();
        let m = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "main", &m).unwrap();
        assert!(remove_thread_manifest_dir(dir.path(), "main").unwrap());
        assert!(!dir.path().join("threads").join("main").exists());
        assert!(dir.path().join("threads").is_dir());
    }

    /// Missing thread reports `false` rather than erroring — drops
    /// are idempotent so a double-drop is a no-op success.
    #[test]
    fn remove_thread_manifest_dir_is_idempotent_on_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("threads")).unwrap();
        assert!(!remove_thread_manifest_dir(dir.path(), "never-existed").unwrap());
    }
}
