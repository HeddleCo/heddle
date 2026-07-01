// SPDX-License-Identifier: Apache-2.0
//! Walk a git repository's refs and the commits reachable from them.
//!
//! This module is the importer's *read* half: it knows git and doesn't know
//! Heddle. Output is a sequence of plain [`CommitEntry`] records (plus
//! [`RefHead`] snapshots) that the state-writer half consumes.
//!
//! # Scope
//!
//! What this walker captures:
//!
//! - Every commit reachable from any local branch or tag (`refs/heads/*`,
//!   `refs/tags/*`).
//! - Every commit reachable from remote-tracking refs (`refs/remotes/*`)
//!   — captured under the `RemoteBranch` namespace so the emitter can
//!   route them to threads named `origin/<branch>` etc., distinct from
//!   any local branch with the same short name.
//! - Full commit metadata: parents, tree, both signatures, message.
//! - Tree and blob readers for use by the [translator](crate).
//!
//! Packed-refs inspection is implicit — sley handles that transparently.
//!
//! Reflog *is* supported via [`GitSource::collect_reflog`] and
//! [`GitSource::reflog_commit_shas`]: the former yields every entry across
//! `HEAD` and every local ref (in iteration order — oldest → newest per
//! ref), the latter deduplicates the SHAs those entries reference and
//! filters out objects that have since been pruned from the object db.
//! The oplog emitter consumes the raw entries; the state writer folds the
//! extra SHAs back into [`GitSource::commits_topo`]'s seed set so the
//! translation covers force-pushed and dropped commits too.
//!
//! # Ordering
//!
//! [`CommitStream::commits_topo`] yields commits in a child-before-parent
//! order *reversed* — i.e. ancestors first. That's what the state writer
//! wants: when it writes a state, every parent it needs is already mapped.
//!
//! Ties within a depth level are broken by committer time, then by SHA, so
//! repeated runs of an unchanged repo produce the same order.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
};

use chrono::{DateTime, TimeZone, Utc};
use sley::{
    GitObjectType, ObjectFormat, ObjectId as SleyObjectId, RefStore as SleyRefStore,
    ReferenceTarget as SleyRefTarget, Repository as SleyRepository, Signature as SleySignature,
};

use crate::IngestError;

/// A reference pointing at a commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefHead {
    /// Short name — e.g. `main`, `v0.1.1`.
    pub short_name: String,
    /// Full name — e.g. `refs/heads/main`, `refs/tags/v0.1.1`.
    pub full_name: String,
    /// Whether this is a branch or a tag.
    pub namespace: RefNamespace,
    /// Commit SHA the ref resolves to (40-char lowercase hex).
    pub target_sha: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RefNamespace {
    /// Local branches under `refs/heads/*`.
    Branch,
    /// Tags under `refs/tags/*`. Annotated tags are pre-peeled to a
    /// commit SHA before being stored in [`RefHead::target_sha`].
    Tag,
    /// Remote-tracking refs under `refs/remotes/*`. Stored under their
    /// `<remote>/<branch>` short name so they don't collide with a
    /// local branch of the same leaf name (e.g. `main` vs `origin/main`).
    RemoteBranch,
}

/// One git commit in the form the state writer consumes.
#[derive(Clone, Debug)]
pub struct CommitEntry {
    pub sha: String,
    pub tree_sha: String,
    pub parents: Vec<String>,
    pub author: GitSignature,
    pub committer: GitSignature,
    /// Full commit message (subject + body + trailers), verbatim git bytes.
    ///
    /// Raw bytes, NOT a `String`: a commit with a non-UTF8 `encoding`
    /// (latin-1, shift-jis, …) carries message bytes that aren't valid UTF-8,
    /// and they must reach [`State::raw_message`](objects::object::State) byte-
    /// identically for #566 reconstruction. Consumers that need a string view
    /// (attribution / intent parsing) derive a lossy one at the edge.
    pub message: Vec<u8>,
    pub authored_at: DateTime<Utc>,
    pub committed_at: DateTime<Utc>,
    /// Every commit header beyond tree/parents/author/committer, in original
    /// order, as raw bytes so non-UTF8 values survive. ORDER IS LOAD-BEARING
    /// for #566 byte-exactness. `gpgsig` rides inline here at its captured
    /// position (not split out) so a non-canonical header order reconstructs
    /// byte-identically. #564 step 1.
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Optional Heddle metadata note from `refs/notes/heddle`, if present.
    /// The state writer parses this to preserve exported Heddle identity and
    /// metadata during a fresh ingest of a Git repo.
    pub heddle_note: Option<Vec<u8>>,
}

/// Author/committer identity + timestamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSignature {
    pub name: String,
    pub email: String,
    pub time: DateTime<Utc>,
    /// Timezone offset in seconds east of UTC (git's `+HHMM`). Heddle used
    /// to discard this; #564 step 1 preserves it for byte-reconstruction.
    pub tz_offset: i32,
}

/// One tree entry (direct child of a git tree).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeChild {
    pub name: String,
    pub raw_name: Vec<u8>,
    pub sha: String,
    pub kind: TreeChildKind,
}

/// One reflog event — a ref moving from `previous_sha` to `new_sha`. Either
/// can be `None` when the entry records a ref being created (`previous`
/// null-sha) or deleted (`new` null-sha).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogEntry {
    /// `HEAD` or a full ref name like `refs/heads/main`.
    pub ref_name: String,
    pub previous_sha: Option<String>,
    pub new_sha: Option<String>,
    pub signature: GitSignature,
    /// Raw message as stored in `.git/logs/...` — e.g.
    /// `"commit: foo"`, `"reset: moving to HEAD~1"`, `"pull: Fast-forward"`.
    pub message: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TreeChildKind {
    Blob {
        executable: bool,
    },
    Tree,
    Symlink,
    /// Gitlinks (submodules) — exposed so the translator can decide a policy
    /// rather than silently skipping them.
    Gitlink,
}

/// Counters returned alongside [`GitSource::collect_refs_detailed`].
/// Exposes the per-namespace breakdown so import summaries can show
/// users *what was visible*, not just *what was kept* — important when
/// the walker silently filters refs (e.g. `origin/HEAD` symbolic refs).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefDiscoveryStats {
    pub local_branches: usize,
    pub tags: usize,
    pub remote_branches: usize,
    /// Refs the walker decided not to surface as a head — currently just
    /// remote `*/HEAD` symbolic refs that would dup-thread their target.
    pub symbolic_skipped: usize,
    /// Refs whose `peel_to_id` failed (e.g. dangling). Stays at zero on
    /// healthy repos; rises when an annotated tag points at a deleted
    /// object or similar corruption.
    pub peel_failed: usize,
    /// Refs whose peel succeeded but landed on a non-commit object (the
    /// most common shape: an annotated tag whose target is a blob, like
    /// `git/git`'s `refs/tags/junio-gpg-pub` pointing at the
    /// maintainer's GPG public key blob, or a tree, like `git-lfs`'s
    /// `refs/tags/core-gpg-keys`). The walker can't surface these as
    /// `RefHead`s because the importer needs commit-shaped tips, but
    /// crashing on them is wrong — they're a real-world pattern in
    /// mature OSS repos. Counted here for the import summary.
    pub non_commit_skipped: usize,
}

