// SPDX-License-Identifier: Apache-2.0
//! Repository capture, attachment, and backfill for the semantic index.
#![cfg(feature = "tree-sitter-symbols")]

use crate::{HeddleError, Repository, Result, StateAttachmentKind};
use objects::{
    object::{
        ContentHash, SemanticIndexRoot, State, StateId, SymbolAnchor,
        SymbolEntry, Tree,
    },
    store::ObjectStore,
};
use semantic::{
    index_assembly::{ParentIndex, SemanticIndexBuilder},
    semantic_index::{EXTRACTOR_VERSION, grammar_version_by_name},
};
#[cfg(test)]
use semantic::{parser::Language, semantic_index::extract_semantic_file};
use std::collections::HashMap;
use tracing::warn;
#[cfg(test)]
use objects::object::SemanticTreeNode;

type DeferredSemanticIndex = (Option<ContentHash>, Vec<(ContentHash, Vec<u8>)>);

impl Repository {
    /// Compute a state's semantic index during capture and persist all node
    /// blobs, returning the root blob hash to attach. Never fails the snapshot:
    /// any error is logged and `Ok(None)` returned.
    pub(crate) fn compute_and_persist_semantic_index(
        &self,
        prior: Option<&State>,
        new: &State,
    ) -> Result<Option<ContentHash>> {
        let tree = match self.store().get_tree(&new.tree) {
            Ok(Some(tree)) => tree,
            Ok(None) => return Ok(None),
            Err(err) => {
                warn!(error = %err, "semantic index: could not load state tree; skipping");
                return Ok(None);
            }
        };
        self.compute_and_persist_semantic_index_for_tree(prior, &tree, None, None)
    }

    pub(crate) fn compute_and_persist_semantic_index_for_tree(
        &self,
        prior: Option<&State>,
        tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
    ) -> Result<Option<ContentHash>> {
        let (root, pending) =
            self.compute_semantic_index_for_tree_deferred(prior, tree, source_blobs, source_trees)?;
        self.store().put_blobs_packed(pending)?;
        Ok(root)
    }

    /// Build and resolve the semantic closure without persisting it. Snapshot
    /// authoring includes the returned blobs in its single authoritative pack.
    pub(crate) fn compute_semantic_index_for_tree_deferred(
        &self,
        prior: Option<&State>,
        tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
    ) -> Result<DeferredSemanticIndex> {
        let parent = match prior.map(|p| self.materialize_parent_index(p)) {
            Some(Ok(Some(parent))) => Some(parent),
            Some(Ok(None)) | None => None,
            Some(Err(err)) => {
                warn!(error = %err, "semantic index: parent reuse unavailable; full build");
                None
            }
        };
        let mut builder = match (source_blobs, source_trees) {
            (Some(source_blobs), Some(source_trees)) => SemanticIndexBuilder::with_source_objects(
                self.store(),
                EXTRACTOR_VERSION,
                source_blobs,
                source_trees,
            ),
            _ => SemanticIndexBuilder::new(self.store(), EXTRACTOR_VERSION),
        };
        match builder.build_root_deferred(tree, parent.as_ref()) {
            Ok((root, _, mut pending)) => {
                let pending_by_hash = pending.iter().cloned().collect::<HashMap<_, _>>();
                match self.persist_resolved_semantic_edges_deferred(prior, root, &pending_by_hash) {
                    Ok((root_hash, resolved)) => {
                        pending.extend(resolved);
                        Ok((Some(root_hash), pending))
                    }
                    Err(err) => {
                        warn!(error = %err, "semantic graph: resolution failed; skipping index");
                        Ok((None, Vec::new()))
                    }
                }
            }
            Err(err) => {
                warn!(error = %err, "semantic index: build failed; skipping");
                Ok((None, Vec::new()))
            }
        }
    }

    /// Whether an attached root is current: extractor version and every grammar
    /// version match this binary's. A stale root is served as a MISS so a
    /// bump recomputes.
    fn index_is_current(&self, root: &SemanticIndexRoot) -> bool {
        root.binding_delta.is_some()
            && root.importer_index.is_some()
            && root.resolver_version == semantic::cross_file_resolution::RESOLVER_VERSION
            && root.extractor_version == EXTRACTOR_VERSION
            && root
                .grammars
                .iter()
                .all(|(name, version)| grammar_version_by_name(name) == Some(version.as_str()))
    }

