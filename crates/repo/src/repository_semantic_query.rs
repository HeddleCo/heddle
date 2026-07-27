// SPDX-License-Identifier: Apache-2.0
//! Read-only, never-compute query primitives over the merkle semantic index
//! (heddle#1067 / heddle#1078).
//!
//! This module is **always compiled** — it is deliberately free of any
//! dependency on `heddle-semantic`/tree-sitter. Every primitive here is a pure
//! store walk over already-stored, content-addressed semantic nodes; NONE of
//! them ever parse a source blob or recompute an index. A consumer that never
//! parses (e.g. `weft`, which must never carry a parser) reads the persisted
//! index through exactly these entry points.
//!
//! The get-or-compute / self-heal / backfill / builder machinery — everything
//! that can *produce* index nodes — lives in `repository_semantic_index`, gated
//! behind the `tree-sitter-symbols` feature.

use std::collections::{BTreeMap, HashMap};

use objects::{
    object::{
        ContentHash, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot, SemanticTreeEntry,
        SemanticTreeNode, StateId, SymbolAnchor, SymbolEntry, SymbolKindTag,
    },
    store::ObjectStore,
};

use crate::{HeddleError, Repository, Result, StateAttachmentKind};

/// Recursion ceiling for the merkle tree/dir walkers. Legitimate directory
/// nesting is far below this; a crafted, pushed `SemanticTreeNode` chain deeper
/// than this is treated as pathological rather than overflowing the stack
/// (same hardening class as the AST walkers, heddle#876). Shared by the
/// (gated) builder in `repository_semantic_index`.
pub(crate) const MAX_SEMANTIC_TREE_DEPTH: usize = 1024;

/// A single-symbol delta between two states, produced by
/// [`Repository::semantic_diff_symbols`].
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

impl Repository {
    /// Load a state's attached semantic index root, if present AND intact —
    /// **never recomputing**. A missing attachment, a body of another kind, or
    /// a missing/corrupt root blob (e.g. a partially-replicated push, or GC that
    /// pruned the sidecar) is reported as ABSENT — `Ok(None)`.
    ///
    /// This is the parse-free read entry point: it only ever loads an
    /// already-stored root. The get-or-compute behaviour (build forward from
    /// the nearest indexed ancestor) lives in the tree-sitter-gated
    /// [`Repository::semantic_index`](crate::Repository) and is NOT reachable
    /// from here.
    pub fn attached_semantic_index(&self, state_id: &StateId) -> Result<Option<SemanticIndexRoot>> {
        let Some(attachment) =
            self.latest_state_attachment(state_id, StateAttachmentKind::SemanticIndex)?
        else {
            return Ok(None);
        };
        let objects::object::StateAttachmentBody::SemanticIndex(root_hash) = attachment.body else {
            return Ok(None);
        };
        let Some(blob) = self.store().get_blob(&root_hash)? else {
            return Ok(None);
        };
        Ok(SemanticIndexRoot::decode(blob.content()).ok())
    }