/// Returns `true` if `oid` resolves to a commit object in `repo`. Used
/// to guard the ref-discovery loops against non-commit-pointing refs
/// (annotated tags whose peeled target is a blob/tree, dangling refs).
/// Returns `false` on any read error — the caller treats that the same
/// as "not a commit" and skips the ref.
fn is_commit(repo: &SleyRepository, oid: SleyObjectId) -> bool {
    matches!(
        repo.read_object(&oid).map(|object| object.object_type),
        Ok(GitObjectType::Commit)
    )
}

enum RefResolution {
    Commit(SleyObjectId),
    PeelFailed,
    NonCommit,
}

/// Opens a git repo and exposes reads keyed on SHA. Holds a sley repository
/// handle internally; clone-cheap via `&` reference only.
pub struct GitSource {
    repo: SleyRepository,
    /// Whether the source repo carries `refs/notes/heddle`, resolved once and
    /// cached. Most imported repos (e.g. a plain ripgrep clone) have no such
    /// ref, so `read_heddle_note_bytes` can short-circuit to `None` instead of
    /// attempting a notes-tree walk per commit (a 4,410-commit import otherwise
    /// pays 4,410 wasted notes lookups).
    heddle_notes_present: std::sync::OnceLock<bool>,
}

impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource")
            .field("git_dir", &self.repo.git_dir())
            .finish()
    }
}

impl GitSource {
    /// Open a repo at `path`. Uses sley's discovery first (works from a
    /// worktree subdirectory), falling back to direct open for explicit
    /// `.git` dirs.
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        // Try `discover` first (works from any subdirectory of a worktree),
        // fall back to `open` for explicit `.git` dirs. Both errors are
        // surfaced through our own string-typed Git variant — we don't care
        // which one fired; the user cares whether the path was usable.
        let repo = match SleyRepository::discover(path) {
            Ok(r) => r,
            Err(_) => SleyRepository::open(path)
                .map_err(|e| IngestError::Git(format!("open {}: {e}", path.display())))?,
        };
        Ok(Self {
            repo,
            heddle_notes_present: std::sync::OnceLock::new(),
        })
    }

    /// Whether `refs/notes/heddle` exists in the source repo, resolved once and
    /// memoized. Absence is the common case (most imports have no heddle notes),
    /// and lets [`Self::read_heddle_note_bytes`] skip the per-commit notes walk.
    fn heddle_notes_present(&self) -> bool {
        *self
            .heddle_notes_present
            .get_or_init(|| matches!(self.repo.find_reference("refs/notes/heddle"), Ok(Some(_))))
    }

    fn resolve_ref_commit(&self, name: &str, target: &SleyRefTarget) -> RefResolution {
        let oid = match target {
            SleyRefTarget::Direct(oid) => *oid,
            SleyRefTarget::Symbolic(_) => match self.repo.find_reference(name) {
                Ok(Some(reference)) => match reference.peeled_oid(&self.repo) {
                    Ok(Some(oid)) => oid,
                    Ok(None) | Err(_) => return RefResolution::PeelFailed,
                },
                Ok(None) | Err(_) => return RefResolution::PeelFailed,
            },
        };

        match sley::plumbing::sley_rev::peel_to_commit(
            self.repo.objects().as_ref(),
            self.repo.object_format(),
            &oid,
        ) {
            Ok(commit_oid) if is_commit(&self.repo, commit_oid) => {
                RefResolution::Commit(commit_oid)
            }
            Ok(_) => RefResolution::NonCommit,
            Err(_) if self.repo.read_object(&oid).is_ok() => RefResolution::NonCommit,
            Err(_) => RefResolution::PeelFailed,
        }
    }

    /// Enumerate every ref that resolves to a commit, alongside the
    /// per-namespace counters callers may want for a "what did we see"
    /// summary. Callers who only need the heads should use
    /// [`Self::collect_refs`].
    pub fn collect_refs_detailed(&self) -> crate::Result<(Vec<RefHead>, RefDiscoveryStats)> {
        let mut out = Vec::new();
        let mut stats = RefDiscoveryStats::default();

        let refs = self
            .repo
            .references()
            .list_refs()
            .map_err(|e| IngestError::Git(format!("references: {e}")))?;

        for reference in refs {
            let full_name = reference.name;
            let Some((namespace, short_name)) = classify_ref_name(&full_name) else {
                continue;
            };

            // Skip `refs/remotes/<remote>/HEAD` — it's a symbolic ref to
            // another remote-tracking branch we'll emit separately. Letting
            // it through would create a duplicate thread named `origin/HEAD`
            // pointing at the same commit as `origin/main`.
            if namespace == RefNamespace::RemoteBranch && short_name.ends_with("/HEAD") {
                stats.symbolic_skipped += 1;
                continue;
            }

            let target = match self.resolve_ref_commit(&full_name, &reference.target) {
                RefResolution::Commit(target) => target,
                RefResolution::PeelFailed => {
                    stats.peel_failed += 1;
                    continue;
                }
                RefResolution::NonCommit => {
                    stats.non_commit_skipped += 1;
                    continue;
                }
            };

            match namespace {
                RefNamespace::Branch => stats.local_branches += 1,
                RefNamespace::Tag => stats.tags += 1,
                RefNamespace::RemoteBranch => stats.remote_branches += 1,
            }
            out.push(RefHead {
                short_name,
                full_name,
                namespace,
                target_sha: target.to_string(),
            });
        }

        // Deterministic order: namespace (branches first, then tags,
        // then remote branches), then name within each namespace.
        out.sort_by(|a, b| {
            (a.namespace as u8, &a.short_name).cmp(&(b.namespace as u8, &b.short_name))
        });
        Ok((out, stats))
    }

    /// Convenience: discards the per-namespace stats. Existing callers
    /// that don't care about the breakdown stay unchanged; the new
    /// importer summary uses [`Self::collect_refs_detailed`].
    pub fn collect_refs(&self) -> crate::Result<Vec<RefHead>> {
        self.collect_refs_detailed().map(|(heads, _)| heads)
    }

    /// Build a forward-edge index of the commit graph: for each commit
    /// SHA in `commits`, list the SHAs whose `parents` reference it.
    /// Cheap (single pass) and lets [`Self::descendants_of`] answer
    /// "is X reachable forward from Y?" via BFS without re-walking the
    /// commit list each time.
    ///
    /// This is the substrate for the matcher's *lineage* signal: a
    /// session that ran in `~/.codex/worktrees/<id>/heddle` with
    /// `session_meta.git.commit_hash = <starting>` could legitimately
    /// have authored any commit that descends from `<starting>`,
    /// including squash merges that landed days later. The lineage
    /// gate is what saves those matches from the 60-minute time gate.
    pub fn child_index(commits: &[CommitEntry]) -> ChildIndex {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for c in commits {
            for p in &c.parents {
                children.entry(p.clone()).or_default().push(c.sha.clone());
            }
        }
        ChildIndex { children }
    }
}

