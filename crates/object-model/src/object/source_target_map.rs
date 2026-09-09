//! Immutable source-target bindings. Forks share a root; updates copy one trie
//! route, never the map or its referencing annotations. Addresses are ordinary
//! blob hashes. A failed update may leave unreachable immutable nodes, but
//! never returns a replacement root for the caller to install.
use super::ContentHash;

const MAGIC: &[u8; 5] = b"HDTM\x01";
const LEAF_ENTRIES: usize = 8;
const LEVELS: usize = 52;
/// Largest canonical node: header, depth, bitmap and 32 (hash, count) children.
pub const MAX_NODE_BYTES: usize = 5 + 1 + 1 + 4 + 32 * 40;

/// Storage must bound a read before allocating/fetching more than `max_bytes`.
/// Writes install immutable bytes at their ordinary blob address.
pub trait SourceTargetMapStore {
    type Error;
    fn read(&mut self, hash: ContentHash, max_bytes: usize)
    -> Result<Option<Vec<u8>>, Self::Error>;
    fn write(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), Self::Error>;
}

/// Remaining work allowance. Reads/writes are charged before the store call;
/// read bytes are charged from the actual returned node, not its maximum size.
#[derive(Clone, Copy, Debug)]
pub struct MapBudget {
    pub node_reads: usize,
    pub read_bytes: usize,
    pub node_writes: usize,
    pub write_bytes: usize,
}
impl MapBudget {
    pub const fn new(
        node_reads: usize,
        read_bytes: usize,
        node_writes: usize,
        write_bytes: usize,
    ) -> Self {
        Self {
            node_reads,
            read_bytes,
            node_writes,
            write_bytes,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MapError<E> {
    #[error("source target map storage error")]
    Storage(E),
    #[error("source target map read budget exhausted")]
    ReadBudget,
    #[error("source target map write budget exhausted")]
    WriteBudget,
    #[error("source target map node is missing: {0}")]
    MissingNode(ContentHash),
    #[error("source target map node does not match its blob address: {0}")]
    HashMismatch(ContentHash),
    #[error("invalid source target map node: {0}")]
    InvalidNode(&'static str),
}

pub struct SourceTargetMap;
impl SourceTargetMap {
    pub fn get<S: SourceTargetMapStore>(
        store: &mut S,
        root: Option<ContentHash>,
        key: ContentHash,
        budget: &mut MapBudget,
    ) -> Result<Option<ContentHash>, MapError<S::Error>> {
        let Some(mut hash) = root else {
            return Ok(None);
        };
        let mut expected_count = None;
        let mut prefix = Vec::new();
        loop {
            match load(store, hash, expected_count, &prefix, budget)? {
                Node::Leaf(entries) => {
                    return Ok(entries
                        .binary_search_by_key(&key, |entry| entry.0)
                        .ok()
                        .map(|index| entries[index].1));
                }
                Node::Branch { children, .. } => {
                    let slot = group(key, prefix.len());
                    let Ok(index) = children.binary_search_by_key(&slot, |child| child.slot) else {
                        return Ok(None);
                    };
                    hash = children[index].link.hash;
                    expected_count = Some(children[index].link.count);
                    prefix.push(slot);
                }
            }
        }
    }

    /// `None` deletes the key. Empty maps use a `None` root and store no node.
    /// Equal values and absent deletions write nothing. The returned root is the
    /// only publication point; keep the prior root when this returns an error.
    pub fn update<S: SourceTargetMapStore>(
        store: &mut S,
        root: Option<ContentHash>,
        key: ContentHash,
        value: Option<ContentHash>,
        budget: &mut MapBudget,
    ) -> Result<Option<ContentHash>, MapError<S::Error>> {
        Ok(update_at(
            store,
            root.map(|hash| (hash, None)),
            key,
            value,
            &mut Vec::new(),
            budget,
        )?
        .map(|link| link.hash))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Link {
    hash: ContentHash,
    count: u64,
}
struct Child {
    slot: u8,
    link: Link,
}
enum Node {
    Leaf(Vec<(ContentHash, ContentHash)>),
    Branch { depth: u8, children: Vec<Child> },
}
impl Node {
    fn count(&self) -> Option<u64> {
        match self {
            Self::Leaf(entries) => Some(entries.len() as u64),
            Self::Branch { children, .. } => children
                .iter()
                .try_fold(0u64, |total, child| total.checked_add(child.link.count)),
        }
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        match self {
            Self::Leaf(entries) => {
                bytes.extend_from_slice(&[0, entries.len() as u8]);
                for (key, value) in entries {
                    bytes.extend_from_slice(key.as_bytes());
                    bytes.extend_from_slice(value.as_bytes());
                }
            }
            Self::Branch { depth, children } => {
                bytes.extend_from_slice(&[1, *depth]);
                let bitmap = children
                    .iter()
                    .fold(0u32, |bits, child| bits | (1u32 << child.slot));
                bytes.extend_from_slice(&bitmap.to_le_bytes());
                for child in children {
                    bytes.extend_from_slice(child.link.hash.as_bytes());
                    bytes.extend_from_slice(&child.link.count.to_le_bytes());
                }
            }
        }
        bytes
    }
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], &'static str> {
    let value = bytes
        .get(..N)
        .ok_or("truncated node")?
        .try_into()
        .map_err(|_| "invalid node field")?;
    *bytes = &bytes[N..];
    Ok(value)
}
fn decode(mut bytes: &[u8], prefix: &[u8]) -> Result<Node, &'static str> {
    if &take::<5>(&mut bytes)? != MAGIC {
        return Err("unsupported node format");
    }
    let kind = take::<1>(&mut bytes)?[0];
    let node = match kind {
        0 => {
            let count = usize::from(take::<1>(&mut bytes)?[0]);
            if !(1..=LEAF_ENTRIES).contains(&count) {
                return Err("invalid leaf size");
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let key = ContentHash::from_bytes(take::<32>(&mut bytes)?);
                let value = ContentHash::from_bytes(take::<32>(&mut bytes)?);
                if entries
                    .last()
                    .is_some_and(|entry: &(ContentHash, ContentHash)| entry.0 >= key)
                {
                    return Err("leaf keys are not strictly ordered");
                }
                if prefix
                    .iter()
                    .enumerate()
                    .any(|(depth, slot)| group(key, depth) != *slot)
                {
                    return Err("leaf key is outside its route");
                }
                entries.push((key, value));
            }
            Node::Leaf(entries)
        }
        1 => {
            let depth = take::<1>(&mut bytes)?[0];
            if usize::from(depth) != prefix.len() || prefix.len() >= LEVELS {
                return Err("invalid branch depth");
            }
            let bitmap = u32::from_le_bytes(take::<4>(&mut bytes)?);
            if bitmap == 0 {
                return Err("empty branch");
            }
            let mut children = Vec::with_capacity(bitmap.count_ones() as usize);
            for slot in 0..32 {
                if bitmap & (1 << slot) != 0 {
                    let hash = ContentHash::from_bytes(take::<32>(&mut bytes)?);
                    let count = u64::from_le_bytes(take::<8>(&mut bytes)?);
                    if count == 0 {
                        return Err("empty child");
                    }
                    children.push(Child {
                        slot,
                        link: Link { hash, count },
                    });
                }
            }
            let node = Node::Branch { depth, children };
            if node.count().ok_or("subtree count overflow")? <= LEAF_ENTRIES as u64 {
                return Err("branch must collapse to a leaf");
            }
            node
        }
        _ => return Err("unknown node kind"),
    };
    if !bytes.is_empty() {
        return Err("trailing node bytes");
    }
    Ok(node)
}

// Hash keys already provide a fixed 256-bit route. Like manifest routes, read
// five bits most-significant first; the final group has one real bit.
fn group(key: ContentHash, depth: usize) -> u8 {
    let mut slot = 0;
    for offset in 0..5 {
        let bit = depth * 5 + offset;
        slot = (slot << 1)
            | if bit < 256 {
                (key.as_bytes()[bit / 8] >> (7 - bit % 8)) & 1
            } else {
                0
            };
    }
    slot
}
fn load<S: SourceTargetMapStore>(
    store: &mut S,
    hash: ContentHash,
    expected_count: Option<u64>,
    prefix: &[u8],
    budget: &mut MapBudget,
) -> Result<Node, MapError<S::Error>> {
    if budget.node_reads == 0 || budget.read_bytes == 0 {
        return Err(MapError::ReadBudget);
    }
    budget.node_reads -= 1;
    let limit = budget.read_bytes.min(MAX_NODE_BYTES);
    let bytes = store
        .read(hash, limit)
        .map_err(MapError::Storage)?
        .ok_or(MapError::MissingNode(hash))?;
    if bytes.len() > limit {
        return Err(MapError::ReadBudget);
    }
    budget.read_bytes -= bytes.len();
    if ContentHash::compute_typed("blob", &bytes) != hash {
        return Err(MapError::HashMismatch(hash));
    }
    let node = decode(&bytes, prefix).map_err(MapError::InvalidNode)?;
    if expected_count.is_some_and(|count| node.count() != Some(count)) {
        return Err(MapError::InvalidNode("child count differs from parent"));
    }
    Ok(node)
}
fn persist<S: SourceTargetMapStore>(
    store: &mut S,
    node: Node,
    budget: &mut MapBudget,
) -> Result<Link, MapError<S::Error>> {
    let count = node
        .count()
        .ok_or(MapError::InvalidNode("subtree count overflow"))?;
    let bytes = node.encode();
    if budget.node_writes == 0 || budget.write_bytes < bytes.len() {
        return Err(MapError::WriteBudget);
    }
    budget.node_writes -= 1;
    budget.write_bytes -= bytes.len();
    let hash = ContentHash::compute_typed("blob", &bytes);
    store.write(hash, bytes).map_err(MapError::Storage)?;
    Ok(Link { hash, count })
}
fn build<S: SourceTargetMapStore>(
    store: &mut S,
    entries: Vec<(ContentHash, ContentHash)>,
    depth: usize,
    budget: &mut MapBudget,
) -> Result<Link, MapError<S::Error>> {
    if entries.len() <= LEAF_ENTRIES {
        return persist(store, Node::Leaf(entries), budget);
    }
    if depth >= LEVELS {
        return Err(MapError::InvalidNode("key route exhausted"));
    }
    let mut groups: [Vec<_>; 32] = std::array::from_fn(|_| Vec::new());
    for entry in entries {
        groups[usize::from(group(entry.0, depth))].push(entry);
    }
    let mut children = Vec::new();
    for (slot, entries) in groups.into_iter().enumerate() {
        if !entries.is_empty() {
            children.push(Child {
                slot: slot as u8,
                link: build(store, entries, depth + 1, budget)?,
            });
        }
    }
    persist(
        store,
        Node::Branch {
            depth: depth as u8,
            children,
        },
        budget,
    )
}
fn collect<S: SourceTargetMapStore>(
    store: &mut S,
    link: Link,
    prefix: &[u8],
    entries: &mut Vec<(ContentHash, ContentHash)>,
    budget: &mut MapBudget,
) -> Result<(), MapError<S::Error>> {
    if link.count > LEAF_ENTRIES as u64 {
        return Err(MapError::InvalidNode("collapse exceeds leaf bound"));
    }
    match load(store, link.hash, Some(link.count), prefix, budget)? {
        Node::Leaf(mut leaf) => {
            if entries.len() + leaf.len() > LEAF_ENTRIES {
                return Err(MapError::InvalidNode("collapse exceeds leaf bound"));
            }
            entries.append(&mut leaf);
        }
        Node::Branch { .. } => return Err(MapError::InvalidNode("small subtree is not canonical")),
    }
    Ok(())
}
fn update_at<S: SourceTargetMapStore>(
    store: &mut S,
    old: Option<(ContentHash, Option<u64>)>,
    key: ContentHash,
    value: Option<ContentHash>,
    prefix: &mut Vec<u8>,
    budget: &mut MapBudget,
) -> Result<Option<Link>, MapError<S::Error>> {
    let Some((hash, expected_count)) = old else {
        return value
            .map(|value| persist(store, Node::Leaf(vec![(key, value)]), budget))
            .transpose();
    };
    let node = load(store, hash, expected_count, prefix, budget)?;
    let original = Link {
        hash,
        count: node
            .count()
            .ok_or(MapError::InvalidNode("subtree count overflow"))?,
    };
    match node {
        Node::Leaf(mut entries) => {
            match (entries.binary_search_by_key(&key, |entry| entry.0), value) {
                (Ok(index), Some(value)) if entries[index].1 == value => return Ok(Some(original)),
                (Err(_), None) => return Ok(Some(original)),
                (Ok(index), Some(value)) => entries[index].1 = value,
                (Ok(index), None) => {
                    entries.remove(index);
                }
                (Err(index), Some(value)) => entries.insert(index, (key, value)),
            }
            if entries.is_empty() {
                return Ok(None);
            }
            Ok(Some(build(store, entries, prefix.len(), budget)?))
        }
        Node::Branch {
            depth,
            mut children,
        } => {
            let slot = group(key, prefix.len());
            let position = children.binary_search_by_key(&slot, |child| child.slot);
            let previous = position.ok().map(|index| children[index].link);
            prefix.push(slot);
            let result = update_at(
                store,
                previous.map(|link| (link.hash, Some(link.count))),
                key,
                value,
                prefix,
                budget,
            );
            prefix.pop();
            let next = result?;
            if previous == next {
                return Ok(Some(original));
            }
            match (position, next) {
                (Ok(index), Some(link)) => children[index].link = link,
                (Ok(index), None) => {
                    children.remove(index);
                }
                (Err(index), Some(link)) => children.insert(index, Child { slot, link }),
                (Err(_), None) => return Ok(Some(original)),
            }
            if children.is_empty() {
                return Ok(None);
            }
            let node = Node::Branch { depth, children };
            if node
                .count()
                .ok_or(MapError::InvalidNode("subtree count overflow"))?
                <= LEAF_ENTRIES as u64
            {
                let Node::Branch { children, .. } = node else {
                    return Err(MapError::InvalidNode("expected branch"));
                };
                let mut entries = Vec::new();
                for child in children {
                    prefix.push(child.slot);
                    let result = collect(store, child.link, prefix, &mut entries, budget);
                    prefix.pop();
                    result?;
                }
                entries.sort_unstable_by_key(|entry| entry.0);
                return Ok(Some(persist(store, Node::Leaf(entries), budget)?));
            }
            Ok(Some(persist(store, node, budget)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct StoreError(&'static str);
    #[derive(Default)]
    struct MemoryStore {
        nodes: BTreeMap<ContentHash, Vec<u8>>,
        reads: usize,
        read_bytes: usize,
        writes: usize,
        write_bytes: usize,
    }
    impl MemoryStore {
        fn reset_counts(&mut self) {
            self.reads = 0;
            self.read_bytes = 0;
            self.writes = 0;
            self.write_bytes = 0;
        }
    }
    impl SourceTargetMapStore for MemoryStore {
        type Error = StoreError;
        fn read(
            &mut self,
            hash: ContentHash,
            max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, Self::Error> {
            self.reads += 1;
            let Some(bytes) = self.nodes.get(&hash) else {
                return Ok(None);
            };
            if bytes.len() > max_bytes {
                return Err(StoreError("bounded read refused"));
            }
            self.read_bytes += bytes.len();
            Ok(Some(bytes.clone()))
        }
        fn write(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), Self::Error> {
            assert_eq!(
                hash,
                ContentHash::compute_typed("blob", &bytes),
                "ordinary blob storage key"
            );
            assert!(bytes.len() <= MAX_NODE_BYTES, "bounded canonical node");
            self.writes += 1;
            self.write_bytes += bytes.len();
            if let Some(old) = self.nodes.insert(hash, bytes.clone()) {
                assert_eq!(old, bytes, "writes cannot replace immutable content");
            }
            Ok(())
        }
    }
    fn budget() -> MapBudget {
        MapBudget::new(1024, 2_000_000, 1024, 2_000_000)
    }
    fn key(value: u32) -> ContentHash {
        ContentHash::compute(&value.to_le_bytes())
    }
    fn put(
        store: &mut MemoryStore,
        root: Option<ContentHash>,
        key: ContentHash,
        value: Option<ContentHash>,
    ) -> Option<ContentHash> {
        SourceTargetMap::update(store, root, key, value, &mut budget()).expect("bounded map update")
    }
    fn get(
        store: &mut MemoryStore,
        root: Option<ContentHash>,
        key: ContentHash,
    ) -> Option<ContentHash> {
        SourceTargetMap::get(store, root, key, &mut budget()).expect("bounded map lookup")
    }

    #[test]
    fn one_and_ten_thousand_bindings_touch_only_one_bounded_route() {
        for size in [1, 10_000] {
            let mut store = MemoryStore::default();
            let mut root = None;
            for index in 0..size {
                root = put(&mut store, root, key(index), Some(key(index + 100_000)));
            }
            store.reset_counts();
            let before = budget();
            let mut work = before;
            let changed =
                SourceTargetMap::update(&mut store, root, key(0), Some(key(900_000)), &mut work)
                    .expect("one replacement");
            assert_ne!(changed, root);
            assert!(
                (1..=6).contains(&store.reads),
                "replacement must not scan {size} bindings: {} reads",
                store.reads
            );
            assert!(
                (1..=6).contains(&store.writes),
                "replacement must not rewrite {size} bindings: {} writes",
                store.writes
            );
            eprintln!(
                "{size} bindings: replacement {} reads/{} bytes, {} writes/{} bytes",
                store.reads, store.read_bytes, store.writes, store.write_bytes
            );
            assert_eq!(before.node_reads - work.node_reads, store.reads);
            assert_eq!(before.read_bytes - work.read_bytes, store.read_bytes);
            assert_eq!(before.node_writes - work.node_writes, store.writes);
            assert_eq!(before.write_bytes - work.write_bytes, store.write_bytes);
            store.reset_counts();
            assert_eq!(get(&mut store, changed, key(0)), Some(key(900_000)));
            assert!(
                (1..=6).contains(&store.reads),
                "lookup follows only key's route"
            );
            assert_eq!(store.writes, 0);
            assert_eq!(
                get(&mut store, root, key(0)),
                Some(key(100_000)),
                "parent root is unchanged"
            );
            if size > 1 {
                assert_eq!(
                    get(&mut store, changed, key(size - 1)),
                    Some(key(size - 1 + 100_000))
                );
            }
        }
    }

    #[test]
    fn unchanged_updates_and_absent_deletions_write_zero_nodes() {
        let mut store = MemoryStore::default();
        let mut root = None;
        for index in 0..100 {
            root = put(&mut store, root, key(index), Some(key(index + 100)));
        }
        store.reset_counts();
        let mut no_writes = MapBudget::new(64, 100_000, 0, 0);
        assert_eq!(
            SourceTargetMap::update(&mut store, root, key(37), Some(key(137)), &mut no_writes)
                .expect("equal value needs no writes"),
            root
        );
        assert_eq!(
            SourceTargetMap::update(&mut store, root, key(999), None, &mut no_writes)
                .expect("absent key needs no writes"),
            root
        );
        assert_eq!(store.writes, 0);
        assert_eq!(store.write_bytes, 0);
        store.reset_counts();
        assert_eq!(put(&mut store, None, key(999), None), None);
        assert_eq!((store.reads, store.writes), (0, 0));
    }

    #[test]
    fn forks_share_roots_and_deletions_collapse_to_canonical_nodes() {
        let mut store = MemoryStore::default();
        let mut parent = None;
        for index in 0..48 {
            parent = put(&mut store, parent, key(index), Some(key(index + 100)));
        }
        store.reset_counts();
        let mut fork = parent;
        assert_eq!(
            (store.reads, store.writes),
            (0, 0),
            "fork copies just the root"
        );
        for index in 0..41 {
            fork = put(&mut store, fork, key(index), None);
        }
        let mut rebuilt = None;
        for index in (41..48).rev() {
            rebuilt = put(&mut store, rebuilt, key(index), Some(key(index + 100)));
        }
        assert_eq!(
            fork, rebuilt,
            "collapsed map has a history-independent canonical root"
        );
        assert_eq!(get(&mut store, fork, key(0)), None);
        assert_eq!(get(&mut store, parent, key(0)), Some(key(100)));
        for index in 41..48 {
            fork = put(&mut store, fork, key(index), None);
        }
        assert_eq!(fork, None);
        assert_eq!(get(&mut store, parent, key(47)), Some(key(147)));
    }

    #[test]
    fn hash_integrity_missing_nodes_and_exact_routes_are_checked() {
        let mut store = MemoryStore::default();
        let root = put(&mut store, None, key(1), Some(key(2))).expect("root");
        // Still a valid canonical leaf for the same key; only its claimed blob
        // address is wrong. A decoder or routing check cannot rescue this test.
        let changed = Node::Leaf(vec![(key(1), key(3))]).encode();
        let original = store.nodes.insert(root, changed).expect("original bytes");
        assert!(
            matches!(SourceTargetMap::get(&mut store, Some(root), key(1), &mut budget()), Err(MapError::HashMismatch(hash)) if hash == root)
        );
        store.nodes.remove(&root);
        assert!(
            matches!(SourceTargetMap::get(&mut store, Some(root), key(1), &mut budget()), Err(MapError::MissingNode(hash)) if hash == root)
        );
        store.nodes.insert(root, original);
        assert_eq!(get(&mut store, Some(root), key(1)), Some(key(2)));

        let child = persist(
            &mut store,
            Node::Leaf(vec![(key(1), key(2))]),
            &mut budget(),
        )
        .expect("valid leaf");
        let other_slot = (group(key(1), 0) + 1) % 32;
        let branch = Node::Branch {
            depth: 0,
            children: vec![Child {
                slot: other_slot,
                link: Link { count: 9, ..child },
            }],
        };
        let branch =
            persist(&mut store, branch, &mut budget()).expect("store malformed routing fixture");
        let mut wrong_key = *key(1).as_bytes();
        wrong_key[0] = (other_slot << 3) | (wrong_key[0] & 7);
        assert!(matches!(
            SourceTargetMap::get(
                &mut store,
                Some(branch.hash),
                ContentHash::from_bytes(wrong_key),
                &mut budget()
            ),
            Err(MapError::InvalidNode("leaf key is outside its route"))
        ));
    }

    #[test]
    fn reads_and_updates_stop_at_budget_without_installing_a_partial_root() {
        let mut store = MemoryStore::default();
        let mut root = None;
        for index in 0..100 {
            root = put(&mut store, root, key(index), Some(key(index + 100)));
        }
        store.reset_counts();
        assert!(matches!(
            SourceTargetMap::get(
                &mut store,
                root,
                key(1),
                &mut MapBudget::new(0, 100_000, 0, 0)
            ),
            Err(MapError::ReadBudget)
        ));
        assert_eq!(store.reads, 0, "reject before physical store read");
        let mut bounded = MapBudget::new(64, 100_000, 1, 100_000);
        assert!(matches!(
            SourceTargetMap::update(&mut store, root, key(1), Some(key(900_000)), &mut bounded),
            Err(MapError::WriteBudget)
        ));
        assert_eq!(
            store.writes, 1,
            "only permitted unreachable immutable write occurred"
        );
        assert_eq!(
            get(&mut store, root, key(1)),
            Some(key(101)),
            "caller still has valid original root"
        );
        store.reset_counts();
        assert!(matches!(
            SourceTargetMap::get(&mut store, root, key(1), &mut MapBudget::new(64, 1, 0, 0)),
            Err(MapError::Storage(StoreError("bounded read refused")))
        ));
        assert_eq!(
            store.read_bytes, 0,
            "backend refuses oversized body before fetching it"
        );
        store.reset_counts();
        assert!(matches!(
            SourceTargetMap::update(
                &mut store,
                root,
                key(1),
                Some(key(900_000)),
                &mut MapBudget::new(64, 100_000, 64, 1)
            ),
            Err(MapError::WriteBudget)
        ));
        assert_eq!(
            store.writes, 0,
            "byte budget is checked before physical write"
        );
    }

    #[test]
    fn common_prefix_and_insertion_order_preserve_canonical_roots() {
        let mut store = MemoryStore::default();
        let mut forward = None;
        let mut reverse = None;
        let keys: Vec<_> = (0..16)
            .map(|n| {
                let mut bytes = [0; 32];
                bytes[31] = n;
                ContentHash::from_bytes(bytes)
            })
            .collect();
        for key in &keys {
            forward = put(&mut store, forward, *key, Some(*key));
        }
        for key in keys.iter().rev() {
            reverse = put(&mut store, reverse, *key, Some(*key));
        }
        assert_eq!(
            forward, reverse,
            "routing remains deterministic through a very long common prefix"
        );
        for key in &keys {
            assert_eq!(get(&mut store, forward, *key), Some(*key));
        }
        let mut remaining = forward;
        for key in &keys[..9] {
            remaining = put(&mut store, remaining, *key, None);
        }
        for key in &keys[9..] {
            assert_eq!(get(&mut store, remaining, *key), Some(*key));
        }
    }
}
