// SPDX-License-Identifier: Apache-2.0
//! Assembly, capture wiring, backfill and query primitives for the merkle
//! semantic index (heddle#1067).
//!
//! The node types and canonical digest layouts live in `objects`; the AST
//! extraction lives in `semantic`. This layer mirrors the source blob/tree DAG
//! into a parallel semantic DAG stored as ordinary content-addressed blobs, and
//! attaches its root to a state via `StateAttachmentBody::SemanticIndex`.
//!
//! ## Cheap-to-maintain
//!
//! [`SemanticIndexBuilder`] parses only *changed* source blobs: it reuses a
//! parent index's node wholesale wherever the source subtree hash is unchanged
//! (assembly is O(changed-path)), and memoizes per source-blob so a blob shared
//! across paths — or across the reformat of a sibling — is parsed at most once.
//!
//! ## Never-fail capture
//!
//! Capture swallows-with-warn exactly like `compute_and_persist_signals`; a
//! parser hiccup must never fail a snapshot.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, HashMap};

use objects::{
    object::{
        Blob, ContentHash, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot,
        SemanticTreeEntry, SemanticTreeNode, State, StateId, SymbolAnchor, SymbolEntry,
        SymbolKindTag, Tree, TreeEntryTarget,
    },
    store::ObjectStore,
};
use semantic::{
    parser::Language,
    semantic_index::{EXTRACTOR_VERSION, extract_semantic_file, grammar_version, language_name},
};
use tracing::warn;

use crate::{Repository, Result, StateAttachmentKind};

/// Source files above this size are recorded as `Opaque` rather than parsed —
/// generated/vendored blobs dominate parse cost and rarely carry review-worthy
/// symbols.
const SEMANTIC_FILE_BUDGET_BYTES: usize = 1 << 20;

/// A single-symbol delta between two states, produced by [`Repository::semantic_diff_symbols`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDelta {
    /// File path + canonical symbol address (`container::name`).
    pub anchor: SymbolAnchor,
    pub kind: SymbolKindTag,
    /// Fingerprint on the `a` side, `None` if the symbol did not exist there.
    pub old_hash: Option<ContentHash>,
    /// Fingerprint on the `b` side, `None` if the symbol does not exist there.
    pub new_hash: Option<ContentHash>,
}

/// What a built subtree resolved to: the storage hash of the node blob (or the
/// raw source blob, for opaque entries) plus its reformat-stable digest.
#[derive(Clone, Copy)]
struct BuiltEntry {
    kind: SemanticEntryKind,
    node: ContentHash,
    semantic_digest: ContentHash,
}

/// Builds a semantic index over a source tree, reusing a parent index where the
/// source is unchanged and memoizing per source-blob so each unique blob is
/// parsed at most once. `parse_count` is exposed for tests that assert the
/// prune-without-reparse invariant.
pub struct SemanticIndexBuilder<'store, S: ObjectStore> {
    store: &'store S,
    extractor_version: u32,
    /// Per-build memo: source blob hash → its file/opaque entry. A blob that
    /// appears at several paths (or is the unchanged sibling of a reformat) is
    /// parsed once.
    file_memo: HashMap<ContentHash, BuiltEntry>,
    /// Languages encountered while building, seeded from the parent root so
    /// pruned subtrees' grammars are not lost.
    grammars: BTreeMap<String, String>,
    /// Number of source blobs actually parsed this build.
    pub parse_count: usize,
}

impl<'store, S: ObjectStore> SemanticIndexBuilder<'store, S> {
    pub fn new(store: &'store S, extractor_version: u32) -> Self {
        Self {
            store,
            extractor_version,
            file_memo: HashMap::new(),
            grammars: BTreeMap::new(),
            parse_count: 0,
        }
    }

    /// Build the index for `tree`, optionally reusing `parent` (its source tree
    /// + semantic index root) for unchanged-subtree pruning. Returns the
    /// persisted [`SemanticIndexRoot`] and its storage hash.
    pub fn build_root(
        &mut self,
        tree: &Tree,
        parent: Option<&ParentIndex>,
    ) -> Result<(SemanticIndexRoot, ContentHash)> {
        if let Some(parent) = parent {
            self.grammars = parent.root.grammars.clone();
        }
        let parent_ctx = parent.map(|p| (&p.source_tree, &p.semantic_tree));
        let (node_hash, digest) = self.build_tree(tree, parent_ctx)?;
        let root = SemanticIndexRoot::new(
            self.extractor_version,
            std::mem::take(&mut self.grammars),
            node_hash,
            digest,
        );
        let root_hash = self.put_node(&root.encode()?)?;
        Ok((root, root_hash))
    }

