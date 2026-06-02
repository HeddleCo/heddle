// SPDX-License-Identifier: Apache-2.0
//! Translate git refs into Heddle threads and markers.
//!
//! After the state writer has translated every reachable commit, this
//! module points Heddle's refs at the resulting `ChangeId`s. The mapping is
//! mechanical:
//!
//! | Git                              | Heddle                            |
//! |----------------------------------|---------------------------------|
//! | `refs/heads/<branch>`            | thread `<branch>`               |
//! | `refs/tags/<tag>`                | marker `<tag>`                  |
//! | `refs/remotes/<remote>/<branch>` | thread `<remote>/<branch>`      |
//!
//! Slashed ref names (`feature/x`, `release/1.2`, `origin/main`) are
//! preserved verbatim — Heddle's `set_thread` stores them under
//! `.heddle/refs/threads/<slashed/name>` the same way git stores them under
//! `.git/refs/heads/<slashed/name>`. That namespace separation is also
//! what keeps a remote-tracking `origin/main` thread from colliding with
//! a hypothetical local branch literally called `origin/main`.
//!
//! # What if a ref points at an untranslated commit?
//!
//! Shouldn't happen for live refs — those were the seed set for the
//! walker. It *can* happen for reflog-only commits if we hand this
//! emitter a list that includes dangling names. We skip those with a
//! `warn!` rather than aborting; the caller gets a count back in
//! [`RefEmitStats::skipped_unmapped`] and can decide whether that's
//! acceptable.

use objects::object::{MarkerName, ThreadName};
use refs::refs::{RefBackend, RefExpectation};
use tracing::warn;

use crate::{
    IngestError,
    git_walk::{RefHead, RefNamespace},
    sha_map::ShaMap,
};

/// Rolling tally returned by [`RefEmitter::emit`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefEmitStats {
    pub threads_written: usize,
    pub markers_written: usize,
    /// Refs whose target commit SHA had no entry in the [`ShaMap`] —
    /// caller should treat any non-zero count as a correctness signal.
    pub skipped_unmapped: usize,
}

/// Writes threads (branches) and markers (tags) into a Heddle repo.
///
/// Generic over the [`RefBackend`] so the same emitter works against the
/// local `RefManager` (CLI / tests) and the server's Postgres-backed
/// backend — the trait's `async fn` reads forbid `&dyn` dispatch, so the
/// backend is a type parameter.
pub struct RefEmitter<'a, R: RefBackend> {
    refs: &'a R,
    map: &'a ShaMap,
}

impl<'a, R: RefBackend> RefEmitter<'a, R> {
    pub fn new(refs: &'a R, map: &'a ShaMap) -> Self {
        Self { refs, map }
    }

