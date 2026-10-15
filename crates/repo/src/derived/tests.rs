// SPDX-License-Identifier: Apache-2.0
use std::{cell::Cell, collections::HashMap, time::Instant};

use objects::object::{ContentHash, StateId, derived::*};

use super::*;

fn id(n: u64) -> StateId {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    StateId::from_bytes(bytes)
}

struct History {
    end: u64,
    edges: HashMap<StateId, Vec<StateId>>,
    reads: Cell<usize>,
}

impl History {
    fn linear(end: u64) -> Self {
        Self { end, edges: HashMap::new(), reads: Cell::new(0) }
    }
}

impl DerivedSource for History {
    fn input(&self, state: StateId, max_bytes: usize, max_parents: usize) -> DerivedResult<Option<DerivedInput>> {
        self.reads.set(self.reads.get() + 1);
        let n = u64::from_le_bytes(state.as_bytes()[..8].try_into().expect("id"));
        if n > self.end { return Ok(None); }
        let parents = self.edges.get(&state).cloned().unwrap_or_else(|| if n == 0 { vec![] } else { vec![id(n - 1)] });
        let input = DerivedInput { state, parents, change: n.to_le_bytes().to_vec() };
        input.check_limits(max_bytes, max_parents)?;
        Ok(Some(input))
    }
}

#[test]
fn deep_history_and_append_use_bounded_work_on_small_stack() {
    std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(|| {
        for count in [2_000, 52_000, 100_000] {
            let temp = tempfile::tempdir().expect("temp");
            let store = LocalDerivedStore::open(temp.path().join("derived.sqlite")).expect("store");
            let source = History::linear(count);
            let limits = Limits::default();
            let start = Instant::now();
            let ready = ensure(&source, &store, &StateClosure, id(count - 1), limits).expect("derive deep history");
            assert_eq!(ready.computed, count);
            assert_eq!(source.reads.get(), count as usize);
            assert!(ready.peak_working_bytes <= limits.max_working_bytes);
            assert!(ready.scratch_bytes <= limits.max_scratch_bytes);
            source.reads.set(0);
            let append = ensure(&source, &store, &StateClosure, id(count), limits).expect("append");
            assert_eq!(append.computed, 1);
            assert_eq!(source.reads.get(), 1);
            let warm = ensure(&source, &store, &StateClosure, id(count), limits).expect("warm");
            assert_eq!(warm.root, append.root);
            assert_eq!(warm.computed, 0);
            assert_eq!(source.reads.get(), 1);
            eprintln!("states={count} elapsed={:?} working_bytes={} scratch_bytes={} append_inputs=1", start.elapsed(), ready.peak_working_bytes, ready.scratch_bytes);
        }
    }).expect("spawn").join().expect("small stack worker");
}

#[test]
fn diamond_and_wide_merge_share_nodes_and_match_reachability_oracle() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(20);
    for n in 1..=10 { source.edges.insert(id(n), vec![id(0)]); }
    source.edges.insert(id(11), (1..=10).map(id).collect());
    source.edges.insert(id(12), vec![]);
    let ready = ensure(&source, &store, &StateClosure, id(11), Limits::default())?;
    assert_eq!(ready.computed, 12);
    for n in 0..=12 {
        assert_eq!(closure_contains(&store, ready.root, id(n), Limits::default())?, n <= 11);
    }
    let node = load_node(&store, StateClosure.descriptor().key(id(11)), Limits::default().max_node_bytes)?;
    assert_eq!(node.parents.len(), 10);
    for (n, parent) in node.parents.iter().enumerate() {
        assert_eq!(parent.key.state, id(n as u64 + 1));
        let branch = load_node(&store, parent.key, Limits::default().max_node_bytes)?;
        assert_eq!(branch.parents[0].key.state, id(0));
    }
    Ok(())
}

