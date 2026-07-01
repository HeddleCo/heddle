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

use objects::{
    object::{ChangeId, MarkerName, ThreadName, Tree},
    store::ObjectStore,
};
use refs::refs::{RefBackend, RefExpectation, RefUpdate};
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
pub struct RefEmitter<'a, R: RefBackend, S: ObjectStore> {
    refs: &'a R,
    store: &'a S,
    map: &'a ShaMap,
}

impl<'a, R: RefBackend, S: ObjectStore> RefEmitter<'a, R, S> {
    pub fn new(refs: &'a R, store: &'a S, map: &'a ShaMap) -> Self {
        Self { refs, store, map }
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
        let mut threads = Vec::new();
        let mut markers = Vec::new();

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
                    repo::validate_thread_id(&head.short_name).map_err(|error| {
                        IngestError::Other(format!(
                            "Git branch '{}' cannot be imported as a Heddle thread: {error}",
                            head.short_name
                        ))
                    })?;
                    let thread_name = ThreadName::from(head.short_name.as_str());
                    // The divergence check MUST run before the batch publish:
                    // a thread import that would move a thread across divergent
                    // history fails closed here, before any ref is written.
                    if let Some(existing) = self
                        .refs
                        .get_thread(&thread_name)
                        .await
                        .map_err(IngestError::from)?
                        && !self.thread_can_adopt_change(&existing, &cid)?
                    {
                        return Err(IngestError::ThreadDiverged {
                            thread: thread_name.to_string(),
                            branch: head.short_name.clone(),
                            existing,
                            incoming: cid,
                        });
                    }
                    threads.push((thread_name, cid));
                }
                RefNamespace::Tag => {
                    let marker_name = MarkerName::from(head.short_name.as_str());
                    // `create_marker` rejects existing names (markers are
                    // write-once by design), so an idempotent importer has to
                    // use `RefExpectation::Any`. We still short-circuit when
                    // the marker already points at the same cid — keeps the
                    // batch free of no-op churn and matches the prior
                    // per-marker behavior.
                    let existing = self
                        .refs
                        .get_marker(&marker_name)
                        .await
                        .map_err(IngestError::from)?;
                    markers.push((marker_name, cid, existing));
                }
            }
        }

        // Batch every thread + marker write into ONE `update_refs` call so the
        // whole import publishes under a single lock with a single ref-summary
        // rebuild. The previous one-ref-at-a-time loop made N publishes, each
        // rescanning the entire refs dir — Σ ≈ N²/2 file reads (101 refs=2s,
        // 401=15s, 801=52s). `update_refs` validates the whole batch atomically
        // and rejects duplicate paths, so the batch must carry at most one
        // update per distinct path. Git ref names live in disjoint namespaces
        // (`refs/heads`/`refs/remotes` → threads, `refs/tags` → markers) and a
        // ref list never repeats a full name, so each (kind, short_name) pair
        // is already unique across the batch.
        let mut updates: Vec<RefUpdate> = Vec::with_capacity(threads.len() + markers.len());

        for (thread_name, cid) in threads {
            updates.push(RefUpdate::Thread {
                name: thread_name,
                // The divergence check above already validated the move; there
                // is no concurrent writer during import, so `Any` mirrors the
                // single-ref `set_thread` it replaces.
                expected: RefExpectation::Any,
                new: Some(cid),
            });
            stats.threads_written += 1;
        }

        for (marker_name, cid, existing) in markers {
            if existing != Some(cid) {
                updates.push(RefUpdate::Marker {
                    name: marker_name,
                    expected: RefExpectation::Any,
                    new: Some(cid),
                });
            }
            // `markers_written` counts every tag we adopted (including no-op
            // re-points), preserving the prior stat semantics.
            stats.markers_written += 1;
        }

        self.refs.update_refs(&updates).map_err(IngestError::from)?;

        Ok(stats)
    }

    fn thread_can_adopt_change(
        &self,
        existing: &ChangeId,
        incoming: &ChangeId,
    ) -> crate::Result<bool> {
        if existing == incoming || self.thread_is_unclaimed_bootstrap(existing)? {
            return Ok(true);
        }
        change_is_ancestor(self.store, existing, incoming)
    }

    fn thread_is_unclaimed_bootstrap(&self, change_id: &ChangeId) -> crate::Result<bool> {
        let Some(state) = self.store.get_state(change_id).map_err(IngestError::from)? else {
            return Ok(false);
        };
        if !state.parents.is_empty() {
            return Ok(false);
        }
        let Some(tree) = self
            .store
            .get_tree(&state.tree)
            .map_err(IngestError::from)?
        else {
            return Ok(false);
        };
        Ok(tree == Tree::new())
    }
}

