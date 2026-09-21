// SPDX-License-Identifier: Apache-2.0
//! Object-source semantic index assembly, independent of repository capture.

use crate::{
    parser::Language,
    semantic_index::{
        extract_semantic_file, grammar_version, grammar_version_by_name, language_name,
    },
};
use objects::{
    error::{HeddleError, Result},
    object::{
        ContentHash, SemanticEntryKind, SemanticFileFacts, SemanticFileNode, SemanticIndexRoot,
        SemanticTreeEntry, SemanticTreeNode, Tree, TreeEntryTarget,
    },
    store::ObjectStore,
};
use std::collections::{BTreeMap, HashMap};

const MAX_SEMANTIC_TREE_DEPTH: usize = 1024;

/// Source files above this size are recorded as `Opaque` rather than parsed —
/// generated/vendored blobs dominate parse cost and rarely carry review-worthy
/// symbols.
const SEMANTIC_FILE_BUDGET_BYTES: usize = 1 << 20;

type PendingSemanticBlobs = Vec<(ContentHash, Vec<u8>)>;
pub type DeferredSemanticRoot = (SemanticIndexRoot, ContentHash, PendingSemanticBlobs);

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
///
/// Work and output allowances apply to the lifetime of this builder, including
/// repeated root builds. Flushing pending bytes does not reset accounting or
/// memoized allocations; create a fresh builder for an independent analysis.
pub struct SemanticIndexBuilder<'store, S: ObjectStore> {
    store: &'store S,
    budget: Option<crate::parser::ParseBudget>,
    work_entries: usize,
    work_bytes: usize,
    output_limit: Option<usize>,
    pending_bytes: usize,
    source_blobs: Option<&'store HashMap<ContentHash, &'store [u8]>>,
    source_trees: Option<&'store HashMap<ContentHash, &'store Tree>>,
    extractor_version: u32,
    /// Per-build memo keyed by `(source blob hash, language)` — a blob that
    /// appears at several paths is parsed once, but byte-identical blobs at
    /// `a.js` and `b.py` get distinct nodes (a node is a pure function of
    /// `(bytes, ext, grammar, extractor)`).
    file_memo: HashMap<(ContentHash, Language), BuiltEntry>,
    /// Languages encountered while building, seeded from the parent root so
    /// pruned subtrees' grammars are not lost.
    grammars: BTreeMap<String, String>,
    /// Node blobs written this build, flushed as one pack at the end so a
    /// snapshot never regresses to N loose per-node fsyncs.
    pending: Vec<(ContentHash, Vec<u8>)>,
    /// Number of source blobs actually parsed this build.
    pub parse_count: usize,
}

impl<'store, S: ObjectStore> SemanticIndexBuilder<'store, S> {
    pub fn new(store: &'store S, extractor_version: u32) -> Self {
        Self {
            store,
            budget: None,
            work_entries: 0,
            work_bytes: 0,
            output_limit: None,
            pending_bytes: 0,
            source_blobs: None,
            source_trees: None,
            extractor_version,
            file_memo: HashMap::new(),
            grammars: BTreeMap::new(),
            pending: Vec::new(),
            parse_count: 0,
        }
    }

    /// Bound RPC analysis independently of historical capture/backfill work.
    pub fn with_budget(mut self, budget: crate::parser::ParseBudget) -> Self {
        self.budget = Some(budget);
        self.output_limit.get_or_insert(15 * 1024 * 1024);
        self
    }
    /// Bound lifetime canonical output independently of input size. The encoder
    /// checks the remaining allowance before growing each node's buffer;
    /// flushing a completed root does not replenish this allowance.
    pub fn with_output_limit(mut self, bytes: usize) -> Self {
        self.output_limit = Some(bytes);
        self
    }

    fn check_work(&self) -> Result<()> {
        if self
            .budget
            .as_ref()
            .is_some_and(|budget| budget.interrupted())
        {
            return Err(HeddleError::InvalidObject(
                "semantic analysis interrupted".into(),
            ));
        }
        if self.budget.is_some() && (self.work_entries > 4096 || self.work_bytes > 32 * 1024 * 1024)
        {
            return Err(HeddleError::InvalidObject(
                "semantic analysis source work budget exceeded".into(),
            ));
        }
        Ok(())
    }

    pub fn with_source_objects(
        store: &'store S,
        extractor_version: u32,
        source_blobs: &'store HashMap<ContentHash, &'store [u8]>,
        source_trees: &'store HashMap<ContentHash, &'store Tree>,
    ) -> Self {
        Self {
            source_blobs: Some(source_blobs),
            source_trees: Some(source_trees),
            ..Self::new(store, extractor_version)
        }
    }

    /// Build the index for `tree`, optionally reusing `parent` (its source tree
    /// plus semantic index root) for unchanged-subtree pruning. Returns the
    /// persisted [`SemanticIndexRoot`] and its storage hash.
    pub fn build_root(
        &mut self,
        tree: &Tree,
        parent: Option<&ParentIndex>,
    ) -> Result<(SemanticIndexRoot, ContentHash)> {
        let (root, root_hash, pending) = self.build_root_deferred(tree, parent)?;
        self.check_work()?;
        self.store.put_blobs_packed(pending)?;
        Ok((root, root_hash))
    }