    fn build_tree(
        &mut self,
        tree: &Tree,
        parent: Option<(&Tree, &SemanticTreeNode)>,
    ) -> Result<(ContentHash, ContentHash)> {
        let mut entries = Vec::with_capacity(tree.len());
        for entry in tree.entries() {
            let name = entry.name();
            let built = match entry.target() {
                TreeEntryTarget::Tree { hash } => {
                    self.build_dir(name, *hash, parent)?
                }
                TreeEntryTarget::Blob { hash, .. } => self.build_file(name, *hash, parent)?,
                TreeEntryTarget::Symlink { hash } => BuiltEntry {
                    kind: SemanticEntryKind::Opaque,
                    node: *hash,
                    semantic_digest: *hash,
                },
                // Git submodule / native child-spool edges have no source blob
                // in this store; fingerprint them by their stable target bytes.
                TreeEntryTarget::Gitlink { .. } | TreeEntryTarget::Spoollink { .. } => {
                    let digest = opaque_edge_digest(entry.target());
                    BuiltEntry {
                        kind: SemanticEntryKind::Opaque,
                        node: digest,
                        semantic_digest: digest,
                    }
                }
            };
            entries.push(SemanticTreeEntry {
                name: name.to_string(),
                kind: built.kind,
                node: built.node,
                semantic_digest: built.semantic_digest,
            });
        }
        let (node, digest) = SemanticTreeNode::new(entries);
        let node_hash = self.put_node(&node.encode()?)?;
        Ok((node_hash, digest))
    }

    fn build_dir(
        &mut self,
        name: &str,
        source_hash: ContentHash,
        parent: Option<(&Tree, &SemanticTreeNode)>,
    ) -> Result<BuiltEntry> {
        // Unchanged-subtree prune: same-named source dir with the same hash and
        // a matching parent semantic entry ⇒ reuse wholesale, no recurse, no
        // parse.
        if let Some((parent_source, parent_sem)) = parent
            && let Some(parent_entry) = parent_source.get(name)
            && parent_entry.tree_hash() == Some(source_hash)
            && let Some(sem_entry) = parent_sem.get(name)
            && sem_entry.kind == SemanticEntryKind::Dir
        {
            return Ok(BuiltEntry {
                kind: SemanticEntryKind::Dir,
                node: sem_entry.node,
                semantic_digest: sem_entry.semantic_digest,
            });
        }

        let source_tree = self
            .store
            .get_tree(&source_hash)?
            .ok_or_else(|| crate::HeddleError::NotFound(format!("tree {source_hash}")))?;

        // Descend with the matching parent subtree as the reuse basis, if any.
        let child_parent = self.child_parent_ctx(name, parent)?;
        let child_parent_ref = child_parent
            .as_ref()
            .map(|(t, n)| (t, n));
        let (node, digest) = self.build_tree(&source_tree, child_parent_ref)?;
        Ok(BuiltEntry {
            kind: SemanticEntryKind::Dir,
            node,
            semantic_digest: digest,
        })
    }

    /// Load the parent source subtree + parent semantic subtree for `name`, to
    /// serve as the reuse basis when recursing into a changed directory.
    fn child_parent_ctx(
        &self,
        name: &str,
        parent: Option<(&Tree, &SemanticTreeNode)>,
    ) -> Result<Option<(Tree, SemanticTreeNode)>> {
        let Some((parent_source, parent_sem)) = parent else {
            return Ok(None);
        };
        let Some(source_entry) = parent_source.get(name) else {
            return Ok(None);
        };
        let Some(source_hash) = source_entry.tree_hash() else {
            return Ok(None);
        };
        let Some(sem_entry) = parent_sem.get(name) else {
            return Ok(None);
        };
        if sem_entry.kind != SemanticEntryKind::Dir {
            return Ok(None);
        }
        let Some(source_tree) = self.store.get_tree(&source_hash)? else {
            return Ok(None);
        };
        let Some(blob) = self.store.get_blob(&sem_entry.node)? else {
            return Ok(None);
        };
        match SemanticTreeNode::decode(blob.content()) {
            Ok(sem_tree) => Ok(Some((source_tree, sem_tree))),
            Err(_) => Ok(None),
        }
    }