/// A reverse map of the parent-of relation: `children[p]` lists every
/// commit whose parents include `p`. Built by [`GitSource::child_index`]
/// from the same `Vec<CommitEntry>` the importer already produces; we
/// don't re-walk the repo.
///
/// Cheap to query: BFS forward through this map yields the full set of
/// descendants of any commit in the index. Querying for a SHA the index
/// doesn't know about returns an empty set — safe for sessions whose
/// `starting_commit` predates the import window.
#[derive(Clone, Debug, Default)]
pub struct ChildIndex {
    children: HashMap<String, Vec<String>>,
}

impl ChildIndex {
    /// Set of every commit reachable forward from `root`, *not including*
    /// `root` itself. The "not including" choice means
    /// `descendants_of(X).contains(X) == false`; callers who want
    /// "X-or-anything-after" should test both.
    ///
    /// Returns an empty set if `root` isn't in the index.
    pub fn descendants_of(&self, root: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut frontier: Vec<String> = self.children.get(root).cloned().unwrap_or_default();
        while let Some(sha) = frontier.pop() {
            if !out.insert(sha.clone()) {
                continue;
            }
            if let Some(kids) = self.children.get(&sha) {
                frontier.extend(kids.iter().cloned());
            }
        }
        out
    }
}

impl GitSource {
    /// Read one commit by SHA.
    pub fn read_commit(&self, sha: &str) -> crate::Result<CommitEntry> {
        let oid = parse_oid(self.repo.object_format(), sha)?;
        let object = self
            .repo
            .read_object(&oid)
            .map_err(|e| IngestError::Git(format!("find_commit {sha}: {e}")))?;
        if object.object_type != GitObjectType::Commit {
            return Err(IngestError::Git(format!(
                "object {sha} is {}, not a commit",
                object.object_type.as_str()
            )));
        }
        let commit = sley::CommitObject::parse_ref(self.repo.object_format(), &object.body)
            .map_err(|e| IngestError::Git(format!("parse commit {sha}: {e}")))?;

        let tree_sha = commit.tree.to_string();

        let parents: Vec<String> = commit.parents.iter().map(ToString::to_string).collect();

        let author = signature_from_bytes(commit.author, sha, "author")?;
        let committer = signature_from_bytes(commit.committer, sha, "committer")?;
        let authored_at = author.time;
        let committed_at = committer.time;

        let message = commit.message.to_vec();

        let extra_headers = collect_commit_extra_headers(&object.body);
        let heddle_note = self.read_heddle_note_bytes(&oid.to_string())?;

        Ok(CommitEntry {
            sha: oid.to_string(),
            tree_sha,
            parents,
            author,
            committer,
            message,
            authored_at,
            committed_at,
            extra_headers,
            heddle_note,
        })
    }

    /// Read Heddle's metadata note for `sha`, if the source repository
    /// carries `refs/notes/heddle`.
    pub fn read_heddle_note_bytes(&self, sha: &str) -> crate::Result<Option<Vec<u8>>> {
        // Short-circuit before touching the object format / notes tree when the
        // source repo has no `refs/notes/heddle` — the overwhelmingly common
        // case. This collapses the per-commit notes lookup to a single cached
        // ref-existence check for the whole import.
        if !self.heddle_notes_present() {
            return Ok(None);
        }
        let oid = parse_oid(self.repo.object_format(), sha)?;
        let notes_ref = sley::notes::NotesRef::expand("refs/notes/heddle");
        self.repo
            .read_note_bytes(&notes_ref, &oid)
            .map_err(|e| IngestError::Git(format!("read Heddle note for {sha}: {e}")))
    }

    /// Read the direct children of a git tree (non-recursive).
    pub fn read_tree(&self, tree_sha: &str) -> crate::Result<Vec<TreeChild>> {
        let oid = parse_oid(self.repo.object_format(), tree_sha)?;
        let entries = if oid == sley::ObjectId::empty_tree(self.repo.object_format()) {
            Vec::new()
        } else {
            self.repo
                .read_tree(&oid)
                .map_err(|e| IngestError::Git(format!("find_tree {tree_sha}: {e}")))?
                .entries
        };

        let mut out = Vec::new();
        for entry in entries {
            let kind = match entry.mode {
                0o040000 => TreeChildKind::Tree,
                0o100644 => TreeChildKind::Blob { executable: false },
                0o100755 => TreeChildKind::Blob { executable: true },
                0o120000 => TreeChildKind::Symlink,
                0o160000 => TreeChildKind::Gitlink,
                mode => {
                    return Err(IngestError::Git(format!(
                        "tree entry {} has unsupported mode {:o}",
                        String::from_utf8_lossy(entry.name.as_bytes()),
                        mode
                    )));
                }
            };
            let raw_name = entry.name.as_bytes().to_vec();
            out.push(TreeChild {
                name: String::from_utf8_lossy(&raw_name).into_owned(),
                raw_name,
                sha: entry.oid.to_string(),
                kind,
            });
        }
        Ok(out)
    }

    /// Read a blob's full contents into memory. Callers should treat this
    /// as the authoritative byte stream for the import — subsequent blob
    /// translation hashes these bytes directly.
    pub fn read_blob(&self, blob_sha: &str) -> crate::Result<Vec<u8>> {
        let oid = parse_oid(self.repo.object_format(), blob_sha)?;
        let object = self
            .repo
            .read_object(&oid)
            .map_err(|e| IngestError::Git(format!("find_object {blob_sha}: {e}")))?;
        if object.object_type != GitObjectType::Blob {
            return Err(IngestError::Git(format!(
                "object {blob_sha} is {}, not a blob",
                object.object_type.as_str()
            )));
        }
        Ok(object.body.clone())
    }

    /// Iterate every reflog entry across `HEAD` and every local branch/tag
    /// ref. Entries are returned in (ref-name asc, file-order) — within a
    /// ref, that's oldest-first, which matches how git writes the log.
    ///
    /// Entries whose target ref has no reflog (common for tags, which
    /// aren't reflogged by default) are silently skipped. Malformed lines
    /// inside an otherwise-readable log are skipped with a debug trace —
    /// the oplog emitter is lossy-by-design, not brittle.
    pub fn collect_reflog(&self) -> crate::Result<Vec<ReflogEntry>> {
        let mut out = Vec::new();

        // HEAD — its reflog captures every checkout/commit/reset the user
        // made through the working tree, which is exactly the honesty
        // signal we care about for the oplog.
        let refs = self.repo.references();
        collect_one_reflog(&refs, "HEAD", &mut out)?;

        // Every local branch + tag.
        for reference in refs
            .list_refs()
            .map_err(|e| IngestError::Git(format!("references: {e}")))?
        {
            if reference.name.starts_with("refs/heads/") || reference.name.starts_with("refs/tags/")
            {
                collect_one_reflog(&refs, &reference.name, &mut out)?;
            }
        }

        Ok(out)
    }