    /// Load the per-file semantic node for `path` in a state's attached index —
    /// a pure store walk to the file node, then the node blob. `Ok(None)` when
    /// the state has no attached index, or the path is absent / resolves to a
    /// dir or opaque entry. NEVER parses.
    pub fn semantic_file_node(
        &self,
        state_id: &StateId,
        path: &str,
    ) -> Result<Option<SemanticFileNode>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(node_hash) = self.resolve_file_node(&root, path)? else {
            return Ok(None);
        };
        Ok(Some(self.load_semantic_file(&node_hash)?))
    }

    /// Whether the symbol at `anchor` changed between `since` and `at`.
    /// Resolves each side through whichever `symbol_hash` is compiled (the
    /// never-compute reader without the parser, the get-or-compute reader with
    /// it).
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

    // ----- never-compute inner walks (shared) -------------------------------

    /// Never-compute `symbol_hash`: resolve an anchor against the state's
    /// ATTACHED index only. Backs the public `symbol_hash` in the parse-free
    /// build; the tree-sitter build's `symbol_hash` is get-or-compute instead.
    #[cfg(not(feature = "tree-sitter-symbols"))]
    pub(crate) fn symbol_hash_readonly(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<SymbolEntry>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(file_node_hash) = self.resolve_file_node(&root, &anchor.file)? else {
            return Ok(None);
        };
        let file = self.load_semantic_file(&file_node_hash)?;
        Ok(file.symbol_by_address(&anchor.symbol).cloned())
    }

    /// Never-compute `semantic_changed`: compare the reformat-stable digest at
    /// `path_prefix` between the two states' ATTACHED indexes.
    #[cfg(not(feature = "tree-sitter-symbols"))]
    pub(crate) fn semantic_changed_readonly(
        &self,
        a: &StateId,
        b: &StateId,
        path_prefix: &str,
    ) -> Result<bool> {
        match (
            self.attached_semantic_index(a)?,
            self.attached_semantic_index(b)?,
        ) {
            (Some(root_a), Some(root_b)) => {
                let da = self.digest_at_path(&root_a, path_prefix)?;
                let db = self.digest_at_path(&root_b, path_prefix)?;
                Ok(da != db)
            }
            // A missing index on either side is a difference iff the other exists.
            (a_opt, b_opt) => Ok(a_opt.is_some() != b_opt.is_some()),
        }
    }

    /// Never-compute `semantic_diff_symbols`: merkle-walk the two states'
    /// ATTACHED indexes, descending only into differing digests.
    #[cfg(not(feature = "tree-sitter-symbols"))]
    pub(crate) fn semantic_diff_symbols_readonly(
        &self,
        a: &StateId,
        b: &StateId,
    ) -> Result<Vec<SymbolDelta>> {
        let (root_a, root_b) = match (
            self.attached_semantic_index(a)?,
            self.attached_semantic_index(b)?,
        ) {
            (Some(ra), Some(rb)) => (ra, rb),
            _ => return Ok(Vec::new()),
        };
        if root_a.semantic_digest == root_b.semantic_digest {
            return Ok(Vec::new());
        }
        let node_a = self.load_semantic_tree(&root_a.tree)?;
        let node_b = self.load_semantic_tree(&root_b.tree)?;
        let mut out = Vec::new();
        self.diff_tree_nodes(&node_a, &node_b, "", 0, &mut out)?;
        Ok(out)
    }

    // ----- shared node loaders + walkers ------------------------------------

    /// Load and decode an index root blob by hash. Used only by the (gated)
    /// compute paths that just persisted a root and want it back as a struct;
    /// the read paths go through [`Repository::attached_semantic_index`].
    #[cfg(feature = "tree-sitter-symbols")]
    pub(crate) fn load_index_root(&self, root_hash: &ContentHash) -> Result<SemanticIndexRoot> {
        let blob = self.store().get_blob(root_hash)?.ok_or_else(|| {
            crate::HeddleError::NotFound(format!("semantic index root {root_hash}"))
        })?;
        SemanticIndexRoot::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    pub(crate) fn load_semantic_tree(&self, node_hash: &ContentHash) -> Result<SemanticTreeNode> {
        let blob = self.store().get_blob(node_hash)?.ok_or_else(|| {
            crate::HeddleError::NotFound(format!("semantic tree node {node_hash}"))
        })?;
        SemanticTreeNode::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    pub(crate) fn load_semantic_file(&self, node_hash: &ContentHash) -> Result<SemanticFileNode> {
        let blob = self.store().get_blob(node_hash)?.ok_or_else(|| {
            crate::HeddleError::NotFound(format!("semantic file node {node_hash}"))
        })?;
        SemanticFileNode::decode(blob.content())
            .map_err(|err| crate::HeddleError::InvalidObject(err.to_string()))
    }

    /// Walk the semantic tree to the `File` node for `path`, returning its
    /// storage hash. `None` if the path is absent or resolves to a dir/opaque.
    pub(crate) fn resolve_file_node(
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

    /// The reformat-stable digest at `path_prefix` within an index. Empty prefix
    /// yields the whole-tree digest. Loads only the tree nodes along the path.
    pub(crate) fn digest_at_path(
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

    pub(crate) fn diff_tree_nodes(
        &self,
        a: &SemanticTreeNode,
        b: &SemanticTreeNode,
        prefix: &str,
        depth: usize,
        out: &mut Vec<SymbolDelta>,
    ) -> Result<()> {
        if depth > MAX_SEMANTIC_TREE_DEPTH {
            return Err(HeddleError::InvalidObject(format!(
                "semantic diff exceeds max depth {MAX_SEMANTIC_TREE_DEPTH}"
            )));
        }
        let a_by_name: HashMap<&str, &SemanticTreeEntry> =
            a.entries.iter().map(|e| (e.name.as_str(), e)).collect();
        let b_by_name: HashMap<&str, &SemanticTreeEntry> =
            b.entries.iter().map(|e| (e.name.as_str(), e)).collect();

        for entry_a in &a.entries {
            let path = join_path(prefix, &entry_a.name);
            match b_by_name.get(entry_a.name.as_str()) {
                None => self.emit_side(entry_a, &path, Side::Removed, depth, out)?,
                Some(entry_b) => {
                    if entry_a.semantic_digest == entry_b.semantic_digest {
                        continue; // pruned — identical subtree/file.
                    }
                    match (entry_a.kind, entry_b.kind) {
                        (SemanticEntryKind::Dir, SemanticEntryKind::Dir) => {
                            let child_a = self.load_semantic_tree(&entry_a.node)?;
                            let child_b = self.load_semantic_tree(&entry_b.node)?;
                            self.diff_tree_nodes(&child_a, &child_b, &path, depth + 1, out)?;
                        }
                        (SemanticEntryKind::File, SemanticEntryKind::File) => {
                            let file_a = self.load_semantic_file(&entry_a.node)?;
                            let file_b = self.load_semantic_file(&entry_b.node)?;
                            diff_file_symbols(&file_a, &file_b, &path, out);
                        }
                        // Kind flipped (file↔dir↔opaque): a full replace.
                        _ => {
                            self.emit_side(entry_a, &path, Side::Removed, depth, out)?;
                            self.emit_side(entry_b, &path, Side::Added, depth, out)?;
                        }
                    }
                }
            }
        }
        // Names only on the b side are additions.
        for entry_b in &b.entries {
            if !a_by_name.contains_key(entry_b.name.as_str()) {
                let path = join_path(prefix, &entry_b.name);
                self.emit_side(entry_b, &path, Side::Added, depth, out)?;
            }
        }
        Ok(())
    }

    /// Emit every symbol of a single-sided entry (whole file/subtree added or
    /// removed). Opaque entries carry no symbols.
    pub(crate) fn emit_side(
        &self,
        entry: &SemanticTreeEntry,
        path: &str,
        side: Side,
        depth: usize,
        out: &mut Vec<SymbolDelta>,
    ) -> Result<()> {
        if depth > MAX_SEMANTIC_TREE_DEPTH {
            return Err(HeddleError::InvalidObject(format!(
                "semantic diff exceeds max depth {MAX_SEMANTIC_TREE_DEPTH}"
            )));
        }
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
                    self.emit_side(child, &child_path, side, depth + 1, out)?;
                }
            }
            SemanticEntryKind::Opaque => {}
        }
        Ok(())
    }
}