#[test]
fn lazy_eager_recomputation_and_restart_have_identical_roots() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("derived.sqlite");
    let source = History::linear(50);
    let store = LocalDerivedStore::open(&path)?;
    let lazy = ensure(&source, &store, &StateClosure, id(50), Limits::default())?;
    drop(store);
    let reopened = LocalDerivedStore::open(path)?;
    source.reads.set(0);
    let warm = ensure(&source, &reopened, &StateClosure, id(50), Limits::default())?;
    assert_eq!(source.reads.get(), 0);
    assert_eq!(warm.root, lazy.root);
    let other = LocalDerivedStore::open(temp.path().join("other.sqlite"))?;
    for n in 0..=50 { ensure(&source, &other, &StateClosure, id(n), Limits::default())?; }
    assert_eq!(load_node(&other, StateClosure.descriptor().key(id(50)), Limits::default().max_node_bytes)?.hash()?, lazy.root);
    Ok(())
}

#[test]
fn missing_parent_and_cycle_never_bind_a_result() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(3);
    source.edges.insert(id(3), vec![id(4)]);
    assert!(matches!(ensure(&source, &store, &StateClosure, id(3), Limits::default()), Err(DerivedError::MissingState(_))));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(3)))?, None);
    source.edges.insert(id(3), vec![id(2)]);
    source.edges.insert(id(2), vec![id(3)]);
    assert!(matches!(ensure(&source, &store, &StateClosure, id(3), Limits::default()), Err(DerivedError::Cycle(_))));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(3)))?, None);
    Ok(())
}

#[test]
fn limits_report_incomplete_work_and_can_resume() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(100);
    let limits = Limits { max_steps: 10, ..Limits::default() };
    assert!(matches!(ensure(&source, &store, &StateClosure, id(100), limits), Err(DerivedError::Limit(_))));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(100)))?, None);
    let ready = ensure(&source, &store, &StateClosure, id(100), Limits::default())?;
    assert!(closure_contains(&store, ready.root, id(0), Limits::default())?);
    assert!(matches!(closure_contains(&store, ready.root, id(101), limits), Err(DerivedError::Limit(_))));
    Ok(())
}

#[test]
fn corrupt_missing_and_wrong_version_parent_values_fail_closed() -> DerivedResult<()> {
    struct Broken<'a> { store: &'a LocalDerivedStore, corrupt: bool }
    impl DerivedStore for Broken<'_> {
        fn lookup(&self, key: DerivedKey) -> DerivedResult<Option<ContentHash>> { self.store.lookup(key) }
        fn get_node(&self, hash: ContentHash, max_bytes: usize) -> DerivedResult<Option<Vec<u8>>> {
            if self.corrupt { Ok(Some(vec![0; 20])) } else { let _ = (hash, max_bytes); Ok(None) }
        }
        fn put_node_if_absent(&self, hash: ContentHash, bytes: &[u8]) -> DerivedResult<()> { self.store.put_node_if_absent(hash, bytes) }
        fn bind_if_absent(&self, key: DerivedKey, root: ContentHash) -> DerivedResult<()> { self.store.bind_if_absent(key, root) }
    }
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(1);
    ensure(&source, &store, &StateClosure, id(0), Limits::default())?;
    for corrupt in [false, true] {
        assert!(ensure(&source, &Broken { store: &store, corrupt }, &StateClosure, id(1), Limits::default()).is_err());
        assert_eq!(store.lookup(StateClosure.descriptor().key(id(1)))?, None);
    }
    let mut node = load_node(&store, StateClosure.descriptor().key(id(0)), Limits::default().max_node_bytes)?;
    node.key.version += 1;
    let bytes = node.encode()?;
    let hash = node.hash()?;
    store.put_node_if_absent(hash, &bytes)?;
    assert!(matches!(store.bind_if_absent(StateClosure.descriptor().key(id(0)), hash), Err(DerivedError::Conflict)));
    assert!(node.check_key(StateClosure.descriptor().key(id(0))).is_err());
    assert!(matches!(store.put_node_if_absent(hash, b"different bytes"), Err(DerivedError::Corrupt(_))));
    Ok(())
}