    /// Iterate reflog entries only for the selected local branch/tag refs.
    ///
    /// Deliberately excludes `HEAD`: checkout/reset entries in `HEAD` can
    /// mention unrelated branch tips as their previous target, which would
    /// make a scoped import unexpectedly pull in commits outside the selected
    /// refs. Remote-tracking refs usually have no local reflog, so they are
    /// imported from their live tip only.
    pub fn collect_reflog_for_refs(&self, heads: &[RefHead]) -> crate::Result<Vec<ReflogEntry>> {
        let mut out = Vec::new();
        let refs = self.repo.references();
        for head in heads {
            if matches!(head.namespace, RefNamespace::Branch | RefNamespace::Tag) {
                collect_one_reflog(&refs, &head.full_name, &mut out)?;
            }
        }
        Ok(out)
    }

    /// Every distinct commit SHA referenced by any reflog entry that still
    /// exists in the object database. Intended to be merged into the seed
    /// set for [`Self::commits_topo`] so force-pushed or amended commits
    /// still get translated into Heddle states.
    pub fn reflog_commit_shas(&self) -> crate::Result<Vec<String>> {
        let entries = self.collect_reflog()?;
        Ok(self.reflog_commit_shas_from_entries(&entries))
    }

    /// Deduplicate and filter commit SHAs from a caller-provided reflog set.
    /// Scoped import uses this after it has selected the reflogs that belong
    /// to the requested refs.
    pub fn reflog_commit_shas_from_entries(&self, entries: &[ReflogEntry]) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for entry in entries {
            for sha in [&entry.previous_sha, &entry.new_sha].into_iter().flatten() {
                if !seen.insert(sha.clone()) {
                    continue;
                }
                if !self.object_is_commit(sha) {
                    continue;
                }
                out.push(sha.clone());
            }
        }
        out.sort();
        out
    }

    /// `true` if `sha` resolves to a commit still present in the odb.
    /// Used to filter pruned / dangling SHAs out of reflog-derived seeds.
    fn object_is_commit(&self, sha: &str) -> bool {
        let Ok(oid) = parse_oid(self.repo.object_format(), sha) else {
            return false;
        };
        is_commit(&self.repo, oid)
    }

    /// Gather every commit reachable from the given heads, in
    /// parent-before-child order. Inside a "generation" (commits with the
    /// same discovered depth) ties break on committer time then SHA.
    ///
    /// Commits appear exactly once even if multiple refs reach them.
    ///
    /// Resident-set: all N commits held in memory simultaneously
    /// (~3× commit count × CommitEntry). Acceptable for batch import; revisit
    /// for streaming if repos exceed ~1M commits.
    pub fn commits_topo(
        &self,
        heads: impl IntoIterator<Item = String>,
    ) -> crate::Result<Vec<CommitEntry>> {
        self.commits_topo_with_progress(heads, None)
    }

    /// [`Self::commits_topo`] with a callback that reports how many commits
    /// have been read while the final total is still being discovered.
    pub fn commits_topo_with_progress(
        &self,
        heads: impl IntoIterator<Item = String>,
        mut progress: Option<&mut dyn FnMut(usize)>,
    ) -> crate::Result<Vec<CommitEntry>> {
        // BFS from heads; dedupe on SHA.
        let mut queue: VecDeque<String> = heads.into_iter().collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut entries: HashMap<String, CommitEntry> = HashMap::new();
        if let Some(progress) = progress.as_deref_mut() {
            progress(0);
        }

        while let Some(sha) = queue.pop_front() {
            if seen.contains(&sha) {
                continue;
            }
            seen.insert(sha.clone());
            let entry = self.read_commit(&sha)?;
            for p in &entry.parents {
                if !seen.contains(p) {
                    queue.push_back(p.clone());
                }
            }
            entries.insert(sha, entry);
            if let Some(progress) = progress.as_deref_mut() {
                progress(entries.len());
            }
        }

        // Kahn's algorithm for a stable parent-before-child order.
        //
        // In-degree counts how many *parents of this commit* live in our
        // set (so roots have in-degree 0). We only count edges that land
        // inside the collected set; parents outside the set (e.g. from
        // shallow clones) don't gate the commit.
        let mut indeg: HashMap<&str, usize> = entries.keys().map(|sha| (sha.as_str(), 0)).collect();
        for (sha, entry) in entries.iter() {
            for p in &entry.parents {
                if entries.contains_key(p) {
                    *indeg.get_mut(sha.as_str()).expect("indeg for commit") += 1;
                }
            }
        }

        // Collect all zero-indegree roots into a sorted frontier so output
        // order is deterministic run-to-run.
        let mut frontier: Vec<&str> = indeg
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(s, _)| *s)
            .collect();
        sort_by_time_then_sha(&mut frontier, &entries);

        let total_entries = entries.len();
        let mut ordered = Vec::with_capacity(total_entries);
        // children[p] = list of commits that have p as a parent.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for (sha, entry) in entries.iter() {
            for p in &entry.parents {
                if entries.contains_key(p) {
                    children.entry(p.as_str()).or_default().push(sha.as_str());
                }
            }
        }

        while let Some(sha) = frontier.pop() {
            // `pop` takes the lexicographically-highest after our stable
            // sort, so reverse the sort sense so the *smallest* leaves first.
            // To keep that invariant sane and documented, we instead re-sort
            // after each drain — the frontier is small enough that this is
            // cheap.
            ordered.push(sha.to_string());
            if let Some(kids) = children.remove(sha) {
                for k in kids {
                    let d = indeg.get_mut(k).expect("indeg for child");
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        frontier.push(k);
                    }
                }
                sort_by_time_then_sha(&mut frontier, &entries);
            }
        }

        if ordered.len() != total_entries {
            return Err(IngestError::Other(format!(
                "topo sort dropped commits: {} in graph, {} emitted — cycle?",
                total_entries,
                ordered.len()
            )));
        }

        let mut out = Vec::with_capacity(total_entries);
        for sha in ordered {
            out.push(entries.remove(&sha).expect("sha present in entries"));
        }

        Ok(out)
    }
}

fn classify_ref_name(full_name: &str) -> Option<(RefNamespace, String)> {
    full_name
        .strip_prefix("refs/heads/")
        .map(|short| (RefNamespace::Branch, short.to_string()))
        .or_else(|| {
            full_name
                .strip_prefix("refs/tags/")
                .map(|short| (RefNamespace::Tag, short.to_string()))
        })
        .or_else(|| {
            full_name
                .strip_prefix("refs/remotes/")
                .map(|short| (RefNamespace::RemoteBranch, short.to_string()))
        })
}