    /// Emit every [`RefHead`] as a thread or marker. Idempotent: calling
    /// twice with the same input is a no-op for any ref whose commit
    /// hasn't moved (the underlying `set_thread` / `create_marker`
    /// overwrite atomically).
    ///
    /// `async` because the marker read (`get_marker`) is an `async`
    /// backend method; for the local `RefManager` the future is
    /// immediately ready.
    pub async fn emit(&self, heads: &[RefHead]) -> crate::Result<RefEmitStats> {
        let mut stats = RefEmitStats::default();
        for head in heads {
            let Some(cid) = self.map.get_commit(&head.target_sha) else {
                warn!(
                    ref_name = %head.full_name,
                    target = %head.target_sha,
                    "skipping ref — target commit not in sha map",
                );
                stats.skipped_unmapped += 1;
                continue;
            };
            match head.namespace {
                RefNamespace::Branch | RefNamespace::RemoteBranch => {
                    // Both land as threads — local branches under their
                    // short name (`main`), remote branches under a
                    // `<remote>/<branch>` short name (`origin/main`). The
                    // slash separation prevents collisions between a
                    // local `main` and `origin/main`; refs stores
                    // slashed names as nested paths under `refs/threads/`.
                    let thread_name = ThreadName::from(head.short_name.as_str());
                    self.refs
                        .set_thread(&thread_name, &cid)
                        .map_err(IngestError::from)?;
                    stats.threads_written += 1;
                }
                RefNamespace::Tag => {
                    // `create_marker` rejects existing names (markers
                    // are write-once by design), so an idempotent
                    // importer has to use `set_marker_cas(Any, …)`.
                    // We still short-circuit when the marker already
                    // points at the same cid — saves a lock cycle.
                    let marker_name = MarkerName::from(head.short_name.as_str());
                    let existing = self
                        .refs
                        .get_marker(&marker_name)
                        .await
                        .map_err(IngestError::from)?;
                    if existing != Some(cid) {
                        self.refs
                            .set_marker_cas(&marker_name, RefExpectation::Any, &cid)
                            .map_err(IngestError::from)?;
                    }
                    stats.markers_written += 1;
                }
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use objects::object::ChangeId;
    use refs::refs::RefManager;
    use tempfile::TempDir;

    use super::*;

    fn fresh_ref_manager() -> (TempDir, RefManager) {
        let tmp = TempDir::new().unwrap();
        let mgr = RefManager::new(tmp.path());
        mgr.init().unwrap();
        (tmp, mgr)
    }

    fn sample_head(name: &str, ns: RefNamespace, git_sha: &str) -> RefHead {
        let full = match ns {
            RefNamespace::Branch => format!("refs/heads/{name}"),
            RefNamespace::Tag => format!("refs/tags/{name}"),
            RefNamespace::RemoteBranch => format!("refs/remotes/{name}"),
        };
        RefHead {
            short_name: name.to_string(),
            full_name: full,
            namespace: ns,
            target_sha: git_sha.to_string(),
        }
    }

    #[test]
    fn writes_branch_as_thread_and_tag_as_marker() {
        let (_tmp, mgr) = fresh_ref_manager();
        let mut map = ShaMap::new();
        let cid = ChangeId::generate();
        let git_sha = "a".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();

        let heads = vec![
            sample_head("main", RefNamespace::Branch, &git_sha),
            sample_head("v0.1", RefNamespace::Tag, &git_sha),
        ];
        let stats = pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();

        assert_eq!(stats.threads_written, 1);
        assert_eq!(stats.markers_written, 1);
        assert_eq!(stats.skipped_unmapped, 0);
        assert_eq!(mgr.get_thread(&ThreadName::new("main")).unwrap(), Some(cid));
        // Markers are listed under list_markers.
        let markers = mgr.list_markers().unwrap();
        assert!(
            markers.iter().any(|m| m == "v0.1"),
            "marker not listed: {markers:?}"
        );
    }

    #[test]
    fn slashed_branch_names_round_trip() {
        let (_tmp, mgr) = fresh_ref_manager();
        let mut map = ShaMap::new();
        let cid = ChangeId::generate();
        let git_sha = "b".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();

        let heads = vec![sample_head(
            "feature/ingest",
            RefNamespace::Branch,
            &git_sha,
        )];
        pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();

        assert_eq!(
            mgr.get_thread(&ThreadName::new("feature/ingest")).unwrap(),
            Some(cid)
        );
    }

    #[test]
    fn skips_refs_with_unmapped_target() {
        let (_tmp, mgr) = fresh_ref_manager();
        // Nothing in the sha map — every ref must be skipped.
        let map = ShaMap::new();
        let git_sha = "c".repeat(40);
        let heads = vec![sample_head("orphan", RefNamespace::Branch, &git_sha)];

        let stats = pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();
        assert_eq!(stats.threads_written, 0);
        assert_eq!(stats.skipped_unmapped, 1);
        assert_eq!(mgr.get_thread(&ThreadName::new("orphan")).unwrap(), None);
    }

    #[test]
    fn remote_branch_lands_as_thread_with_slashed_short_name() {
        // `RemoteBranch` should round-trip through `set_thread`, with
        // `origin/main` stored at `refs/threads/origin/main` so it
        // doesn't collide with a hypothetical local `main`.
        let (_tmp, mgr) = fresh_ref_manager();
        let mut map = ShaMap::new();
        let local_cid = ChangeId::generate();
        let remote_cid = ChangeId::generate();
        let local_sha = "1".repeat(40);
        let remote_sha = "2".repeat(40);
        map.insert_commit(&local_sha, local_cid).unwrap();
        map.insert_commit(&remote_sha, remote_cid).unwrap();

        let heads = vec![
            sample_head("main", RefNamespace::Branch, &local_sha),
            sample_head("origin/main", RefNamespace::RemoteBranch, &remote_sha),
        ];
        let stats = pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();

        assert_eq!(stats.threads_written, 2);
        assert_eq!(
            mgr.get_thread(&ThreadName::new("main")).unwrap(),
            Some(local_cid)
        );
        assert_eq!(
            mgr.get_thread(&ThreadName::new("origin/main")).unwrap(),
            Some(remote_cid)
        );
    }

    #[test]
    fn emit_is_idempotent() {
        let (_tmp, mgr) = fresh_ref_manager();
        let mut map = ShaMap::new();
        let cid = ChangeId::generate();
        let git_sha = "d".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();
        let heads = vec![sample_head("main", RefNamespace::Branch, &git_sha)];

        let first = pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();
        let second = pollster::block_on(RefEmitter::new(&mgr, &map).emit(&heads)).unwrap();

        assert_eq!(first, second);
        assert_eq!(mgr.get_thread(&ThreadName::new("main")).unwrap(), Some(cid));
    }
}