/// Never-compute public read methods, compiled when the parser is absent — the
/// parse-free consumer's (weft's) surface. When `tree-sitter-symbols` is on,
/// `repository_semantic_index` supplies get-or-compute + self-heal variants of
/// these same names instead.
#[cfg(not(feature = "tree-sitter-symbols"))]
impl Repository {
    /// Resolve a symbol anchor to its entry in a state's ATTACHED index.
    pub fn symbol_hash(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<SymbolEntry>> {
        self.symbol_hash_readonly(state_id, anchor)
    }

    /// Whether the semantic content under `path_prefix` differs between two
    /// states, compared top-down by digest over their ATTACHED indexes.
    pub fn semantic_changed(&self, a: &StateId, b: &StateId, path_prefix: &str) -> Result<bool> {
        self.semantic_changed_readonly(a, b, path_prefix)
    }

    /// Symbol-level delta between two states' ATTACHED indexes.
    pub fn semantic_diff_symbols(&self, a: &StateId, b: &StateId) -> Result<Vec<SymbolDelta>> {
        self.semantic_diff_symbols_readonly(a, b)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Side {
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

/// The identity a symbol is matched on across two file versions:
/// `(container_path, name, kind)`. Distinguishes `fn f` in `mod a` from `mod b`,
/// `fn X` from `struct X`, so the diff never last-wins-collides distinct
/// symbols into a mislabeled Modified or a silent skip.
type SymbolKey = (Vec<String>, String, SymbolKindTag);

fn symbol_key(sym: &SymbolEntry) -> SymbolKey {
    (sym.container_path.clone(), sym.name.clone(), sym.kind)
}

/// Diff two file nodes by symbol identity (a `(container, name, kind)`
/// MULTISET), emitting a delta per added/removed/changed symbol. Same-key
/// entries (e.g. C++ overloads the extractor can't tell apart) are paired
/// positionally in canonical order; unmatched extras become add/remove.
/// Unchanged symbols (equal `semantic_hash`) are skipped.
fn diff_file_symbols(
    a: &SemanticFileNode,
    b: &SemanticFileNode,
    path: &str,
    out: &mut Vec<SymbolDelta>,
) {
    // BTreeMap, not HashMap: the loops below emit into `out`, so key order must
    // be deterministic — this is a public, determinism-centric API. (The
    // pre-refactor code iterated `a.symbols` in canonical order; a HashMap here
    // reintroduced nondeterministic delta ordering.)
    let mut a_by_key: BTreeMap<SymbolKey, Vec<&SymbolEntry>> = BTreeMap::new();
    for sym in &a.symbols {
        a_by_key.entry(symbol_key(sym)).or_default().push(sym);
    }
    let mut b_by_key: BTreeMap<SymbolKey, Vec<&SymbolEntry>> = BTreeMap::new();
    for sym in &b.symbols {
        b_by_key.entry(symbol_key(sym)).or_default().push(sym);
    }

    for (key, a_syms) in &a_by_key {
        let b_syms = b_by_key.get(key).map(Vec::as_slice).unwrap_or(&[]);
        // Symbols arrive in the file node already sorted by
        // (container, name, kind, span.0), so same-key lists are in a stable
        // order; pair them positionally.
        for pair in a_syms.iter().zip(b_syms.iter()) {
            let (sym_a, sym_b) = pair;
            if sym_a.semantic_hash != sym_b.semantic_hash {
                out.push(SymbolDelta {
                    anchor: SymbolAnchor::new(path, sym_b.address()),
                    kind: sym_b.kind,
                    old_hash: Some(sym_a.semantic_hash),
                    new_hash: Some(sym_b.semantic_hash),
                });
            }
        }
        // Extra `a` occurrences with no `b` counterpart are removals.
        for sym_a in a_syms.iter().skip(b_syms.len()) {
            out.push(Side::Removed.delta(path, sym_a));
        }
    }
    // Keys (or extra occurrences of a key) present only in `b` are additions.
    for (key, b_syms) in &b_by_key {
        let a_len = a_by_key.get(key).map(Vec::len).unwrap_or(0);
        for sym_b in b_syms.iter().skip(a_len) {
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
    use std::collections::BTreeMap;

    use chrono::Utc;
    use objects::object::{
        Attribution, Blob, Principal, State, StateAttachment, StateAttachmentBody,
    };
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

    fn put_encoded(repo: &Repository, bytes: Vec<u8>) -> ContentHash {
        repo.store().put_blob(&Blob::new(bytes)).unwrap()
    }

    /// Hand-assemble a one-file semantic index (no parser involved) and attach
    /// its root to a freshly-put state, returning the state id and the root's
    /// tree hash.
    fn attach_handbuilt_index(repo: &Repository) -> (StateId, ContentHash) {
        let sym = SymbolEntry {
            name: "foo".to_string(),
            kind: SymbolKindTag::Function,
            container_path: vec![],
            semantic_hash: ContentHash::compute(b"foo-body"),
            span: (1, 1),
        };
        let file_node = SemanticFileNode::new(
            "rust",
            "0.24",
            1,
            ContentHash::compute(b"src-blob"),
            ContentHash::compute(b"scaffold"),
            vec![sym],
        );
        let file_hash = put_encoded(repo, file_node.encode().unwrap());
        let (tree_node, tree_digest) = SemanticTreeNode::new(vec![SemanticTreeEntry {
            name: "foo.rs".to_string(),
            kind: SemanticEntryKind::File,
            node: file_hash,
            semantic_digest: file_node.semantic_digest,
        }]);
        let tree_hash = put_encoded(repo, tree_node.encode().unwrap());
        let root = SemanticIndexRoot::new(1, BTreeMap::new(), tree_hash, tree_digest);
        let root_hash = put_encoded(repo, root.encode().unwrap());

        let state = State::new(ContentHash::compute(b"state-tree"), vec![], author());
        repo.store().put_state(&state).unwrap();
        let state_id = state.id();
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::SemanticIndex(root_hash),
            attribution: author(),
            created_at: Utc::now(),
            supersedes: None,
        })
        .unwrap();
        (state_id, tree_hash)
    }

    /// `attached_semantic_index` returns `None` for a state with no index
    /// attachment — and NEVER computes one (there is no parser in this build
    /// path, and the state's tree hash is dangling).
    #[test]
    fn attached_semantic_index_none_without_attachment() {
        let (_temp, repo) = repo();
        let state = State::new(ContentHash::compute(b"no-index-tree"), vec![], author());
        repo.store().put_state(&state).unwrap();
        assert!(
            repo.attached_semantic_index(&state.id()).unwrap().is_none(),
            "a state with no attachment must read as no index, never recompute"
        );
    }

    /// `attached_semantic_index` returns the stored root when one is attached,
    /// and `semantic_file_node` loads a file's symbols by path — both pure
    /// store walks, no parser.
    #[test]
    fn attached_index_and_file_node_load_without_parser() {
        let (_temp, repo) = repo();
        let (state_id, tree_hash) = attach_handbuilt_index(&repo);

        let root = repo
            .attached_semantic_index(&state_id)
            .unwrap()
            .expect("attached root must load");
        assert_eq!(root.tree, tree_hash);

        let file = repo
            .semantic_file_node(&state_id, "foo.rs")
            .unwrap()
            .expect("file node must resolve by path");
        assert_eq!(file.language, "rust");
        assert!(
            file.symbols.iter().any(|s| s.name == "foo"),
            "hand-built symbol must be present: {:?}",
            file.symbols
        );

        // An absent path resolves to None, never an error.
        assert!(
            repo.semantic_file_node(&state_id, "missing.rs")
                .unwrap()
                .is_none()
        );
    }
}