/// Drain one reflog into `out`. Quietly tolerates a missing log
/// (common for tags and freshly-created refs) by treating it as "no
/// entries". Malformed lines are skipped — we'd rather degrade to fewer
/// oplog entries than abort the whole import.
fn collect_one_reflog(
    refs: &SleyRefStore,
    ref_name: &str,
    out: &mut Vec<ReflogEntry>,
) -> crate::Result<()> {
    let entries = match refs.read_reflog(ref_name) {
        Ok(entries) if entries.is_empty() => return Ok(()),
        Ok(entries) => entries,
        Err(e) => {
            // A broken reflog shouldn't sink the rest of the import.
            tracing::debug!(ref_name, error = %e, "reflog read failed; skipping");
            return Ok(());
        }
    };

    for entry in entries {
        // `null_sha` is 40 zeros; map it to `None` so the caller doesn't
        // have to special-case creation / deletion markers downstream.
        let prev = oid_hex_or_none(&entry.old_oid);
        let new = oid_hex_or_none(&entry.new_oid);
        let Some(signature) = signature_from_raw(&entry.committer) else {
            tracing::debug!(ref_name, "malformed reflog signature; skipping entry");
            continue;
        };
        out.push(ReflogEntry {
            ref_name: ref_name.to_string(),
            previous_sha: prev,
            new_sha: new,
            signature,
            message: String::from_utf8_lossy(&entry.message).into_owned(),
        });
    }
    Ok(())
}

fn oid_hex_or_none(oid: &SleyObjectId) -> Option<String> {
    (!oid.is_null()).then(|| oid.to_string())
}

fn sort_by_time_then_sha(frontier: &mut [&str], entries: &HashMap<String, CommitEntry>) {
    // Sort so that pop() yields oldest-first (smallest time, ties → sha).
    //
    // pop() pulls from the end, so we want the oldest at the end: descending
    // sort, with oldest commits at the tail.
    frontier.sort_by(|a, b| {
        let ea = entries.get(*a).expect("a in entries");
        let eb = entries.get(*b).expect("b in entries");
        // Newest first, so oldest ends up at the tail (where pop takes from).
        eb.committed_at.cmp(&ea.committed_at).then_with(|| b.cmp(a))
    });
}

fn parse_oid(format: ObjectFormat, sha: &str) -> crate::Result<SleyObjectId> {
    SleyObjectId::from_hex(format, sha)
        .map_err(|e| IngestError::Git(format!("parse oid {sha}: {e}")))
}

fn signature_from_bytes(raw: &[u8], sha: &str, field: &str) -> crate::Result<GitSignature> {
    signature_from_raw(raw)
        .ok_or_else(|| IngestError::Git(format!("{field} {sha}: invalid signature")))
}

fn signature_from_raw(raw: &[u8]) -> Option<GitSignature> {
    let sig = SleySignature::from_ident_line(raw)?;
    let time = sig.time;
    let tz_offset = i32::from(time.timezone_offset_minutes) * 60;
    Some(GitSignature {
        name: String::from_utf8_lossy(sig.name.as_bytes()).into_owned(),
        email: String::from_utf8_lossy(sig.email.as_bytes()).into_owned(),
        time: Utc
            .timestamp_opt(time.seconds, 0)
            .single()
            .unwrap_or_else(Utc::now),
        tz_offset,
    })
}