    fn build_file(
        &mut self,
        name: &str,
        source_hash: ContentHash,
        parent: Option<(&Tree, &SemanticTreeNode)>,
    ) -> Result<BuiltEntry> {
        // Parsed once per unique source blob.
        if let Some(built) = self.file_memo.get(&source_hash) {
            return Ok(*built);
        }

        // Unchanged-file reuse: same-named source blob with the same hash and a
        // matching parent semantic entry ⇒ reuse, no parse.
        if let Some((parent_source, parent_sem)) = parent
            && let Some(parent_entry) = parent_source.get(name)
            && parent_entry.blob_hash() == Some(source_hash)
            && let Some(sem_entry) = parent_sem.get(name)
        {
            let built = BuiltEntry {
                kind: sem_entry.kind,
                node: sem_entry.node,
                semantic_digest: sem_entry.semantic_digest,
            };
            self.file_memo.insert(source_hash, built);
            return Ok(built);
        }

        let built = self.parse_file(name, source_hash)?;
        self.file_memo.insert(source_hash, built);
        Ok(built)
    }

    fn parse_file(&mut self, name: &str, source_hash: ContentHash) -> Result<BuiltEntry> {
        let opaque = BuiltEntry {
            kind: SemanticEntryKind::Opaque,
            node: source_hash,
            semantic_digest: source_hash,
        };

        let language = Language::from_path(std::path::Path::new(name));
        if language.parser_handle().is_none() {
            return Ok(opaque);
        }
        let Some(blob) = self.store.get_blob(&source_hash)? else {
            return Ok(opaque);
        };
        if blob.size() > SEMANTIC_FILE_BUDGET_BYTES {
            return Ok(opaque);
        }
        let Some(extracted) = extract_semantic_file(blob.content(), language) else {
            // Unsupported/parse-fail → opaque.
            return Ok(opaque);
        };
        self.parse_count += 1;

        let lang = language_name(extracted.language).to_string();
        let gv = grammar_version(extracted.language).to_string();
        self.grammars.entry(lang.clone()).or_insert_with(|| gv.clone());

        let node = SemanticFileNode::new(
            lang,
            gv,
            self.extractor_version,
            source_hash,
            extracted.symbols,
        );
        let digest = node.semantic_digest;
        let node_hash = self.put_node(&node.encode()?)?;
        Ok(BuiltEntry {
            kind: SemanticEntryKind::File,
            node: node_hash,
            semantic_digest: digest,
        })
    }

    fn put_node(&self, bytes: &[u8]) -> Result<ContentHash> {
        self.store.put_blob(&Blob::new(bytes.to_vec()))
    }
}