fn change_is_ancestor<S: ObjectStore>(
    store: &S,
    ancestor: &ChangeId,
    descendant: &ChangeId,
) -> crate::Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }

    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![*descendant];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(state) = store.get_state(&id).map_err(IngestError::from)? else {
            return Ok(false);
        };
        for parent in state.parents {
            if parent == *ancestor {
                return Ok(true);
            }
            stack.push(parent);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Attribution, ChangeId, Principal, State},
        store::{InMemoryStore, ObjectStore},
    };
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

    fn test_state(store: &InMemoryStore, parents: Vec<ChangeId>) -> ChangeId {
        let tree = store.put_tree(&Tree::new()).unwrap();
        let state = State::new(
            tree,
            parents,
            Attribution::human(Principal::new("Test", "test@example.com")),
        );
        let change_id = state.change_id;
        store.put_state(&state).unwrap();
        change_id
    }

    #[test]
    fn writes_branch_as_thread_and_tag_as_marker() {
        let (_tmp, mgr) = fresh_ref_manager();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let cid = test_state(&store, vec![]);
        let git_sha = "a".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();

        let heads = vec![
            sample_head("main", RefNamespace::Branch, &git_sha),
            sample_head("v0.1", RefNamespace::Tag, &git_sha),
        ];
        let stats = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();

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
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let cid = test_state(&store, vec![]);
        let git_sha = "b".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();

        let heads = vec![sample_head(
            "feature/ingest",
            RefNamespace::Branch,
            &git_sha,
        )];
        pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();

        assert_eq!(
            mgr.get_thread(&ThreadName::new("feature/ingest")).unwrap(),
            Some(cid)
        );
    }

    #[test]
    fn skips_refs_with_unmapped_target() {
        let (_tmp, mgr) = fresh_ref_manager();
        let store = InMemoryStore::new();
        // Nothing in the sha map — every ref must be skipped.
        let map = ShaMap::new();
        let git_sha = "c".repeat(40);
        let heads = vec![sample_head("orphan", RefNamespace::Branch, &git_sha)];

        let stats = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();
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
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let local_cid = test_state(&store, vec![]);
        let remote_cid = test_state(&store, vec![]);
        let local_sha = "1".repeat(40);
        let remote_sha = "2".repeat(40);
        map.insert_commit(&local_sha, local_cid).unwrap();
        map.insert_commit(&remote_sha, remote_cid).unwrap();

        let heads = vec![
            sample_head("main", RefNamespace::Branch, &local_sha),
            sample_head("origin/main", RefNamespace::RemoteBranch, &remote_sha),
        ];
        let stats = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();

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
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let cid = test_state(&store, vec![]);
        let git_sha = "d".repeat(40);
        map.insert_commit(&git_sha, cid).unwrap();
        let heads = vec![sample_head("main", RefNamespace::Branch, &git_sha)];

        let first = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();
        let second = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();

        assert_eq!(first, second);
        assert_eq!(mgr.get_thread(&ThreadName::new("main")).unwrap(), Some(cid));
    }

    #[test]
    fn batched_emit_writes_every_thread_and_marker_exactly_once() {
        // Regression guard for the O(refs²) → single-batch rewrite: a many-ref
        // import must land EVERY branch + tag at its exact change-id, with
        // nothing dropped, duplicated, or mis-targeted. A batched
        // `update_refs` that loses or mis-routes a ref is the cardinal bug.
        let (_tmp, mgr) = fresh_ref_manager();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();

        const N_BRANCHES: usize = 30;
        const N_REMOTES: usize = 10;
        const N_TAGS: usize = 20;

        let mut heads = Vec::new();
        // Each ref gets its OWN distinct commit/change-id so a mis-target
        // (ref A ending up pointing at ref B's cid) is detectable.
        let mut expected_threads = Vec::new();
        let mut expected_markers = Vec::new();

        let mut next_sha = 0u64;
        let fresh = |store: &InMemoryStore, map: &mut ShaMap, n: &mut u64| {
            let cid = test_state(store, vec![]);
            let sha = format!("{:040x}", *n);
            *n += 1;
            map.insert_commit(&sha, cid).unwrap();
            (sha, cid)
        };

        for i in 0..N_BRANCHES {
            let (sha, cid) = fresh(&store, &mut map, &mut next_sha);
            // Mix flat and slashed names to exercise nested path writes.
            let name = if i % 3 == 0 {
                format!("feature/branch-{i}")
            } else {
                format!("branch-{i}")
            };
            heads.push(sample_head(&name, RefNamespace::Branch, &sha));
            expected_threads.push((name, cid));
        }
        for i in 0..N_REMOTES {
            let (sha, cid) = fresh(&store, &mut map, &mut next_sha);
            let name = format!("origin/remote-{i}");
            heads.push(sample_head(&name, RefNamespace::RemoteBranch, &sha));
            expected_threads.push((name, cid));
        }
        for i in 0..N_TAGS {
            let (sha, cid) = fresh(&store, &mut map, &mut next_sha);
            let name = format!("v{i}.0");
            heads.push(sample_head(&name, RefNamespace::Tag, &sha));
            expected_markers.push((name, cid));
        }

        let stats = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();

        assert_eq!(stats.threads_written, N_BRANCHES + N_REMOTES);
        assert_eq!(stats.markers_written, N_TAGS);
        assert_eq!(stats.skipped_unmapped, 0);

        // Every thread present at its EXACT change-id.
        for (name, cid) in &expected_threads {
            assert_eq!(
                mgr.get_thread(&ThreadName::new(name)).unwrap(),
                Some(*cid),
                "thread {name} missing or mis-targeted after batched emit",
            );
        }
        // Exactly the threads we asked for — none extra, none dropped.
        let listed_threads = mgr.list_threads().unwrap();
        assert_eq!(
            listed_threads.len(),
            expected_threads.len(),
            "thread count drift: {listed_threads:?}",
        );

        // Every marker present at its EXACT change-id.
        for (name, cid) in &expected_markers {
            assert_eq!(
                mgr.get_marker(&MarkerName::new(name)).unwrap(),
                Some(*cid),
                "marker {name} missing or mis-targeted after batched emit",
            );
        }
        let listed_markers = mgr.list_markers().unwrap();
        assert_eq!(
            listed_markers.len(),
            expected_markers.len(),
            "marker count drift: {listed_markers:?}",
        );

        // Idempotent: a second identical emit is a no-op for final state.
        let again = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads)).unwrap();
        assert_eq!(again.threads_written, N_BRANCHES + N_REMOTES);
        for (name, cid) in &expected_threads {
            assert_eq!(mgr.get_thread(&ThreadName::new(name)).unwrap(), Some(*cid));
        }
        for (name, cid) in &expected_markers {
            assert_eq!(mgr.get_marker(&MarkerName::new(name)).unwrap(), Some(*cid));
        }
    }

    #[test]
    fn refuses_to_move_thread_across_divergent_history() {
        let (_tmp, mgr) = fresh_ref_manager();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let base = test_state(&store, vec![]);
        let heddle_side = test_state(&store, vec![base]);
        let git_side = test_state(&store, vec![base]);
        let git_sha = "e".repeat(40);
        map.insert_commit(&git_sha, git_side).unwrap();
        mgr.set_thread(&ThreadName::new("main"), &heddle_side)
            .unwrap();
        let heads = vec![sample_head("main", RefNamespace::Branch, &git_sha)];

        let error = pollster::block_on(RefEmitter::new(&mgr, &store, &map).emit(&heads))
            .expect_err("divergent thread import should fail closed");
        match error {
            IngestError::ThreadDiverged {
                thread,
                branch,
                existing,
                incoming,
            } => {
                assert_eq!(thread, "main");
                assert_eq!(branch, "main");
                assert_eq!(existing, heddle_side);
                assert_eq!(incoming, git_side);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            mgr.get_thread(&ThreadName::new("main")).unwrap(),
            Some(heddle_side)
        );
    }
}