/// Collect a commit's extension headers in their original on-the-wire order,
/// as raw bytes so non-UTF8 values survive. ORDER IS LOAD-BEARING for #566
/// byte-reconstruction.
///
/// Built straight from the raw commit object bytes via
/// [`objects::object::parse_commit_extension_headers`] so `encoding` / `gpgsig`
/// / `mergetag` / any unknown header all land at their TRUE captured position
/// through one code path — the SAME path the bridge importer uses. We do NOT
/// stitch the vec from typed accessors (`CommitRef::encoding`, ...): some
/// headers are surfaced outside `extra_headers`, and re-inserting them by hand
/// reorders them (the close-the-class bug this replaces). #564 de-lossy step 1.
fn collect_commit_extra_headers(raw_commit: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    objects::object::parse_commit_extension_headers(raw_commit)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    /// Build a throwaway git repo with a known topology, then verify the
    /// walker sees exactly what we put in. Keeps the suite honest without
    /// depending on the host repo's state.
    fn seed_repo(path: &Path) -> String {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(path.join("a.txt"), "hello").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(path.join("b.txt"), "world").unwrap();
        run(&["add", "b.txt"]);
        run(&["commit", "-q", "-m", "second commit"]);
        run(&["tag", "-a", "v0.1", "-m", "tag v0.1"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .expect("rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Follow-up C: ingest's ref discovery must not crash on annotated
    /// tags pointing at non-commit objects (the QA found
    /// `git/git`'s `refs/tags/junio-gpg-pub` → blob and
    /// `git-lfs`'s `refs/tags/core-gpg-keys` → tree both made the
    /// pre-fix walker error with "Expected commit but got blob/tree").
    /// Such refs should be counted in `non_commit_skipped` and excluded
    /// from the head list.
    #[test]
    fn collect_refs_skips_tag_pointing_at_blob() {
        let tmp = TempDir::new().unwrap();
        let _head = seed_repo(tmp.path());

        // Construct a blob-pointing annotated tag the way mature OSS
        // repos do for shipping signing keys: hash a plain blob, then
        // mktag with target=that-blob, target-type=blob, then
        // update-ref refs/tags/<name>.
        let path = tmp.path();
        let run = |args: &[&str], stdin: Option<&str>| -> String {
            use std::process::Stdio;
            let mut child = Command::new("git")
                .args(args)
                .current_dir(path)
                .stdin(if stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn git");
            if let Some(input) = stdin {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
            }
            let out = child.wait_with_output().expect("wait");
            assert!(out.status.success(), "git {:?} failed", args);
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };

        let blob_sha = run(
            &["hash-object", "-w", "--stdin"],
            Some(
                "-----BEGIN PGP PUBLIC KEY BLOCK-----\nfake-key\n-----END PGP PUBLIC KEY BLOCK-----\n",
            ),
        );
        let tag_payload = format!(
            "object {}\ntype blob\ntag gpg-pub\ntagger Test <test@example.com> 1700000000 +0000\n\nGPG public key\n",
            blob_sha
        );
        let tag_sha = run(&["mktag"], Some(&tag_payload));
        run(&["update-ref", "refs/tags/gpg-pub", &tag_sha], None);

        let src = GitSource::open(path).expect("open");
        let (refs, stats) = src.collect_refs_detailed().expect("collect_refs_detailed");

        // The blob-pointing tag must NOT appear in the head list (the
        // commit-translating downstream can't model it).
        assert!(
            !refs.iter().any(|r| r.short_name == "gpg-pub"),
            "gpg-pub (tag → blob) should be excluded from head list"
        );
        assert!(
            stats.non_commit_skipped >= 1,
            "non_commit_skipped should record the gpg-pub tag, got {stats:?}"
        );
        // The healthy ref (v0.1, annotated tag → commit) must still be
        // present — non-commit guard must not mass-skip annotated tags.
        assert!(
            refs.iter().any(|r| r.short_name == "v0.1"),
            "annotated commit-pointing tags should still be listed"
        );
    }

    #[test]
    fn opens_and_lists_refs() {
        let tmp = TempDir::new().unwrap();
        let _head = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).expect("open");
        let refs = src.collect_refs().expect("collect_refs");

        let branch_count = refs
            .iter()
            .filter(|r| r.namespace == RefNamespace::Branch)
            .count();
        let tag_count = refs
            .iter()
            .filter(|r| r.namespace == RefNamespace::Tag)
            .count();

        assert_eq!(branch_count, 1, "expected one branch (main)");
        assert_eq!(tag_count, 1, "expected one tag (v0.1)");
        assert!(refs.iter().any(|r| r.short_name == "main"));
        assert!(refs.iter().any(|r| r.short_name == "v0.1"));
    }

    #[test]
    fn reads_commit_with_parents_and_author() {
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let commit = src.read_commit(&head).unwrap();

        assert_eq!(commit.sha, head);
        assert_eq!(commit.parents.len(), 1, "second commit has one parent");
        assert_eq!(commit.author.name, "Test");
        assert_eq!(commit.author.email, "test@example.com");
        assert!(String::from_utf8_lossy(&commit.message).contains("second commit"));
    }

    #[test]
    fn heddle_note_absent_repo_short_circuits_to_none() {
        // The common case (e.g. a plain ripgrep clone): no `refs/notes/heddle`.
        // `read_heddle_note_bytes` must return `None` without walking a notes
        // tree, and the presence flag must read `false`.
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        assert!(
            !src.heddle_notes_present(),
            "repo without refs/notes/heddle must report notes absent"
        );
        assert_eq!(
            src.read_heddle_note_bytes(&head).unwrap(),
            None,
            "absent-notes repo must short-circuit to None"
        );
        // The walked commit carries no note either.
        let commit = src.read_commit(&head).unwrap();
        assert_eq!(commit.heddle_note, None);
    }

    #[test]
    fn heddle_note_present_repo_reads_payload() {
        // The present path must be preserved: a repo WITH refs/notes/heddle
        // still surfaces the note payload byte-for-byte.
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());
        let path = tmp.path();

        let note_body = "heddle-metadata-payload";
        let status = Command::new("git")
            .args(["notes", "--ref=heddle", "add", "-m", note_body, &head])
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git notes add");
        assert!(status.success(), "git notes add failed");

        let src = GitSource::open(path).unwrap();
        assert!(
            src.heddle_notes_present(),
            "repo with refs/notes/heddle must report notes present"
        );
        let bytes = src
            .read_heddle_note_bytes(&head)
            .unwrap()
            .expect("note should be present");
        // git notes stores the message with a trailing newline.
        assert_eq!(String::from_utf8_lossy(&bytes).trim_end(), note_body);

        // A commit WITHOUT a note in a notes-present repo still yields None.
        let first = src
            .read_commit(&head)
            .unwrap()
            .parents
            .first()
            .cloned()
            .expect("head has a parent");
        assert_eq!(
            src.read_heddle_note_bytes(&first).unwrap(),
            None,
            "un-annotated commit must yield None even when notes ref exists"
        );

        // And the head commit's walked entry carries the note bytes.
        let commit = src.read_commit(&head).unwrap();
        assert_eq!(
            commit
                .heddle_note
                .map(|b| String::from_utf8_lossy(&b).trim_end().to_string()),
            Some(note_body.to_string()),
        );
    }

    /// Close-the-class conformance (ingest path): a commit whose extension
    /// headers are in NON-canonical order — `x-custom`, then a folded `gpgsig`,
    /// then `encoding`, then a folded `mergetag` — must reach `extra_headers` in
    /// that exact on-the-wire order with byte-exact values. Real git always
    /// emits `encoding` first, so this order can only be hand-crafted; it is
    /// precisely what a typed-field-stitching importer would mangle. Mirrors the
    /// bridge-path test of the same name and the shared parser's unit test.
    #[test]
    fn read_commit_preserves_noncanonical_extension_header_order() {
        use std::process::Stdio;

        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());
        let path = tmp.path();

        let run = |args: &[&str], stdin: Option<&[u8]>| -> String {
            let mut child = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdin(if stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn git");
            if let Some(input) = stdin {
                use std::io::Write;
                child.stdin.as_mut().unwrap().write_all(input).unwrap();
            }
            let out = child.wait_with_output().expect("wait");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };

        let tree = run(&["rev-parse", "HEAD^{tree}"], None);

        // `--literally` so git writes the non-canonical header order verbatim
        // instead of normalising/rejecting it.
        let content: Vec<u8> = [
            format!("tree {tree}").into_bytes(),
            format!("parent {head}").into_bytes(),
            b"author Alice <alice@example.com> 1700000000 +0000".to_vec(),
            b"committer Bob <bob@example.com> 1700000100 +0000".to_vec(),
            b"x-custom custom value".to_vec(),
            b"gpgsig -----BEGIN PGP SIGNATURE-----".to_vec(),
            b" sig-line-1".to_vec(),
            b" -----END PGP SIGNATURE-----".to_vec(),
            b"encoding ISO-8859-1".to_vec(),
            b"mergetag object 3333333333333333333333333333333333333333".to_vec(),
            b" type commit".to_vec(),
            b" tag sidetag".to_vec(),
            b"".to_vec(),
            b"the commit message".to_vec(),
            b"".to_vec(),
        ]
        .join(&b'\n');

        let sha = run(
            &[
                "hash-object",
                "--literally",
                "-w",
                "-t",
                "commit",
                "--stdin",
            ],
            Some(&content),
        );

        let src = GitSource::open(path).expect("open");
        let commit = src.read_commit(&sha).expect("read_commit");

        let expected: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"x-custom".to_vec(), b"custom value".to_vec()),
            (
                b"gpgsig".to_vec(),
                b"-----BEGIN PGP SIGNATURE-----\nsig-line-1\n-----END PGP SIGNATURE-----".to_vec(),
            ),
            (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
            (
                b"mergetag".to_vec(),
                b"object 3333333333333333333333333333333333333333\ntype commit\ntag sidetag"
                    .to_vec(),
            ),
        ];
        assert_eq!(commit.extra_headers, expected);
    }

    #[test]
    fn reads_tree_children() {
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let commit = src.read_commit(&head).unwrap();
        let children = src.read_tree(&commit.tree_sha).unwrap();

        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        for c in &children {
            assert!(matches!(c.kind, TreeChildKind::Blob { executable: false }));
        }
    }

    #[test]
    fn reads_blob_bytes() {
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());
        let src = GitSource::open(tmp.path()).unwrap();
        let commit = src.read_commit(&head).unwrap();
        let children = src.read_tree(&commit.tree_sha).unwrap();
        let a_sha = &children.iter().find(|c| c.name == "a.txt").unwrap().sha;
        let bytes = src.read_blob(a_sha).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn topo_orders_parents_before_children() {
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let commits = src.commits_topo(vec![head.clone()]).unwrap();
        assert_eq!(commits.len(), 2);

        // First emitted must be the root (no parents).
        assert!(commits[0].parents.is_empty());
        // Second must be the tip.
        assert_eq!(commits[1].sha, head);
        // And its parent must match the first.
        assert_eq!(commits[1].parents, vec![commits[0].sha.clone()]);
    }

    #[test]
    fn collects_head_reflog_entries() {
        let tmp = TempDir::new().unwrap();
        let _ = seed_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let entries = src.collect_reflog().expect("collect_reflog");

        // Two commits on main → at least two HEAD reflog lines.
        let head_lines: Vec<_> = entries.iter().filter(|e| e.ref_name == "HEAD").collect();
        assert!(
            head_lines.len() >= 2,
            "expected >=2 HEAD reflog entries, got {}: {:?}",
            head_lines.len(),
            entries
        );
        // First commit is a ref-creation → previous_sha is null, hence None.
        assert!(
            head_lines.iter().any(|e| e.previous_sha.is_none()),
            "expected a creation entry with null previous sha"
        );
        // Every new_sha that's set must be 40 hex chars.
        for e in &head_lines {
            if let Some(s) = &e.new_sha {
                assert_eq!(s.len(), 40, "new_sha not 40 chars: {s}");
            }
        }
    }

    #[test]
    fn reflog_shas_survive_force_reset() {
        // Force-move main backwards so commit 2 is dangling-reachable only
        // via the reflog. The walker should still surface its SHA.
        let tmp = TempDir::new().unwrap();
        let head_sha = seed_repo(tmp.path());

        // Back up to the first commit; the second commit is now only in
        // the reflog (HEAD@{1}).
        let status = Command::new("git")
            .args(["reset", "--hard", "HEAD~1"])
            .current_dir(tmp.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success());

        let src = GitSource::open(tmp.path()).unwrap();
        let shas = src.reflog_commit_shas().unwrap();

        assert!(
            shas.contains(&head_sha),
            "reflog should still mention the orphaned tip {head_sha}; got {shas:?}"
        );
    }

    /// Construct a repo that exercises every edge case the importer is
    /// supposed to round-trip cleanly:
    ///
    /// - **Easy tag**: lightweight `v0.1` on the first commit.
    /// - **Mid-history annotated tag**: `v1.0-rc` on the second commit.
    /// - **Tag-of-tag chain**: `v1.0` is an annotated tag whose target is
    ///   `v1.0-rc` (the tag object), forcing `peel_to_id` to chase
    ///   through both layers to land on a commit.
    /// - **Remote-only commit**: a commit reachable *only* via
    ///   `refs/remotes/origin/abandoned`. Without remote-tracking ref
    ///   support this commit is silently dropped from the import.
    /// - **Tag on the remote-only commit**: tag `release-pre` pointing
    ///   at that orphaned commit, exercising both code paths together.
    /// - **Symlink**: `link.txt -> a.txt` — a tree entry with mode
    ///   `120000`. The translator must honour the [`TreeChildKind::Symlink`]
    ///   variant rather than treating it as a blob.
    /// - **Empty file**: `empty.txt` with zero bytes.
    /// - **Executable bit**: `run.sh` with mode `100755`.
    fn seed_edge_case_repo(path: &Path) -> EdgeCaseShas {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        let capture = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git capture");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };

        run(&["init", "-q", "--initial-branch=main"]);

        // Commit 1: lightweight files.
        std::fs::write(path.join("a.txt"), "hello").unwrap();
        std::fs::write(path.join("empty.txt"), "").unwrap();
        // Symlink. macOS + Linux both support `ln -s`; sley reads the
        // resulting tree mode as `Link` regardless of host filesystem.
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", path.join("link.txt")).unwrap();
        run(&["add", "a.txt", "empty.txt"]);
        #[cfg(unix)]
        run(&["add", "link.txt"]);
        run(&["commit", "-q", "-m", "first commit"]);
        let first = capture(&["rev-parse", "HEAD"]);

        // Lightweight tag on the first commit. No `-a`, no `-m`.
        run(&["tag", "v0.1"]);

        // Commit 2: add an executable script.
        std::fs::write(path.join("run.sh"), "#!/bin/sh\necho ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(path.join("run.sh"))
                .unwrap()
                .permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(path.join("run.sh"), perm).unwrap();
        }
        run(&["add", "run.sh"]);
        run(&["commit", "-q", "-m", "second commit"]);
        let second = capture(&["rev-parse", "HEAD"]);

        // Annotated tag on the mid-history commit.
        run(&["tag", "-a", "v1.0-rc", "-m", "release candidate"]);
        // Tag-of-tag: v1.0 points at the v1.0-rc *tag object*, not at
        // its underlying commit. `git rev-parse v1.0-rc` returns the
        // tag SHA, which we feed into `git tag -a` directly.
        let v10_rc_sha = capture(&["rev-parse", "v1.0-rc"]);
        run(&["tag", "-a", "v1.0", "-m", "release", &v10_rc_sha]);

        // Now build a third commit that we'll abandon. We do this on a
        // detached HEAD so the commit isn't reachable from `main` once
        // we move HEAD back.
        run(&["checkout", "-q", "--detach", &second]);
        std::fs::write(path.join("c.txt"), "abandoned\n").unwrap();
        run(&["add", "c.txt"]);
        run(&["commit", "-q", "-m", "abandoned commit"]);
        let abandoned = capture(&["rev-parse", "HEAD"]);
        // Restore main as HEAD so the abandoned commit is unreachable
        // from any local head — it must come in via remote refs only.
        run(&["checkout", "-q", "main"]);

        // Park the abandoned commit under refs/remotes/origin/abandoned.
        // This simulates the "fetched from teammate, never merged" case.
        run(&["update-ref", "refs/remotes/origin/abandoned", &abandoned]);
        // And tag it — exercises the tag-points-at-remote-only-commit
        // path (peel works against any commit, not just reachable ones).
        run(&["update-ref", "refs/tags/release-pre", &abandoned]);

        EdgeCaseShas {
            first,
            second,
            abandoned,
        }
    }

    struct EdgeCaseShas {
        first: String,
        second: String,
        abandoned: String,
    }

    #[test]
    fn collect_refs_captures_remotes_and_chained_tags() {
        let tmp = TempDir::new().unwrap();
        let _shas = seed_edge_case_repo(tmp.path());

        let src = GitSource::open(tmp.path()).expect("open");
        let refs = src.collect_refs().expect("collect_refs");

        // Local branches: main only.
        let local: Vec<&str> = refs
            .iter()
            .filter(|r| r.namespace == RefNamespace::Branch)
            .map(|r| r.short_name.as_str())
            .collect();
        assert_eq!(local, vec!["main"], "got: {refs:#?}");

        // Tags: v0.1 (lightweight), v1.0 (chained), v1.0-rc, release-pre.
        // All four must surface as `Tag`-namespace heads.
        let mut tags: Vec<&str> = refs
            .iter()
            .filter(|r| r.namespace == RefNamespace::Tag)
            .map(|r| r.short_name.as_str())
            .collect();
        tags.sort();
        assert_eq!(
            tags,
            vec!["release-pre", "v0.1", "v1.0", "v1.0-rc"],
            "got: {refs:#?}"
        );

        // Remote tracking: origin/abandoned is the only remote we set;
        // origin/HEAD (if it existed) is filtered out by the walker.
        let remotes: Vec<&str> = refs
            .iter()
            .filter(|r| r.namespace == RefNamespace::RemoteBranch)
            .map(|r| r.short_name.as_str())
            .collect();
        assert_eq!(remotes, vec!["origin/abandoned"], "got: {refs:#?}");
    }

    #[test]
    fn chained_annotated_tag_peels_to_commit() {
        // `v1.0` is an annotated tag whose target is the `v1.0-rc` tag
        // object (also annotated), which in turn points at the second
        // commit. `target_sha` must be the *commit* SHA, not either
        // tag SHA — the sha map can only translate commits.
        let tmp = TempDir::new().unwrap();
        let shas = seed_edge_case_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let refs = src.collect_refs().unwrap();
        let v10 = refs
            .iter()
            .find(|r| r.short_name == "v1.0")
            .expect("v1.0 must be present");
        assert_eq!(
            v10.target_sha, shas.second,
            "v1.0 (tag-of-tag) should peel to the second commit"
        );
    }

    #[test]
    fn remote_only_commit_is_reachable_via_topo_walk() {
        // The abandoned commit lives only under `refs/remotes/origin/abandoned`.
        // Feeding the full ref set into `commits_topo` must pull it in;
        // dropping the remotes (the old behavior) leaves it on the floor.
        let tmp = TempDir::new().unwrap();
        let shas = seed_edge_case_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        let refs = src.collect_refs().unwrap();
        let heads: Vec<String> = refs.iter().map(|r| r.target_sha.clone()).collect();
        let commits = src.commits_topo(heads).unwrap();
        let shas_seen: Vec<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
        assert!(
            shas_seen.contains(&shas.first.as_str()),
            "first commit missing"
        );
        assert!(
            shas_seen.contains(&shas.second.as_str()),
            "second commit missing"
        );
        assert!(
            shas_seen.contains(&shas.abandoned.as_str()),
            "remote-only abandoned commit must be in topo walk; got {shas_seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tree_preserves_symlinks_and_executable_bit() {
        let tmp = TempDir::new().unwrap();
        let shas = seed_edge_case_repo(tmp.path());

        let src = GitSource::open(tmp.path()).unwrap();
        // Inspect the tree at HEAD (second commit). The first-commit
        // tree carries the symlink and the empty file; the second-commit
        // tree adds run.sh.
        let head = src.read_commit(&shas.second).unwrap();
        let children = src.read_tree(&head.tree_sha).unwrap();

        let by_name: std::collections::HashMap<&str, &TreeChild> =
            children.iter().map(|c| (c.name.as_str(), c)).collect();

        let link = by_name.get("link.txt").expect("symlink must be preserved");
        assert!(
            matches!(link.kind, TreeChildKind::Symlink),
            "link.txt should be Symlink, got {:?}",
            link.kind
        );

        let run = by_name.get("run.sh").expect("run.sh must be present");
        assert!(
            matches!(run.kind, TreeChildKind::Blob { executable: true }),
            "run.sh should be executable, got {:?}",
            run.kind
        );

        let plain = by_name.get("a.txt").expect("a.txt must be present");
        assert!(
            matches!(plain.kind, TreeChildKind::Blob { executable: false }),
            "a.txt should be a non-exec blob, got {:?}",
            plain.kind
        );
    }

    #[test]
    fn empty_file_is_a_zero_byte_blob() {
        let tmp = TempDir::new().unwrap();
        let shas = seed_edge_case_repo(tmp.path());
        let src = GitSource::open(tmp.path()).unwrap();
        let first = src.read_commit(&shas.first).unwrap();
        let children = src.read_tree(&first.tree_sha).unwrap();

        let empty = children
            .iter()
            .find(|c| c.name == "empty.txt")
            .expect("empty.txt must round-trip");
        assert!(matches!(empty.kind, TreeChildKind::Blob { .. }));
        let bytes = src.read_blob(&empty.sha).unwrap();
        assert!(
            bytes.is_empty(),
            "empty.txt should produce 0 bytes, got {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn child_index_descendants_walks_forward_through_graph() {
        // Build a four-commit chain: A → B → C, plus an unrelated D
        // descended from B (sibling of C). Verify:
        //   descendants_of(A) == {B, C, D}
        //   descendants_of(B) == {C, D}
        //   descendants_of(C) == {}    (leaf)
        //   descendants_of("missing") == {}
        let make = |sha: &str, parents: &[&str]| CommitEntry {
            sha: sha.into(),
            tree_sha: "t".into(),
            parents: parents.iter().map(|s| (*s).to_string()).collect(),
            author: GitSignature {
                name: "x".into(),
                email: "x".into(),
                time: Utc::now(),
                tz_offset: 0,
            },
            committer: GitSignature {
                name: "x".into(),
                email: "x".into(),
                time: Utc::now(),
                tz_offset: 0,
            },
            message: Vec::new(),
            authored_at: Utc::now(),
            committed_at: Utc::now(),
            extra_headers: Vec::new(),
            heddle_note: None,
        };
        let commits = vec![
            make("A", &[]),
            make("B", &["A"]),
            make("C", &["B"]),
            make("D", &["B"]),
        ];
        let idx = GitSource::child_index(&commits);
        let mut a = idx.descendants_of("A").into_iter().collect::<Vec<_>>();
        a.sort();
        assert_eq!(a, vec!["B".to_string(), "C".to_string(), "D".to_string()]);
        let mut b = idx.descendants_of("B").into_iter().collect::<Vec<_>>();
        b.sort();
        assert_eq!(b, vec!["C".to_string(), "D".to_string()]);
        assert!(idx.descendants_of("C").is_empty());
        assert!(idx.descendants_of("missing").is_empty());
    }

    #[test]
    fn deterministic_across_runs() {
        let tmp = TempDir::new().unwrap();
        let head = seed_repo(tmp.path());
        let src = GitSource::open(tmp.path()).unwrap();

        let a = src.commits_topo(vec![head.clone()]).unwrap();
        let b = src.commits_topo(vec![head.clone()]).unwrap();
        assert_eq!(
            a.iter().map(|c| c.sha.clone()).collect::<Vec<_>>(),
            b.iter().map(|c| c.sha.clone()).collect::<Vec<_>>()
        );
    }
}