/// Digest for a git submodule / spool edge — hashed over its stable target
/// bytes so a submodule pointer bump perturbs the digest chain.
fn opaque_edge_digest(target: &TreeEntryTarget) -> ContentHash {
    match target {
        TreeEntryTarget::Gitlink { target } => {
            ContentHash::compute_typed("hd-sem-opaque-gitlink", target.as_bytes())
        }
        TreeEntryTarget::Spoollink { spool_id, state_id } => {
            let mut buf = Vec::new();
            buf.extend_from_slice(spool_id.as_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(state_id.as_bytes());
            ContentHash::compute_typed("hd-sem-opaque-spoollink", &buf)
        }
        // Only edge targets reach here.
        _ => ContentHash::compute_typed("hd-sem-opaque", &[]),
    }
}

/// A parent state's index, materialized for reuse during an incremental build.
pub struct ParentIndex {
    pub source_tree: Tree,
    pub semantic_tree: SemanticTreeNode,
    pub root: SemanticIndexRoot,
}

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
        let parent = match prior.map(|p| self.materialize_parent_index(p)) {
            Some(Ok(Some(parent))) => Some(parent),
            Some(Ok(None)) | None => None,
            Some(Err(err)) => {
                warn!(error = %err, "semantic index: parent reuse unavailable; full build");
                None
            }
        };
        let mut builder = SemanticIndexBuilder::new(self.store(), EXTRACTOR_VERSION);
        match builder.build_root(&tree, parent.as_ref()) {
            Ok((_, root_hash)) => Ok(Some(root_hash)),
            Err(err) => {
                warn!(error = %err, "semantic index: build failed; skipping");
                Ok(None)
            }
        }
    }

    /// Load a state's attached semantic index root, if present.
    fn load_attached_index(&self, state_id: &StateId) -> Result<Option<SemanticIndexRoot>> {
        let Some(attachment) =
            self.latest_state_attachment(state_id, StateAttachmentKind::SemanticIndex)?
        else {
            return Ok(None);
        };
        let objects::object::StateAttachmentBody::SemanticIndex(root_hash) = attachment.body else {
            return Ok(None);
        };
        self.load_index_root(&root_hash).map(Some)
    }

    fn load_index_root(&self, root_hash: &ContentHash) -> Result<SemanticIndexRoot> {
        let blob = self
            .store()
            .get_blob(root_hash)?
            .ok_or_else(|| crate::HeddleError::NotFound(format!("semantic index root {root_hash}")))?;
        SemanticIndexRoot::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    fn load_semantic_tree(&self, node_hash: &ContentHash) -> Result<SemanticTreeNode> {
        let blob = self
            .store()
            .get_blob(node_hash)?
            .ok_or_else(|| crate::HeddleError::NotFound(format!("semantic tree node {node_hash}")))?;
        SemanticTreeNode::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    fn load_semantic_file(&self, node_hash: &ContentHash) -> Result<SemanticFileNode> {
        let blob = self
            .store()
            .get_blob(node_hash)?
            .ok_or_else(|| crate::HeddleError::NotFound(format!("semantic file node {node_hash}")))?;
        SemanticFileNode::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    /// Materialize a parent state's index for reuse: its source tree + semantic
    /// top node + root. Returns `None` when the parent has no attached index
    /// (caller falls back to a full build).
    fn materialize_parent_index(&self, parent: &State) -> Result<Option<ParentIndex>> {
        let Some(root) = self.load_attached_index(&parent.id())? else {
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
        if let Some(root) = self.load_attached_index(state_id)? {
            return Ok(Some(root));
        }
        if self.store().get_state(state_id)?.is_none() {
            return Ok(None);
        }

        // Walk the first-parent chain until we find an ancestor with an index
        // (or run out), collecting the states we must build.
        let mut to_build = vec![*state_id];
        let mut base_state: Option<StateId> = None;
        let mut cursor = self.first_parent(state_id)?;
        while let Some(parent_id) = cursor {
            if self.load_attached_index(&parent_id)?.is_some() {
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
        let Some(root) = self.semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(file_node_hash) = self.resolve_file_node(&root, &anchor.file)? else {
            return Ok(None);
        };
        let file = self.load_semantic_file(&file_node_hash)?;
        Ok(file.symbol_by_address(&anchor.symbol).cloned())
    }

    /// Walk the semantic tree to the `File` node for `path`, returning its
    /// storage hash. `None` if the path is absent or resolves to a dir/opaque.
    fn resolve_file_node(
        &self,
        root: &SemanticIndexRoot,
        path: &str,
    ) -> Result<Option<ContentHash>> {
        let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            return Ok(None);
        }
        let mut node = self.load_semantic_tree(&root.tree)?;
        for (i, comp) in components.iter().enumerate() {
            let Some(entry) = node.get(comp) else {
                return Ok(None);
            };
            let last = i + 1 == components.len();
            match (last, entry.kind) {
                (true, SemanticEntryKind::File) => return Ok(Some(entry.node)),
                (false, SemanticEntryKind::Dir) => {
                    node = self.load_semantic_tree(&entry.node)?;
                }
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Whether the semantic content under `path_prefix` differs between two
    /// states, compared top-down by digest with identical subtrees pruned.
    /// ZERO source re-parse — only semantic node blobs along the prefix load.
    pub fn semantic_changed(
        &self,
        a: &StateId,
        b: &StateId,
        path_prefix: &str,
    ) -> Result<bool> {
        let (Some(root_a), Some(root_b)) = (self.semantic_index(a)?, self.semantic_index(b)?) else {
            // A missing index on either side is a difference iff the other exists.
            return Ok(self.semantic_index(a)?.is_some() != self.semantic_index(b)?.is_some());
        };
        let da = self.digest_at_path(&root_a, path_prefix)?;
        let db = self.digest_at_path(&root_b, path_prefix)?;
        Ok(da != db)
    }

    /// The reformat-stable digest at `path_prefix` within an index. Empty prefix
    /// yields the whole-tree digest. Loads only the tree nodes along the path.
    fn digest_at_path(
        &self,
        root: &SemanticIndexRoot,
        path_prefix: &str,
    ) -> Result<Option<ContentHash>> {
        let components: Vec<&str> = path_prefix.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            return Ok(Some(root.semantic_digest));
        }
        let mut node = self.load_semantic_tree(&root.tree)?;
        for (i, comp) in components.iter().enumerate() {
            let Some(entry) = node.get(comp) else {
                return Ok(None);
            };
            let last = i + 1 == components.len();
            if last {
                return Ok(Some(entry.semantic_digest));
            }
            if entry.kind != SemanticEntryKind::Dir {
                return Ok(None);
            }
            node = self.load_semantic_tree(&entry.node)?;
        }
        Ok(None)
    }

    /// Symbol-level delta between two states, via a merkle walk that descends
    /// only into differing digests (identical subtrees are pruned without
    /// loading their file nodes).
    pub fn semantic_diff_symbols(&self, a: &StateId, b: &StateId) -> Result<Vec<SymbolDelta>> {
        let (root_a, root_b) = match (self.semantic_index(a)?, self.semantic_index(b)?) {
            (Some(ra), Some(rb)) => (ra, rb),
            _ => return Ok(Vec::new()),
        };
        if root_a.semantic_digest == root_b.semantic_digest {
            return Ok(Vec::new());
        }
        let node_a = self.load_semantic_tree(&root_a.tree)?;
        let node_b = self.load_semantic_tree(&root_b.tree)?;
        let mut out = Vec::new();
        self.diff_tree_nodes(&node_a, &node_b, "", &mut out)?;
        Ok(out)
    }

    fn diff_tree_nodes(
        &self,
        a: &SemanticTreeNode,
        b: &SemanticTreeNode,
        prefix: &str,
        out: &mut Vec<SymbolDelta>,
    ) -> Result<()> {
        let a_by_name: HashMap<&str, &SemanticTreeEntry> =
            a.entries.iter().map(|e| (e.name.as_str(), e)).collect();
        let b_by_name: HashMap<&str, &SemanticTreeEntry> =
            b.entries.iter().map(|e| (e.name.as_str(), e)).collect();

        for entry_a in &a.entries {
            let path = join_path(prefix, &entry_a.name);
            match b_by_name.get(entry_a.name.as_str()) {
                None => self.emit_side(entry_a, &path, Side::Removed, out)?,
                Some(entry_b) => {
                    if entry_a.semantic_digest == entry_b.semantic_digest {
                        continue; // pruned — identical subtree/file.
                    }
                    match (entry_a.kind, entry_b.kind) {
                        (SemanticEntryKind::Dir, SemanticEntryKind::Dir) => {
                            let child_a = self.load_semantic_tree(&entry_a.node)?;
                            let child_b = self.load_semantic_tree(&entry_b.node)?;
                            self.diff_tree_nodes(&child_a, &child_b, &path, out)?;
                        }
                        (SemanticEntryKind::File, SemanticEntryKind::File) => {
                            let file_a = self.load_semantic_file(&entry_a.node)?;
                            let file_b = self.load_semantic_file(&entry_b.node)?;
                            diff_file_symbols(&file_a, &file_b, &path, out);
                        }
                        // Kind flipped (file↔dir↔opaque): a full replace.
                        _ => {
                            self.emit_side(entry_a, &path, Side::Removed, out)?;
                            self.emit_side(entry_b, &path, Side::Added, out)?;
                        }
                    }
                }
            }
        }
        // Names only on the b side are additions.
        for entry_b in &b.entries {
            if !a_by_name.contains_key(entry_b.name.as_str()) {
                let path = join_path(prefix, &entry_b.name);
                self.emit_side(entry_b, &path, Side::Added, out)?;
            }
        }
        Ok(())
    }

    /// Emit every symbol of a single-sided entry (whole file/subtree added or
    /// removed). Opaque entries carry no symbols.
    fn emit_side(
        &self,
        entry: &SemanticTreeEntry,
        path: &str,
        side: Side,
        out: &mut Vec<SymbolDelta>,
    ) -> Result<()> {
        match entry.kind {
            SemanticEntryKind::File => {
                let file = self.load_semantic_file(&entry.node)?;
                for sym in &file.symbols {
                    out.push(side.delta(path, sym));
                }
            }
            SemanticEntryKind::Dir => {
                let node = self.load_semantic_tree(&entry.node)?;
                for child in &node.entries {
                    let child_path = join_path(path, &child.name);
                    self.emit_side(child, &child_path, side, out)?;
                }
            }
            SemanticEntryKind::Opaque => {}
        }
        Ok(())
    }

    /// Whether the symbol at `anchor` changed between `since` and `at`.
    pub fn changed_since(
        &self,
        anchor: &SymbolAnchor,
        since: &StateId,
        at: &StateId,
    ) -> Result<bool> {
        let old = self.symbol_hash(since, anchor)?.map(|s| s.semantic_hash);
        let new = self.symbol_hash(at, anchor)?.map(|s| s.semantic_hash);
        Ok(old != new)
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
            let already = self.load_attached_index(&state_id)?.is_some();
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

#[derive(Clone, Copy)]
enum Side {
    Added,
    Removed,
}

impl Side {
    fn delta(self, path: &str, sym: &SymbolEntry) -> SymbolDelta {
        let anchor = SymbolAnchor::new(path, sym.address());
        match self {
            Side::Added => SymbolDelta {
                anchor,
                kind: sym.kind,
                old_hash: None,
                new_hash: Some(sym.semantic_hash),
            },
            Side::Removed => SymbolDelta {
                anchor,
                kind: sym.kind,
                old_hash: Some(sym.semantic_hash),
                new_hash: None,
            },
        }
    }
}

/// Diff two file nodes by symbol address, emitting a delta per added/removed/
/// changed symbol. Unchanged symbols (equal `semantic_hash`) are skipped.
fn diff_file_symbols(
    a: &SemanticFileNode,
    b: &SemanticFileNode,
    path: &str,
    out: &mut Vec<SymbolDelta>,
) {
    let a_by_addr: HashMap<String, &SymbolEntry> =
        a.symbols.iter().map(|s| (s.address(), s)).collect();
    let b_by_addr: HashMap<String, &SymbolEntry> =
        b.symbols.iter().map(|s| (s.address(), s)).collect();

    for sym_a in &a.symbols {
        let addr = sym_a.address();
        match b_by_addr.get(&addr) {
            None => out.push(Side::Removed.delta(path, sym_a)),
            Some(sym_b) => {
                if sym_a.semantic_hash != sym_b.semantic_hash {
                    out.push(SymbolDelta {
                        anchor: SymbolAnchor::new(path, addr),
                        kind: sym_b.kind,
                        old_hash: Some(sym_a.semantic_hash),
                        new_hash: Some(sym_b.semantic_hash),
                    });
                }
            }
        }
    }
    for sym_b in &b.symbols {
        if !a_by_addr.contains_key(&sym_b.address()) {
            out.push(Side::Added.delta(path, sym_b));
        }
    }
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, Principal, TreeEntry};
    use tempfile::TempDir;

    use super::*;

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
        assert_eq!(parent_builder.parse_count, 2, "cold build parses both files");

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
        let blob = put_blob(&repo, b"fn shared() -> i32 { 7 }\n");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("x.rs", blob, false).unwrap(),
            TreeEntry::file("y.rs", blob, false).unwrap(),
        ]);
        let mut builder = SemanticIndexBuilder::new(repo.store(), EXTRACTOR_VERSION);
        builder.build_root(&tree, None).unwrap();
        assert_eq!(builder.parse_count, 1, "shared blob parsed once");
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
}
