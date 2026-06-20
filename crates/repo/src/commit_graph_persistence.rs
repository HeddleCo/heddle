// SPDX-License-Identifier: Apache-2.0
//! On-disk persistence for the merge commit graph index.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, ContentHash},
};

const GRAPH_FILE_NAME: &str = "commit-graph.bin";
const GRAPH_MAGIC: [u8; 8] = *b"LMGRAPH\0";
const GRAPH_VERSION: u32 = 1;
const CHANGE_ID_BYTES: usize = 16;
const CONTENT_HASH_BYTES: usize = 32;

pub(crate) type LoadedCommitGraph = HashMap<ChangeId, PersistedCommitGraphNode>;

pub(crate) trait CommitGraphCache {
    fn load(&self) -> Result<Option<LoadedCommitGraph>>;
    fn save(&self, nodes: &LoadedCommitGraph) -> Result<()>;
    fn path(&self) -> Option<&Path> {
        None
    }
}

pub(crate) struct FsCommitGraphCache {
    path: PathBuf,
}

impl FsCommitGraphCache {
    pub(crate) fn new(repo_root: &Path) -> Self {
        Self {
            path: commit_graph_path(repo_root),
        }
    }
}

impl CommitGraphCache for FsCommitGraphCache {
    fn load(&self) -> Result<Option<LoadedCommitGraph>> {
        load_commit_graph(&self.path)
    }

    fn save(&self, nodes: &LoadedCommitGraph) -> Result<()> {
        save_commit_graph(&self.path, nodes)
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

pub(crate) struct NullCommitGraphCache;

impl CommitGraphCache for NullCommitGraphCache {
    fn load(&self) -> Result<Option<LoadedCommitGraph>> {
        Ok(None)
    }

    fn save(&self, _nodes: &LoadedCommitGraph) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PersistedCommitGraphNode {
    pub(crate) parents: Vec<ChangeId>,
    pub(crate) generation: usize,
    pub(crate) tree_hash: ContentHash,
    pub(crate) created_at_secs: i64,
    pub(crate) agent_model: Option<String>,
    pub(crate) bloom: Option<[u8; 256]>,
}

pub(crate) fn commit_graph_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".heddle/state").join(GRAPH_FILE_NAME)
}

pub(crate) fn load_commit_graph(
    path: &Path,
) -> Result<Option<HashMap<ChangeId, PersistedCommitGraphNode>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read commit graph at {}", path.display()));
        }
    };

    parse_commit_graph(&bytes)
        .with_context(|| format!("failed to parse commit graph at {}", path.display()))
        .map(Some)
}

pub(crate) fn save_commit_graph(
    path: &Path,
    nodes: &HashMap<ChangeId, PersistedCommitGraphNode>,
) -> Result<()> {
    let bytes = serialize_commit_graph(nodes)?;
    write_file_atomic(path, &bytes)
        .with_context(|| format!("failed to write commit graph at {}", path.display()))?;
    Ok(())
}

fn serialize_commit_graph(nodes: &HashMap<ChangeId, PersistedCommitGraphNode>) -> Result<Vec<u8>> {
    let node_count = u64::try_from(nodes.len()).context("commit graph has too many nodes")?;
    let mut entries: Vec<_> = nodes.iter().collect();
    entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&GRAPH_MAGIC);
    bytes.extend_from_slice(&GRAPH_VERSION.to_le_bytes());
    bytes.extend_from_slice(&node_count.to_le_bytes());

    for (change_id, node) in entries {
        let generation = u64::try_from(node.generation)
            .context("commit graph generation does not fit in u64")?;
        let parent_count =
            u32::try_from(node.parents.len()).context("commit graph node has too many parents")?;

        bytes.extend_from_slice(change_id.as_bytes());
        bytes.extend_from_slice(&generation.to_le_bytes());
        bytes.extend_from_slice(&parent_count.to_le_bytes());
        for parent in &node.parents {
            bytes.extend_from_slice(parent.as_bytes());
        }

        // tree_hash: 32 bytes
        bytes.extend_from_slice(node.tree_hash.as_bytes());

        // created_at_secs: i64 LE
        bytes.extend_from_slice(&node.created_at_secs.to_le_bytes());

        // agent_model: u16 length prefix (0 = None)
        match &node.agent_model {
            Some(model) => {
                let len = u16::try_from(model.len())
                    .context("agent model string too long for commit graph")?;
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(model.as_bytes());
            }
            None => {
                bytes.extend_from_slice(&0u16.to_le_bytes());
            }
        }

        // bloom_present: u8, then bloom data if present
        match &node.bloom {
            Some(bloom) => {
                bytes.push(1u8);
                bytes.extend_from_slice(bloom.as_ref());
            }
            None => {
                bytes.push(0u8);
            }
        }
    }

    Ok(bytes)
}