    /// Build the semantic closure without crossing a durability barrier.
    /// Worktree snapshots fold these blobs into their authoritative commit
    /// pack; other callers use [`Self::build_root`] for immediate persistence.
    pub fn build_root_deferred(
        &mut self,
        tree: &Tree,
        parent: Option<&ParentIndex>,
    ) -> Result<DeferredSemanticRoot> {
        // Refuse node reuse across an extractor or grammar bump: reusing stale
        // nodes would mix v1+v2 fingerprints in one index. A non-current parent
        // is dropped entirely, forcing a clean full rebuild.
        let parent = parent.filter(|p| self.parent_is_reusable(p));
        if let Some(parent) = parent {
            self.grammars = parent.root.grammars.clone();
        }
        let parent_ctx = parent.map(|p| (&p.source_tree, &p.semantic_tree));
        let (node_hash, digest) = self.build_tree(tree, parent_ctx, 0)?;
        let root = SemanticIndexRoot::new(
            self.extractor_version,
            std::mem::take(&mut self.grammars),
            node_hash,
            digest,
        );
        let root_hash = self.put_node(&root)?;
        Ok((root, root_hash, std::mem::take(&mut self.pending)))
    }

    /// Whether a parent index may be reused: its extractor version and every
    /// grammar version must match the builder's current ones.
    fn parent_is_reusable(&self, parent: &ParentIndex) -> bool {
        parent.root.extractor_version == self.extractor_version
            && parent
                .root
                .grammars
                .iter()
                .all(|(name, version)| grammar_version_by_name(name) == Some(version.as_str()))
    }