    /// Materialize a parent state's index for reuse: its source tree + semantic
    /// top node + root. Returns `None` when the parent has no attached index
    /// (caller falls back to a full build).
    fn materialize_parent_index(&self, parent: &State) -> Result<Option<ParentIndex>> {
        let Some(root) = self.attached_semantic_index(&parent.id())? else {
            return Ok(None);
        };
        let Some(source_tree) = self.store().get_tree(&parent.tree)? else {
            return Ok(None);
        };
        let semantic_tree = self.load_semantic_tree(&root.tree)?;
        Ok(Some(ParentIndex {
            source_tree,
            semantic_tree,
            root,
        }))
    }

    /// Get-or-compute a state's semantic index, parents-first. If already
    /// attached, returns it; otherwise builds forward from the nearest ancestor
    /// that has an index (reusing it), attaches each, and returns the target's.
    pub fn semantic_index(&self, state_id: &StateId) -> Result<Option<SemanticIndexRoot>> {
        if let Some(root) = self.attached_semantic_index(state_id)?
            && self.index_is_current(&root)
        {
            return Ok(Some(root));
        }
        // Absent, corrupt, or stale-version → recompute below (superseding any
        // stale attachment).
        if self.store().get_state(state_id)?.is_none() {
            return Ok(None);
        }

        // Walk the first-parent chain until we find an ancestor with an index
        // (or run out), collecting the states we must build.
        let mut to_build = vec![*state_id];
        let mut base_state: Option<StateId> = None;
        let mut cursor = self.first_parent(state_id)?;
        while let Some(parent_id) = cursor {
            if self.attached_semantic_index(&parent_id)?.is_some() {
                base_state = Some(parent_id);
                break;
            }
            to_build.push(parent_id);
            cursor = self.first_parent(&parent_id)?;
        }

        // Build oldest-first so each step can reuse the one before it.
        let mut prior_state = base_state;
        let mut result = None;
        for build_id in to_build.into_iter().rev() {
            let root = self.compute_and_attach_index(&build_id, prior_state.as_ref())?;
            prior_state = Some(build_id);
            result = root;
        }
        Ok(result)
    }

    fn first_parent(&self, state_id: &StateId) -> Result<Option<StateId>> {
        Ok(self
            .store()
            .get_state(state_id)?
            .and_then(|s| s.parents.first().copied()))
    }

    /// Build a state's index (reusing `prior` if it has one) and attach it.
    fn compute_and_attach_index(
        &self,
        state_id: &StateId,
        prior: Option<&StateId>,
    ) -> Result<Option<SemanticIndexRoot>> {
        let Some(state) = self.store().get_state(state_id)? else {
            return Ok(None);
        };
        let prior_state = match prior {
            Some(id) => self.store().get_state(id)?,
            None => None,
        };
        let root_hash = self.compute_and_persist_semantic_index(prior_state.as_ref(), &state)?;
        let Some(root_hash) = root_hash else {
            return Ok(None);
        };
        self.attach_semantic_index(state_id, &state, root_hash)?;
        Ok(Some(self.load_index_root(&root_hash)?))
    }