fn parse_commit_graph(bytes: &[u8]) -> Result<HashMap<ChangeId, PersistedCommitGraphNode>> {
    let mut cursor = GraphCursor::new(bytes);
    let magic = cursor.read_array::<8>()?;
    if magic != GRAPH_MAGIC {
        bail!("unexpected commit graph magic number");
    }

    let version = cursor.read_u32()?;
    if version != GRAPH_VERSION {
        bail!("unsupported commit graph version {version}");
    }

    let node_count = usize::try_from(cursor.read_u64()?)
        .context("commit graph node count does not fit in usize")?;
    let mut nodes = HashMap::with_capacity(node_count);
    for _ in 0..node_count {
        let change_id = ChangeId::from_bytes(cursor.read_array::<CHANGE_ID_BYTES>()?);
        let generation = usize::try_from(cursor.read_u64()?)
            .context("commit graph generation does not fit in usize")?;
        let parent_count = usize::try_from(cursor.read_u32()?)
            .context("commit graph parent count does not fit in usize")?;
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(ChangeId::from_bytes(
                cursor.read_array::<CHANGE_ID_BYTES>()?,
            ));
        }

        let tree_hash = ContentHash::from_bytes(cursor.read_array::<CONTENT_HASH_BYTES>()?);
        let created_at_secs = cursor.read_i64()?;

        let agent_model_len = cursor.read_u16()? as usize;
        let agent_model = if agent_model_len == 0 {
            None
        } else {
            let raw = cursor.read_bytes(agent_model_len)?;
            let model = std::str::from_utf8(&raw)
                .context("agent model is not valid UTF-8")?
                .to_string();
            Some(model)
        };

        let bloom_present = cursor.read_u8()?;
        let bloom = if bloom_present == 1 {
            let raw = cursor.read_array::<256>()?;
            Some(raw)
        } else {
            None
        };

        if nodes
            .insert(
                change_id,
                PersistedCommitGraphNode {
                    parents,
                    generation,
                    tree_hash,
                    created_at_secs,
                    agent_model,
                    bloom,
                },
            )
            .is_some()
        {
            bail!("duplicate change id in commit graph");
        }
    }

    cursor.finish()?;
    Ok(nodes)
}

struct GraphCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> GraphCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self
            .offset
            .checked_add(N)
            .context("commit graph offset overflow")?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .context("commit graph truncated")?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        self.offset = end;
        Ok(out)
    }

    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let end = self
            .offset
            .checked_add(n)
            .context("commit graph offset overflow")?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .context("commit graph truncated")?;
        let out = slice.to_vec();
        self.offset = end;
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8> {
        let end = self
            .offset
            .checked_add(1)
            .context("commit graph offset overflow")?;
        let byte = self
            .bytes
            .get(self.offset)
            .copied()
            .context("commit graph truncated")?;
        self.offset = end;
        Ok(byte)
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.read_array::<8>()?))
    }

    fn finish(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            return Ok(());
        }

        bail!("commit graph has trailing data")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::{Context, Result};
    use objects::object::{ChangeId, ContentHash};

    use super::{
        PersistedCommitGraphNode, commit_graph_path, load_commit_graph, parse_commit_graph,
        save_commit_graph,
    };

    fn make_node(parents: Vec<ChangeId>, generation: usize) -> PersistedCommitGraphNode {
        PersistedCommitGraphNode {
            parents,
            generation,
            tree_hash: ContentHash::from_bytes([0u8; 32]),
            created_at_secs: 1_700_000_000,
            agent_model: None,
            bloom: None,
        }
    }

    #[test]
    fn commit_graph_round_trips_on_disk() -> Result<()> {
        let temp_dir = tempfile::TempDir::new()?;
        let path = commit_graph_path(temp_dir.path());
        let root = ChangeId::generate();
        let child = ChangeId::generate();
        let mut nodes = HashMap::new();
        nodes.insert(root, make_node(Vec::new(), 0));
        nodes.insert(child, make_node(vec![root], 1));

        save_commit_graph(&path, &nodes)?;
        let loaded = load_commit_graph(&path)?.context("missing persisted graph")?;
        assert_eq!(loaded, nodes);

        Ok(())
    }

    #[test]
    fn commit_graph_round_trips_with_full_metadata() -> Result<()> {
        let temp_dir = tempfile::TempDir::new()?;
        let path = commit_graph_path(temp_dir.path());
        let id = ChangeId::generate();
        let tree_hash = ContentHash::from_bytes([42u8; 32]);
        let mut bloom = [0u8; 256];
        bloom[0] = 0xFF;
        bloom[255] = 0xAB;

        let mut nodes = HashMap::new();
        nodes.insert(
            id,
            PersistedCommitGraphNode {
                parents: Vec::new(),
                generation: 5,
                tree_hash,
                created_at_secs: -100,
                agent_model: Some("claude-opus-4-5".to_string()),
                bloom: Some(bloom),
            },
        );

        save_commit_graph(&path, &nodes)?;
        let loaded = load_commit_graph(&path)?.context("missing persisted graph")?;
        assert_eq!(loaded, nodes);

        Ok(())
    }

    #[test]
    fn commit_graph_rejects_invalid_magic() {
        let error = parse_commit_graph(b"not-a-commit-graph").unwrap_err();
        assert!(error.to_string().contains("magic"));
    }
}