    fn build_tree(
        &mut self,
        tree: &Tree,
        parent: Option<(&Tree, &SemanticTreeNode)>,
        depth: usize,
    ) -> Result<(ContentHash, ContentHash)> {
        if depth > MAX_SEMANTIC_TREE_DEPTH {
            return Err(HeddleError::InvalidObject(format!(
                "semantic index tree exceeds max depth {MAX_SEMANTIC_TREE_DEPTH}"
            )));
        }
        self.work_entries = self.work_entries.saturating_add(tree.len());
        self.check_work()?;
        let mut entries = Vec::with_capacity(tree.len());
        for entry in tree.entries() {
            self.check_work()?;
            let name = entry.name();
            let built = match entry.target() {
                TreeEntryTarget::Tree { hash } => self.build_dir(name, *hash, parent, depth)?,
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
        let node_hash = self.put_node(&node)?;
        Ok((node_hash, digest))
    }

    fn build_dir(
        &mut self,
        name: &str,
        source_hash: ContentHash,
        parent: Option<(&Tree, &SemanticTreeNode)>,
        depth: usize,
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

        let source_tree = match self
            .source_trees
            .and_then(|trees| trees.get(&source_hash).copied())
        {
            Some(tree) => tree.clone(),
            None => self
                .store
                .get_tree(&source_hash)?
                .ok_or_else(|| HeddleError::NotFound(format!("tree {source_hash}")))?,
        };

        // Descend with the matching parent subtree as the reuse basis, if any.
        let child_parent = self.child_parent_ctx(name, parent)?;
        let child_parent_ref = child_parent.as_ref().map(|(t, n)| (t, n));
        let (node, digest) = self.build_tree(&source_tree, child_parent_ref, depth + 1)?;
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
        // A file node is a pure function of (bytes, ext/language, grammar,
        // extractor), so memoize per `(source_hash, language)` — NOT bytes
        // alone (byte-identical `a.js`/`b.py` must not share a node).
        let language = Language::from_path(std::path::Path::new(name));
        let memo_key = (source_hash, language);
        if let Some(built) = self.file_memo.get(&memo_key) {
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
            self.file_memo.insert(memo_key, built);
            return Ok(built);
        }

        let built = self.parse_file(language, source_hash)?;
        self.file_memo.insert(memo_key, built);
        Ok(built)
    }

    fn parse_file(&mut self, language: Language, source_hash: ContentHash) -> Result<BuiltEntry> {
        let opaque = BuiltEntry {
            kind: SemanticEntryKind::Opaque,
            node: source_hash,
            semantic_digest: source_hash,
        };

        if language.parser_handle().is_none() {
            return Ok(opaque);
        }
        let blob = match self
            .source_blobs
            .and_then(|blobs| blobs.get(&source_hash).copied())
        {
            Some(bytes) => Some(objects::object::Blob::from(bytes.to_vec())),
            None => self.store.get_blob(&source_hash)?,
        };
        let Some(blob) = blob else {
            return Ok(opaque);
        };
        self.work_bytes = self.work_bytes.saturating_add(blob.size());
        self.check_work()?;
        if blob.size() > SEMANTIC_FILE_BUDGET_BYTES {
            return Ok(opaque);
        }
        let extracted = match &self.budget {
            Some(budget) => crate::semantic_index::extract_semantic_file_bounded(
                blob.content(),
                language,
                budget,
            )
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?,
            None => extract_semantic_file(blob.content(), language),
        };
        self.check_work()?;
        let Some(extracted) = extracted else {
            // Unsupported/parse-fail → opaque.
            return Ok(opaque);
        };
        self.parse_count += 1;

        let lang = language_name(extracted.language).to_string();
        let gv = grammar_version(extracted.language).to_string();
        // Freshly-parsed grammar version wins in the root metadata (overwrite,
        // not or_insert) so a stale seed from a current parent can't linger.
        self.grammars.insert(lang.clone(), gv.clone());

        let node = SemanticFileNode::new(
            lang,
            gv,
            self.extractor_version,
            source_hash,
            extracted.scaffold_hash,
            SemanticFileFacts {
                symbols: extracted.symbols,
                scopes: extracted.scopes,
                imports: extracted.imports,
                occurrences: extracted.occurrences,
            },
        );
        let digest = node.semantic_digest;
        let node_hash = self.put_node(&node)?;
        Ok(BuiltEntry {
            kind: SemanticEntryKind::File,
            node: node_hash,
            semantic_digest: digest,
        })
    }

    /// Queue an encoded node blob for the end-of-build pack flush, returning its
    /// content hash (identical to what `put_blob` would assign).
    fn put_node(&mut self, node: &impl serde::Serialize) -> Result<ContentHash> {
        self.check_work()?;
        let remaining = self
            .output_limit
            .unwrap_or(usize::MAX)
            .saturating_sub(self.pending_bytes);
        let mut writer = SemanticNodeWriter {
            bytes: Vec::new(),
            remaining,
            exceeded: false,
        };
        let encoded =
            node.serialize(&mut rmp_serde::Serializer::new(&mut writer).with_struct_map());
        if writer.exceeded {
            return Err(HeddleError::InvalidObject(
                "semantic analysis output budget exceeded".into(),
            ));
        }
        encoded.map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        let bytes = writer.bytes;
        let hash = ContentHash::compute_typed("blob", &bytes);
        self.pending_bytes = self.pending_bytes.saturating_add(bytes.len());
        self.pending.push((hash, bytes));
        Ok(hash)
    }
}

/// A canonical node is encoded directly into its remaining output allowance.
/// Reject before extending the byte buffer, not after materializing a node.
struct SemanticNodeWriter {
    bytes: Vec<u8>,
    remaining: usize,
    exceeded: bool,
}
impl std::io::Write for SemanticNodeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "semantic analysis output budget exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        self.remaining = self.remaining.saturating_sub(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_index::EXTRACTOR_VERSION;
    use objects::object::{SemanticEntryKind, SemanticTreeEntry};
    #[test]
    fn bounded_analysis_encodes_canonical_nodes_with_cumulative_output_limit() {
        let store = objects::store::InMemoryStore::new();
        let (node, _) = SemanticTreeNode::new(vec![SemanticTreeEntry {
            name: "a.rs".into(),
            kind: SemanticEntryKind::Opaque,
            node: ContentHash::from_bytes([23; 32]),
            semantic_digest: ContentHash::from_bytes([23; 32]),
        }]);
        let canonical = node.encode().expect("canonical bytes");
        let mut builder = SemanticIndexBuilder::new(&store, EXTRACTOR_VERSION)
            .with_output_limit(canonical.len() * 2 - 1);
        let hash = builder.put_node(&node).expect("first node fits");
        assert_eq!(hash, ContentHash::compute_typed("blob", &canonical));
        assert_eq!(
            builder.pending[0].1, canonical,
            "encoder retains exact canonical representation"
        );
        let error = builder
            .put_node(&node)
            .expect_err("second node exceeds total output allowance");
        assert!(
            error.to_string().contains("output budget exceeded"),
            "{error}"
        );
        assert_eq!(builder.pending.len(), 1, "failed node is never queued");
        assert_eq!(builder.pending_bytes, canonical.len());
        assert!(
            store.get_blob(&hash).expect("lookup").is_none(),
            "no node is published during bounded assembly"
        );
    }

    #[test]
    fn bounded_analysis_writer_rejects_before_buffer_growth() {
        use std::io::Write;
        let mut writer = SemanticNodeWriter {
            bytes: Vec::new(),
            remaining: 7,
            exceeded: false,
        };
        writer.write_all(b"small").expect("within allowance");
        let capacity = writer.bytes.capacity();
        let expanded = vec![0; 1 << 20];
        let error = writer
            .write_all(&expanded)
            .expect_err("reject before growing buffer");
        assert!(error.to_string().contains("output budget exceeded"));
        assert_eq!(writer.bytes, b"small");
        assert_eq!(writer.bytes.capacity(), capacity);
    }
}