    /// Build only this exact State, with no ancestor backfill or network work.
    pub fn analyze_semantic_index(
        &self,
        state_id: StateId,
        budget: semantic::parser::ParseBudget,
    ) -> Result<SemanticIndexRoot> {
        self.analyze_semantic_index_with_admission(state_id, budget, || Ok(()))
    }
    pub fn analyze_semantic_index_with_admission(
        &self,
        state_id: StateId,
        budget: semantic::parser::ParseBudget,
        before_publish: impl FnOnce() -> Result<()>,
    ) -> Result<SemanticIndexRoot> {
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| HeddleError::NotFound("analysis source State".into()))?;
        let tree = self
            .store()
            .get_tree(&state.tree)?
            .ok_or_else(|| HeddleError::NotFound("analysis source tree".into()))?;
        // Inspect lengths before decoding any source blob. A stored large object
        // must not allocate outside the RPC budget merely to decide it is opaque.
        let mut pending = vec![state.tree];
        let mut visited = std::collections::BTreeSet::new();
        let mut bytes = 0u64;
        let mut entries = 0usize;
        while let Some(hash) = pending.pop() {
            if !visited.insert(hash) {
                continue;
            }
            if budget.interrupted() {
                return Err(HeddleError::InvalidObject(
                    "semantic analysis interrupted".into(),
                ));
            }
            let directory = self
                .store()
                .get_tree(&hash)?
                .ok_or_else(|| HeddleError::NotFound("analysis source directory".into()))?;
            entries = entries.saturating_add(directory.len());
            if entries > 4096 {
                return Err(HeddleError::InvalidObject(
                    "semantic analysis source work budget exceeded".into(),
                ));
            }
            for entry in directory.entries() {
                if let Some(child) = entry.tree_hash() {
                    pending.push(child);
                }
                if let Some(blob) = entry.blob_hash() {
                    let length =
                        objects::store::ObjectSource::decoded_blob_len(self.store(), &blob)?
                            .ok_or_else(|| HeddleError::NotFound("analysis source blob".into()))?;
                    bytes = bytes.saturating_add(length);
                    if bytes > 32 * 1024 * 1024 {
                        return Err(HeddleError::InvalidObject(
                            "semantic analysis source byte budget exceeded".into(),
                        ));
                    }
                }
            }
        }
        let mut builder =
            SemanticIndexBuilder::new(self.store(), EXTRACTOR_VERSION).with_budget(budget.clone());
        let (root, root_hash) = builder.build_root(&tree, None)?;
        if budget.interrupted() {
            return Err(HeddleError::InvalidObject(
                "semantic analysis interrupted".into(),
            ));
        }
        before_publish()?;
        self.attach_semantic_index(&state_id, &state, root_hash)?;
        Ok(root)
    }

    /// Rebuild a state's index from scratch with NO parent reuse — guaranteeing
    /// a complete, self-contained node closure independent of any pruned or
    /// broken parent nodes — and supersede the prior attachment. The recovery
    /// path when a query hits a missing/corrupt semantic node.
    fn force_recompute_index(&self, state_id: &StateId) -> Result<Option<SemanticIndexRoot>> {
        let Some(state) = self.store().get_state(state_id)? else {
            return Ok(None);
        };
        let Some(tree) = self.store().get_tree(&state.tree)? else {
            return Ok(None);
        };
        let mut builder = SemanticIndexBuilder::new(self.store(), EXTRACTOR_VERSION);
        let (root, _) = builder.build_root(&tree, None)?;
        let root_hash = self.persist_resolved_semantic_edges(None, root)?;
        self.attach_semantic_index(state_id, &state, root_hash)?;
        Ok(Some(self.load_index_root(&root_hash)?))
    }

    /// Run a query; if it trips over a missing/corrupt semantic node (a pruned
    /// or partially-replicated index), force-recompute the involved states'
    /// indexes and retry ONCE, so queries self-heal instead of erroring forever.
    fn recover_on_broken<T>(
        &self,
        states: &[&StateId],
        op: impl Fn(&Self) -> Result<T>,
    ) -> Result<T> {
        match op(self) {
            Err(err) if is_broken_index_error(&err) => {
                for state in states {
                    self.force_recompute_index(state)?;
                }
                op(self)
            }
            other => other,
        }
    }

    fn attach_semantic_index(
        &self,
        state_id: &StateId,
        state: &State,
        root_hash: ContentHash,
    ) -> Result<()> {
        // Supersede any existing (stale-grammar/older-extractor) index.
        let supersedes = self
            .latest_state_attachment(state_id, StateAttachmentKind::SemanticIndex)?
            .map(|a| a.id());
        self.put_state_attachment(&objects::object::StateAttachment {
            state_id: *state_id,
            body: objects::object::StateAttachmentBody::SemanticIndex(root_hash),
            attribution: state.attribution.clone(),
            created_at: chrono::Utc::now(),
            supersedes,
        })?;
        Ok(())
    }

    /// Resolve a symbol anchor (file path + symbol address) to its entry in a
    /// state's index. Get-or-computes the index on miss.
    pub fn symbol_hash(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<SymbolEntry>> {
        self.recover_on_broken(&[state_id], |me| me.symbol_hash_inner(state_id, anchor))
    }

    fn symbol_hash_inner(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<SymbolEntry>> {
        let Some(root) = self.semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(file_node_hash) = self.resolve_file_node(&root, &anchor.file)? else {
            return Ok(None);
        };
        let file = self.load_semantic_file(&file_node_hash)?;
        Ok(file.symbol_by_address(&anchor.symbol).cloned())
    }

    /// Whether the semantic content under `path_prefix` differs between two
    /// states, compared top-down by digest with identical subtrees pruned.
    /// ZERO source re-parse — only semantic node blobs along the prefix load.
    pub fn semantic_changed(&self, a: &StateId, b: &StateId, path_prefix: &str) -> Result<bool> {
        self.recover_on_broken(&[a, b], |me| me.semantic_changed_inner(a, b, path_prefix))
    }

    fn semantic_changed_inner(&self, a: &StateId, b: &StateId, path_prefix: &str) -> Result<bool> {
        let (Some(root_a), Some(root_b)) = (self.semantic_index(a)?, self.semantic_index(b)?)
        else {
            // A missing index on either side is a difference iff the other exists.
            return Ok(self.semantic_index(a)?.is_some() != self.semantic_index(b)?.is_some());
        };
        let da = self.digest_at_path(&root_a, path_prefix)?;
        let db = self.digest_at_path(&root_b, path_prefix)?;
        Ok(da != db)
    }

    /// Lazy backfill: compute-and-attach a semantic index for every state that
    /// lacks one, oldest-first (so parents are reused), restartable across runs
    /// (progress = the last state that gained an attachment). Returns the count
    /// of states newly indexed.
    pub fn backfill_semantic_index(&self, all: bool) -> Result<usize> {
        let states = self.store().list_states()?;
        // Oldest-first: fewer parents ⇒ appears earlier. Topologically, a state
        // with no ancestors sorts first; approximate with a parents-before-child
        // ordering derived from reachability.
        let ordered = self.order_states_oldest_first(states)?;
        let mut count = 0;
        for state_id in ordered {
            let already = self
                .attached_semantic_index(&state_id)?
                .is_some_and(|root| self.index_is_current(&root));
            if already && !all {
                continue; // restartable: skip states that already have one.
            }
            if all {
                // Force a fresh recompute (supersedes any stale index).
                let prior = self.first_parent(&state_id)?;
                if self
                    .compute_and_attach_index(&state_id, prior.as_ref())?
                    .is_some()
                {
                    count += 1;
                }
            } else if self.semantic_index(&state_id)?.is_some() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Order states so a parent precedes its children (best-effort Kahn-style
    /// topological sort over the in-store parent edges).
    fn order_states_oldest_first(&self, states: Vec<StateId>) -> Result<Vec<StateId>> {
        use std::collections::HashSet;
        let present: HashSet<StateId> = states.iter().copied().collect();
        let mut visited: HashSet<StateId> = HashSet::new();
        let mut ordered = Vec::with_capacity(states.len());
        // Iterative post-order DFS emits ancestors before descendants.
        for root in &states {
            let mut stack = vec![(*root, false)];
            while let Some((id, processed)) = stack.pop() {
                if processed {
                    if visited.insert(id) {
                        ordered.push(id);
                    }
                    continue;
                }
                if visited.contains(&id) {
                    continue;
                }
                stack.push((id, true));
                if let Some(state) = self.store().get_state(&id)? {
                    for parent in &state.parents {
                        if present.contains(parent) && !visited.contains(parent) {
                            stack.push((*parent, false));
                        }
                    }
                }
            }
        }
        Ok(ordered)
    }
}

/// A missing (`NotFound`) or corrupt (`InvalidObject`) semantic node — the
/// recoverable "broken index" class.
fn is_broken_index_error(err: &HeddleError) -> bool {
    matches!(
        err,
        HeddleError::NotFound(_) | HeddleError::InvalidObject(_)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use objects::object::{
        Attribution, Blob, Principal, StateAttachment, StateAttachmentBody, SymbolKindTag,
        TreeEntry,
    };
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn bounded_analysis_rejects_source_work_before_loading_or_publishing() {
        let (_temp, repository) = repo();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let budget = semantic::parser::ParseBudget {
            cancelled: cancelled.clone(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
        };
        let mut tree = Tree::new();
        for index in 0..4097 {
            tree.insert(
                TreeEntry::file(
                    format!("{index}.rs"),
                    ContentHash::from_bytes([19; 32]),
                    false,
                )
                .expect("entry"),
            );
        }
        let error = SemanticIndexBuilder::new(repository.store(), EXTRACTOR_VERSION)
            .with_budget(budget.clone())
            .build_root(&tree, None)
            .expect_err("entry bound");
        assert!(
            error.to_string().contains("source work budget exceeded"),
            "must reject at the work bound: {error}"
        );
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let state = repository.head().expect("head").expect("initial source");
        let before = repository
            .latest_state_attachment(&state, StateAttachmentKind::SemanticIndex)
            .expect("prior")
            .map(|a| a.id());
        let error = repository
            .analyze_semantic_index(state, budget)
            .expect_err("cancelled before execution");
        assert!(error.to_string().contains("interrupted"));
        assert_eq!(
            before,
            repository
                .latest_state_attachment(&state, StateAttachmentKind::SemanticIndex)
                .expect("after")
                .map(|a| a.id()),
            "cancelled analysis cannot publish an attachment"
        );
    }

    /// Attach `root_hash` as the (superseding) SemanticIndex on `state_id`.
    fn attach(repo: &Repository, state_id: &StateId, root_hash: ContentHash) {
        let state = repo.store().get_state(state_id).unwrap().unwrap();
        let prior = repo
            .latest_state_attachment(state_id, StateAttachmentKind::SemanticIndex)
            .unwrap()
            .map(|a| a.id());
        repo.put_state_attachment(&StateAttachment {
            state_id: *state_id,
            body: StateAttachmentBody::SemanticIndex(root_hash),
            attribution: state.attribution.clone(),
            created_at: Utc::now(),
            supersedes: prior,
        })
        .unwrap();
    }

    fn state_tree(repo: &Repository, state_id: &StateId) -> Tree {
        let state = repo.store().get_state(state_id).unwrap().unwrap();
        repo.store().get_tree(&state.tree).unwrap().unwrap()
    }

    fn repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn author() -> Attribution {
        Attribution::human(Principal::new("Test", "test@example.com"))
    }

    fn snapshot(repo: &Repository, temp: &TempDir, path: &str, content: &str) -> StateId {
        std::fs::write(temp.path().join(path), content).unwrap();
        repo.snapshot_with_attribution(Some("capture".to_string()), None, author())
            .unwrap()
            .id()
    }

    fn put_blob(repo: &Repository, content: &[u8]) -> ContentHash {
        repo.store().put_blob(&Blob::new(content.to_vec())).unwrap()
    }

    fn parent_index(repo: &Repository, source_tree: Tree, root: SemanticIndexRoot) -> ParentIndex {
        let blob = repo.store().get_blob(&root.tree).unwrap().unwrap();
        let semantic_tree = SemanticTreeNode::decode(blob.content()).unwrap();
        ParentIndex {
            source_tree,
            semantic_tree,
            root,
        }
    }

    /// GOLDEN: reformatting a file changes the storage hash of the semantic
    /// tree node (spans moved) but leaves the whole-tree `semantic_digest`
    /// STABLE — the two-hash crux, end to end through capture.
    #[test]
    fn reformat_changes_storage_hash_but_not_semantic_digest() {
        let (temp, repo) = repo();
        let a = snapshot(&repo, &temp, "hello.rs", "fn foo() -> i32 { 1 }\n");
        let b = snapshot(&repo, &temp, "hello.rs", "fn foo() -> i32 {\n    1\n}\n");

        let ra = repo.semantic_index(&a).unwrap().unwrap();
        let rb = repo.semantic_index(&b).unwrap().unwrap();

        assert_ne!(ra.tree, rb.tree, "reformat must move the storage hash");
        assert_eq!(
            ra.semantic_digest, rb.semantic_digest,
            "reformat must NOT change the semantic_digest"
        );
        assert!(
            !repo.semantic_changed(&a, &b, "").unwrap(),
            "semantic_changed must prune a pure reformat"
        );
    }

    /// heddle#1068: a `.zig` blob must index to a real File node (not the
    /// `Opaque` fallback), and reformatting it must leave the whole-tree
    /// `semantic_digest` stable — the ghostty git-lane import guarantee.
    #[test]
    fn zig_file_indexes_to_real_node_and_reformat_is_digest_stable() {
        let (temp, repo) = repo();
        let tight =
            "pub fn add(a: i32, b: i32) i32 { return a + b; }\ntest \"add\" { _ = add(1, 2); }\n";
        let loose = "pub fn add(a: i32,   b: i32) i32 {\n    // sum\n    return a + b;\n}\n\ntest \"add\" {\n    _ = add(1, 2);\n}\n";

        let a = snapshot(&repo, &temp, "math.zig", tight);
        let ra = repo.semantic_index(&a).unwrap().unwrap();

        // Real File node — not Opaque (which resolves to None here).
        let node_hash = repo
            .resolve_file_node(&ra, "math.zig")
            .unwrap()
            .expect("zig file must index to a real File node, not Opaque");
        let file = repo.load_semantic_file(&node_hash).unwrap();
        assert_eq!(file.language, "zig");
        assert!(
            file.symbols.iter().any(|s| s.name == "add"),
            "fn add must be a symbol: {:?}",
            file.symbols
        );
        assert!(
            file.symbols.iter().any(|s| s.name == "test:\"add\""),
            "test block must be a symbol: {:?}",
            file.symbols
        );

        let b = snapshot(&repo, &temp, "math.zig", loose);
        let rb = repo.semantic_index(&b).unwrap().unwrap();
        assert_ne!(ra.tree, rb.tree, "reformat must move the storage hash");
        assert_eq!(
            ra.semantic_digest, rb.semantic_digest,
            "reformatting a Zig file must NOT change the semantic_digest"
        );
        assert!(
            !repo.semantic_changed(&a, &b, "").unwrap(),
            "semantic_changed must prune a pure Zig reformat"
        );
    }

    /// GOLDEN: a one-token change to a single function yields exactly one
    /// SymbolDelta, for that symbol, with both old and new hashes present.
    #[test]
    fn one_token_change_yields_exactly_one_delta() {
        let (temp, repo) = repo();
        let a = snapshot(
            &repo,
            &temp,
            "m.rs",
            "fn foo() -> i32 { 1 }\nfn bar() -> i32 { 2 }\n",
        );
        let b = snapshot(
            &repo,
            &temp,
            "m.rs",
            "fn foo() -> i32 { 1 }\nfn bar() -> i32 { 3 }\n",
        );

        let deltas = repo.semantic_diff_symbols(&a, &b).unwrap();
        assert_eq!(deltas.len(), 1, "exactly one symbol changed: {deltas:?}");
        assert_eq!(deltas[0].anchor.symbol, "bar");
        assert_eq!(deltas[0].anchor.file, "m.rs");
        assert!(deltas[0].old_hash.is_some());
        assert!(deltas[0].new_hash.is_some());
        assert_ne!(deltas[0].old_hash, deltas[0].new_hash);
    }

    /// GOLDEN: `symbol_hash` resolves an anchor, and `changed_since` reports
    /// per-symbol change correctly (untouched symbol stable, edited one not).
    #[test]
    fn symbol_hash_and_changed_since() {
        let (temp, repo) = repo();
        let a = snapshot(
            &repo,
            &temp,
            "m.rs",
            "fn foo() -> i32 { 1 }\nfn bar() -> i32 { 2 }\n",
        );
        let b = snapshot(
            &repo,
            &temp,
            "m.rs",
            "fn foo() -> i32 { 1 }\nfn bar() -> i32 { 3 }\n",
        );

        let foo = SymbolAnchor::new("m.rs", "foo");
        let bar = SymbolAnchor::new("m.rs", "bar");
        assert!(repo.symbol_hash(&a, &foo).unwrap().is_some());
        assert!(!repo.changed_since(&foo, &a, &b).unwrap(), "foo untouched");
        assert!(repo.changed_since(&bar, &a, &b).unwrap(), "bar edited");
    }

    /// GOLDEN: incremental assembly parses only the changed blob — the
    /// unchanged sibling is reused from the parent index WITHOUT a re-parse.
    #[test]
    fn incremental_build_prunes_unchanged_without_reparse() {
        let (_temp, repo) = repo();
        let blob_a = put_blob(&repo, b"fn a() -> i32 { 1 }\n");
        let blob_b = put_blob(&repo, b"fn b() -> i32 { 2 }\n");
        let tree_a = Tree::from_entries(vec![
            TreeEntry::file("a.rs", blob_a, false).unwrap(),
            TreeEntry::file("b.rs", blob_b, false).unwrap(),
        ]);

        let mut parent_builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        let (root_a, _) = parent_builder.build_root(&tree_a, None).unwrap();
        assert_eq!(
            parent_builder.parse_count, 2,
            "cold build parses both files"
        );

        // Only b.rs changes.
        let blob_b2 = put_blob(&repo, b"fn b() -> i32 { 99 }\n");
        let tree_b = Tree::from_entries(vec![
            TreeEntry::file("a.rs", blob_a, false).unwrap(),
            TreeEntry::file("b.rs", blob_b2, false).unwrap(),
        ]);

        let parent = parent_index(&repo, tree_a, root_a);
        let mut child_builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        child_builder.build_root(&tree_b, Some(&parent)).unwrap();
        assert_eq!(
            child_builder.parse_count, 1,
            "only the changed file is reparsed"
        );
    }

    /// GOLDEN: a source blob appearing at several paths is parsed exactly once
    /// (backfill/build memoizes by source blob hash).
    #[test]
    fn shared_blob_parsed_once() {
        let (_temp, repo) = repo();
        let blob = put_blob(&repo, b"use crate::api::greet;\nfn shared() { greet(); }\n");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("x.rs", blob, false).unwrap(),
            TreeEntry::file("y.rs", blob, false).unwrap(),
        ]);
        let mut builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        let (root, _) = builder.build_root(&tree, None).unwrap();
        assert_eq!(builder.parse_count, 1, "shared blob parsed once");
        let x = repo.resolve_file_node(&root, "x.rs").unwrap().unwrap();
        let y = repo.resolve_file_node(&root, "y.rs").unwrap().unwrap();
        assert_eq!(x, y, "identical source blobs must share one file node");
    }

    #[test]
    fn extracted_facts_roundtrip_through_persisted_file_node() {
        let (_temp, repo) = repo();
        let source = b"use crate::api::greet;\nfn run() { greet(); }\n";
        let blob = put_blob(&repo, source);
        let tree = Tree::from_entries(vec![TreeEntry::file("main.rs", blob, false).unwrap()]);
        let expected = extract_semantic_file(source, Language::Rust).unwrap();

        let mut builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        let (root, _) = builder.build_root(&tree, None).unwrap();
        let node_hash = repo.resolve_file_node(&root, "main.rs").unwrap().unwrap();
        let loaded = repo.load_semantic_file(&node_hash).unwrap();

        assert_eq!(loaded.symbols, expected.symbols);
        assert_eq!(loaded.scopes, expected.scopes);
        assert_eq!(loaded.imports, expected.imports);
        assert_eq!(loaded.occurrences, expected.occurrences);
        assert_eq!(loaded.source_blob, blob);
    }

    /// The lazy backfill indexes every state once and is a no-op the second
    /// time (restartable / idempotent).
    #[test]
    fn backfill_is_idempotent() {
        let (temp, repo) = repo();
        snapshot(&repo, &temp, "a.rs", "fn a() {}\n");
        snapshot(&repo, &temp, "b.rs", "fn b() {}\n");

        // Captured states are already indexed eagerly; only pre-capture states
        // (e.g. the init root) remain. A first backfill picks those up, and a
        // second is a no-op — the restartable/idempotent property.
        repo.backfill_semantic_index(false).unwrap();
        assert_eq!(
            repo.backfill_semantic_index(false).unwrap(),
            0,
            "backfill must be idempotent"
        );
        // --all recomputes every state.
        let total = repo.store().list_states().unwrap().len();
        assert_eq!(repo.backfill_semantic_index(true).unwrap(), total);
    }

    /// DEFECT 2: an extractor bump must refuse stale node reuse — an unchanged
    /// blob is re-parsed into a fresh node, so no index mixes v1+v2 fingerprints.
    #[test]
    fn extractor_bump_refuses_stale_reuse() {
        let (_temp, repo) = repo();
        let blob = put_blob(&repo, b"fn a() -> i32 { 1 }\n");
        let tree = Tree::from_entries(vec![TreeEntry::file("a.rs", blob, false).unwrap()]);

        assert_eq!(EXTRACTOR_VERSION, 5, "schema addition must bump extraction");
        let old_version = EXTRACTOR_VERSION - 1;
        let mut b1 = SemanticIndexBuilder::new(repo.store(), old_version);
        let (root1, _) = b1.build_root(&tree, None).unwrap();
        assert_eq!(root1.extractor_version, old_version);

        // Same source, current extractor offered a stale-version parent.
        let parent = parent_index(&repo, tree.clone(), root1);
        let mut b2 = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        let (root2, _) = b2.build_root(&tree, Some(&parent)).unwrap();
        assert_eq!(
            b2.parse_count, 1,
            "extractor bump must reparse, not reuse the v1 node"
        );
        assert_eq!(root2.extractor_version, EXTRACTOR_VERSION);
        let node_hash = repo.resolve_file_node(&root2, "a.rs").unwrap().unwrap();
        assert_eq!(
            repo.load_semantic_file(&node_hash)
                .unwrap()
                .extractor_version,
            EXTRACTOR_VERSION,
            "no mixed-version index"
        );
    }

    /// DEFECT 2: `semantic_index()` must treat a stale-version attached root as
    /// a MISS and recompute+supersede to the current extractor version.
    #[test]
    fn stale_attached_index_is_recomputed() {
        let (temp, repo) = repo();
        let a = snapshot(&repo, &temp, "m.rs", "fn f() {}\n");

        // Force a stale (extractor 999) index onto the state.
        let mut builder = SemanticIndexBuilder::new(repo.store(), 999);
        let (stale_root, stale_hash) = builder.build_root(&state_tree(&repo, &a), None).unwrap();
        assert_eq!(stale_root.extractor_version, 999);
        attach(&repo, &a, stale_hash);

        let root = repo.semantic_index(&a).unwrap().unwrap();
        assert_eq!(
            root.extractor_version, EXTRACTOR_VERSION,
            "stale attached root must be recomputed to the current version"
        );
    }

    /// DEFECT 3: a dangling attached index (root present but pointing at a
    /// missing node — e.g. a partially-replicated push or a pruned sidecar)
    /// must self-heal: queries recompute+supersede instead of erroring forever.
    #[test]
    fn dangling_index_recovers_on_query() {
        let (temp, repo) = repo();
        let a = snapshot(&repo, &temp, "m.rs", "fn f() -> i32 { 1 }\n");

        // Attach a current-version root pointing at a node that does not exist.
        let fake_tree = ContentHash::compute(b"missing-semantic-node");
        let bogus = SemanticIndexRoot::new(
            EXTRACTOR_VERSION,
            BTreeMap::new(),
            fake_tree,
            ContentHash::compute(b"digest"),
        );
        let bogus_hash = repo
            .store()
            .put_blob(&Blob::new(bogus.encode().unwrap()))
            .unwrap();
        attach(&repo, &a, bogus_hash);

        // The query descends the dangling tree, hits the missing node, and must
        // recover — not propagate a NotFound.
        let anchor = SymbolAnchor::new("m.rs", "f");
        let sym = repo
            .symbol_hash(&a, &anchor)
            .expect("query must recover, not error");
        assert!(sym.is_some(), "recomputed index resolves the symbol");

        // And the state now carries a valid, resolvable index.
        let root = repo.semantic_index(&a).unwrap().unwrap();
        assert!(repo.resolve_file_node(&root, "m.rs").unwrap().is_some());
    }

    /// DEFECT 5: byte-identical blobs at `.js` and `.py` must get DISTINCT nodes
    /// with the correct language — a node is a pure function of (bytes, ext,
    /// grammar, extractor), not bytes alone.
    #[test]
    fn same_bytes_distinct_language_nodes() {
        let (_temp, repo) = repo();
        // `x = 1` parses (error-free) as both JavaScript and Python.
        let blob = put_blob(&repo, b"x = 1\n");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("a.js", blob, false).unwrap(),
            TreeEntry::file("b.py", blob, false).unwrap(),
        ]);
        let mut builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        let (root, _) = builder.build_root(&tree, None).unwrap();
        assert_eq!(builder.parse_count, 2, "parsed once per (bytes, language)");

        let js = repo.resolve_file_node(&root, "a.js").unwrap().unwrap();
        let py = repo.resolve_file_node(&root, "b.py").unwrap().unwrap();
        assert_ne!(
            js, py,
            "byte-identical files get distinct nodes per language"
        );
        assert_eq!(repo.load_semantic_file(&js).unwrap().language, "javascript");
        assert_eq!(repo.load_semantic_file(&py).unwrap().language, "python");
    }

    /// DEFECT 6: same-name symbols of different KIND must not collide — editing
    /// `fn X` must report exactly `fn X`, leaving `struct X` untouched.
    #[test]
    fn diff_decollides_same_name_different_kind() {
        let (temp, repo) = repo();
        let a = snapshot(
            &repo,
            &temp,
            "m.rs",
            "struct X { a: u8 }\nfn X() -> i32 { 1 }\n",
        );
        let b = snapshot(
            &repo,
            &temp,
            "m.rs",
            "struct X { a: u8 }\nfn X() -> i32 { 2 }\n",
        );
        let deltas = repo.semantic_diff_symbols(&a, &b).unwrap();
        assert_eq!(deltas.len(), 1, "only fn X changed: {deltas:?}");
        assert_eq!(deltas[0].anchor.symbol, "X");
        assert_eq!(deltas[0].kind, SymbolKindTag::Function);
    }

    /// DEFECT 6: same-name symbols in different modules must not collide —
    /// editing `mod a::f` must report `a::f` and NEVER `b::f`.
    #[test]
    fn diff_decollides_same_name_across_modules() {
        let (temp, repo) = repo();
        let a = snapshot(
            &repo,
            &temp,
            "m.rs",
            "mod a { pub fn f() -> i32 { 1 } }\nmod b { pub fn f() -> i32 { 9 } }\n",
        );
        let b = snapshot(
            &repo,
            &temp,
            "m.rs",
            "mod a { pub fn f() -> i32 { 2 } }\nmod b { pub fn f() -> i32 { 9 } }\n",
        );
        let deltas = repo.semantic_diff_symbols(&a, &b).unwrap();
        let symbols: Vec<_> = deltas.iter().map(|d| d.anchor.symbol.clone()).collect();
        assert!(
            symbols.contains(&"a::f".to_string()),
            "a::f must be reported: {symbols:?}"
        );
        assert!(
            !symbols.contains(&"b::f".to_string()),
            "b::f must NOT be reported (no cross-module address collision): {symbols:?}"
        );
    }
}
