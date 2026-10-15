// SPDX-License-Identifier: Apache-2.0
use std::{cell::Cell, collections::HashMap, time::Instant};

use objects::object::{ContentHash, StateId};

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
        Self {
            end,
            edges: HashMap::new(),
            reads: Cell::new(0),
        }
    }
}

impl DerivedSource for History {
    fn input(
        &self,
        state: StateId,
        max_bytes: usize,
        max_parents: usize,
    ) -> DerivedResult<Option<DerivedInput>> {
        self.reads.set(self.reads.get() + 1);
        let n = u64::from_le_bytes(state.as_bytes()[..8].try_into().expect("id"));
        if n > self.end {
            return Ok(None);
        }
        let parents = self
            .edges
            .get(&state)
            .cloned()
            .unwrap_or_else(|| if n == 0 { vec![] } else { vec![id(n - 1)] });
        let input = DerivedInput {
            state,
            parents,
            change: n.to_le_bytes().to_vec(),
        };
        input.check_limits(max_bytes, max_parents)?;
        Ok(Some(input))
    }
}

#[test]
fn deep_history_and_append_use_bounded_work_on_small_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            for count in [2_000, 52_000, 100_000] {
                let store = LocalDerivedStore::open(":memory:").expect("store");
                let source = History::linear(count);
                let limits = Limits::default();
                let start = Instant::now();
                let ready = ensure(&source, &store, &StateClosure, id(count - 1), limits)
                    .expect("derive deep history");
                assert_eq!(ready.computed, count);
                assert_eq!(source.reads.get(), count as usize);
                assert!(ready.peak_working_bytes <= limits.max_working_bytes);
                assert!(ready.scratch_bytes <= limits.max_scratch_bytes);
                source.reads.set(0);
                let append =
                    ensure(&source, &store, &StateClosure, id(count), limits).expect("append");
                assert_eq!(append.computed, 1);
                assert_eq!(source.reads.get(), 1);
                let warm = ensure(&source, &store, &StateClosure, id(count), limits).expect("warm");
                assert_eq!(warm.root, append.root);
                assert_eq!(warm.computed, 0);
                assert_eq!(source.reads.get(), 1);
                eprintln!(
                    "states={count} elapsed={:?} working_bytes={} scratch_bytes={} append_inputs=1",
                    start.elapsed(),
                    ready.peak_working_bytes,
                    ready.scratch_bytes
                );
            }
        })
        .expect("spawn")
        .join()
        .expect("small stack worker");
}

#[test]
fn diamond_and_wide_merge_share_nodes_and_match_reachability_oracle() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(20);
    for n in 1..=10 {
        source.edges.insert(id(n), vec![id(0)]);
    }
    source.edges.insert(id(11), (1..=10).map(id).collect());
    source.edges.insert(id(12), vec![]);
    let ready = ensure(&source, &store, &StateClosure, id(11), Limits::default())?;
    assert_eq!(ready.computed, 12);
    for n in 0..=12 {
        assert_eq!(
            closure_contains(&store, ready.root, id(n), Limits::default())?,
            n <= 11
        );
    }
    let node = load_node(
        &store,
        StateClosure.descriptor().key(id(11)),
        Limits::default().max_node_bytes,
    )?;
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
    const RESTART_PATH: &str = "HEDDLE_DERIVED_TEST_RESTART_PATH";
    const RESTART_ROOT: &str = "HEDDLE_DERIVED_TEST_RESTART_ROOT";
    if let Some(path) = std::env::var_os(RESTART_PATH) {
        let store = LocalDerivedStore::open(path)?;
        let source = History::linear(50);
        let ready = ensure(&source, &store, &StateClosure, id(50), Limits::default())?;
        assert_eq!(source.reads.get(), 0);
        assert_eq!(
            ready.root.to_hex(),
            std::env::var(RESTART_ROOT).expect("expected root")
        );
        return Ok(());
    }
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("derived.sqlite");
    let source = History::linear(50);
    let store = LocalDerivedStore::open(&path)?;
    let lazy = ensure(&source, &store, &StateClosure, id(50), Limits::default())?;
    drop(store);
    let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "derived::tests::lazy_eager_recomputation_and_restart_have_identical_roots",
        ])
        .env(RESTART_PATH, &path)
        .env(RESTART_ROOT, lazy.root.to_hex())
        .output()
        .expect("restart test process");
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let reopened = LocalDerivedStore::open(path)?;
    source.reads.set(0);
    let warm = ensure(&source, &reopened, &StateClosure, id(50), Limits::default())?;
    assert_eq!(source.reads.get(), 0);
    assert_eq!(warm.root, lazy.root);
    let other = LocalDerivedStore::open(temp.path().join("other.sqlite"))?;
    for n in 0..=50 {
        ensure(&source, &other, &StateClosure, id(n), Limits::default())?;
    }
    assert_eq!(
        load_node(
            &other,
            StateClosure.descriptor().key(id(50)),
            Limits::default().max_node_bytes
        )?
        .hash()?,
        lazy.root
    );
    Ok(())
}

#[test]
fn missing_parent_and_cycle_never_bind_a_result() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(3);
    source.edges.insert(id(3), vec![id(4)]);
    assert!(matches!(
        ensure(&source, &store, &StateClosure, id(3), Limits::default()),
        Err(DerivedError::MissingState(_))
    ));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(3)))?, None);
    source.edges.insert(id(3), vec![id(2)]);
    source.edges.insert(id(2), vec![id(3)]);
    assert!(matches!(
        ensure(&source, &store, &StateClosure, id(3), Limits::default()),
        Err(DerivedError::Cycle(_))
    ));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(3)))?, None);
    Ok(())
}

#[test]
fn limits_report_incomplete_work_and_can_resume() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(100);
    let limits = Limits {
        max_steps: 10,
        ..Limits::default()
    };
    assert!(matches!(
        ensure(&source, &store, &StateClosure, id(100), limits),
        Err(DerivedError::Limit(_))
    ));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(100)))?, None);
    let ready = ensure(&source, &store, &StateClosure, id(100), Limits::default())?;
    assert!(closure_contains(
        &store,
        ready.root,
        id(0),
        Limits::default()
    )?);
    assert!(matches!(
        closure_contains(&store, ready.root, id(101), limits),
        Err(DerivedError::Limit(_))
    ));
    Ok(())
}

#[test]
fn corrupt_missing_and_wrong_version_parent_values_fail_closed() -> DerivedResult<()> {
    struct Broken<'a> {
        store: &'a LocalDerivedStore,
        corrupt: bool,
    }
    impl DerivedStore for Broken<'_> {
        fn lookup(&self, key: DerivedKey) -> DerivedResult<Option<ContentHash>> {
            self.store.lookup(key)
        }
        fn get_node(&self, hash: ContentHash, max_bytes: usize) -> DerivedResult<Option<Vec<u8>>> {
            if self.corrupt {
                Ok(Some(vec![0; 20]))
            } else {
                let _ = (hash, max_bytes);
                Ok(None)
            }
        }
        fn put_node_if_absent(&self, hash: ContentHash, bytes: &[u8]) -> DerivedResult<()> {
            self.store.put_node_if_absent(hash, bytes)
        }
        fn bind_if_absent(&self, key: DerivedKey, root: ContentHash) -> DerivedResult<()> {
            self.store.bind_if_absent(key, root)
        }
    }
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(1);
    ensure(&source, &store, &StateClosure, id(0), Limits::default())?;
    for corrupt in [false, true] {
        assert!(
            ensure(
                &source,
                &Broken {
                    store: &store,
                    corrupt
                },
                &StateClosure,
                id(1),
                Limits::default()
            )
            .is_err()
        );
        assert_eq!(store.lookup(StateClosure.descriptor().key(id(1)))?, None);
    }
    let mut node = load_node(
        &store,
        StateClosure.descriptor().key(id(0)),
        Limits::default().max_node_bytes,
    )?;
    node.key.version += 1;
    let bytes = node.encode()?;
    let hash = node.hash()?;
    store.put_node_if_absent(hash, &bytes)?;
    assert!(matches!(
        store.bind_if_absent(StateClosure.descriptor().key(id(0)), hash),
        Err(DerivedError::Conflict)
    ));
    assert!(
        node.check_key(StateClosure.descriptor().key(id(0)))
            .is_err()
    );
    assert!(matches!(
        store.put_node_if_absent(hash, b"different bytes"),
        Err(DerivedError::Corrupt(_))
    ));
    Ok(())
}

#[test]
fn native_states_use_the_shared_closure_derivation() -> DerivedResult<()> {
    use objects::{
        object::{Attribution, Principal, State},
        store::{InMemoryStore, ObjectStore},
    };
    let objects = InMemoryStore::new();
    let attribution = Attribution::human(Principal::new("Closure test", "closure@example.invalid"));
    let mut root = State::new(ContentHash::compute(b"tree"), vec![], attribution.clone());
    root.hash();
    objects.put_state(&root).expect("state");
    let mut child = State::new(
        ContentHash::compute(b"changed tree"),
        vec![root.state_id],
        attribution,
    );
    child.hash();
    objects.put_state(&child).expect("state");
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = StateGraphSource(&objects);
    let ready = ensure(
        &source,
        &store,
        &StateClosure,
        child.state_id,
        Limits::default(),
    )?;
    assert!(closure_contains(
        &store,
        ready.root,
        root.state_id,
        Limits::default()
    )?);
    let node = load_node(
        &store,
        StateClosure.descriptor().key(child.state_id),
        Limits::default().max_node_bytes,
    )?;
    assert_eq!(node.value, child.tree.as_bytes());
    assert!(!closure_contains(
        &store,
        ready.root,
        id(500),
        Limits::default()
    )?);
    Ok(())
}

#[test]
fn input_memory_parent_and_disk_budgets_fail_explicitly() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(1000);
    for limits in [
        Limits {
            max_parents: 0,
            ..Limits::default()
        },
        Limits {
            max_input_bytes: 4,
            ..Limits::default()
        },
        Limits {
            max_working_bytes: 1024,
            ..Limits::default()
        },
        Limits {
            max_node_bytes: 2,
            ..Limits::default()
        },
        Limits {
            max_scratch_bytes: 16 * 4096,
            ..Limits::default()
        },
    ] {
        let error =
            ensure(&source, &store, &StateClosure, id(1000), limits).expect_err("budget must fail");
        assert!(matches!(error, DerivedError::Limit(_)), "{error:?}");
        assert_eq!(store.lookup(StateClosure.descriptor().key(id(1000)))?, None);
    }
    Ok(())
}

#[test]
fn a_wrong_version_binding_is_rejected_before_parent_composition() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let source = History::linear(1);
    let key = StateClosure.descriptor().key(id(0));
    let wrong_key = DerivedKey {
        version: key.version + 1,
        ..key
    };
    let wrong = DerivedNode {
        key: wrong_key,
        parents: vec![],
        value: vec![],
    };
    let root = wrong.hash()?;
    store.put_node_if_absent(root, &wrong.encode()?)?;
    assert!(matches!(
        store.bind_if_absent(key, root),
        Err(DerivedError::Corrupt(_))
    ));
    // Simulate a host adapter returning a valid node under the wrong key.
    struct Wrong<'a> {
        store: &'a LocalDerivedStore,
        root: ContentHash,
    }
    impl DerivedStore for Wrong<'_> {
        fn lookup(&self, key: DerivedKey) -> DerivedResult<Option<ContentHash>> {
            Ok((key.state == id(0)).then_some(self.root))
        }
        fn get_node(&self, hash: ContentHash, cap: usize) -> DerivedResult<Option<Vec<u8>>> {
            self.store.get_node(hash, cap)
        }
        fn put_node_if_absent(&self, hash: ContentHash, bytes: &[u8]) -> DerivedResult<()> {
            self.store.put_node_if_absent(hash, bytes)
        }
        fn bind_if_absent(&self, key: DerivedKey, root: ContentHash) -> DerivedResult<()> {
            self.store.bind_if_absent(key, root)
        }
    }
    assert!(matches!(
        ensure(
            &source,
            &Wrong {
                store: &store,
                root
            },
            &StateClosure,
            id(1),
            Limits::default()
        ),
        Err(DerivedError::Corrupt(_))
    ));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(1)))?, None);
    Ok(())
}

#[test]
fn a_full_width_merge_stays_within_the_working_budget() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(1025);
    for n in 1..=1024 {
        source.edges.insert(id(n), vec![id(0)]);
    }
    source.edges.insert(id(1025), (1..=1024).map(id).collect());
    let ready = ensure(&source, &store, &StateClosure, id(1025), Limits::default())?;
    assert_eq!(ready.computed, 1026);
    assert!(ready.peak_working_bytes < Limits::default().max_working_bytes);
    assert!(closure_contains(
        &store,
        ready.root,
        id(1024),
        Limits::default()
    )?);
    assert!(!closure_contains(
        &store,
        ready.root,
        id(1026),
        Limits::default()
    )?);
    Ok(())
}

#[test]
fn derivations_receive_parent_values_in_recorded_order() -> DerivedResult<()> {
    struct Fold;
    impl Derivation for Fold {
        fn descriptor(&self) -> Descriptor {
            Descriptor::new("test-ordered-fold", 1)
        }
        fn compose(
            &self,
            input: &DerivedInput,
            parents: &[DerivedNode],
            max_bytes: usize,
        ) -> DerivedResult<Vec<u8>> {
            if max_bytes < 32 {
                return Err(DerivedError::Limit("result bytes"));
            }
            let mut hasher = blake3::Hasher::new();
            hasher.update(&input.change);
            for parent in parents {
                hasher.update(&parent.value);
            }
            Ok(hasher.finalize().as_bytes().to_vec())
        }
    }
    let temp = tempfile::tempdir().expect("temp");
    let store = LocalDerivedStore::open(temp.path().join("derived.sqlite"))?;
    let mut source = History::linear(3);
    source.edges.insert(id(3), vec![id(2), id(1)]);
    let ready = ensure(&source, &store, &Fold, id(3), Limits::default())?;
    let mut values: Vec<[u8; 32]> = vec![];
    for n in 0u64..3 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&n.to_le_bytes());
        if let Some(parent) = values.last() {
            hasher.update(parent);
        }
        values.push(*hasher.finalize().as_bytes());
    }
    let mut expected = blake3::Hasher::new();
    expected.update(&3u64.to_le_bytes());
    expected.update(&values[2]);
    expected.update(&values[1]);
    let actual = load_node(
        &store,
        Fold.descriptor().key(id(3)),
        Limits::default().max_node_bytes,
    )?;
    assert_eq!(actual.value, expected.finalize().as_bytes());
    assert_eq!(actual.hash()?, ready.root);
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(3)))?, None);
    Ok(())
}

#[test]
fn local_store_detects_corruption_and_conflicting_valid_results() -> DerivedResult<()> {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("derived.sqlite");
    let store = LocalDerivedStore::open(&path)?;
    let source = History::linear(1);
    let ready = ensure(&source, &store, &StateClosure, id(0), Limits::default())?;
    let key = StateClosure.descriptor().key(id(0));
    let mut other = load_node(&store, key, Limits::default().max_node_bytes)?;
    other.value.push(42);
    let other_root = other.hash()?;
    store.put_node_if_absent(other_root, &other.encode()?)?;
    assert!(matches!(
        store.bind_if_absent(key, other_root),
        Err(DerivedError::Conflict)
    ));
    assert_eq!(store.lookup(key)?, Some(ready.root));
    assert!(matches!(
        store.get_node(ready.root, 1),
        Err(DerivedError::Limit(_))
    ));
    let db = rusqlite::Connection::open(&path).expect("open test database");
    db.execute(
        "UPDATE derived_nodes SET bytes=x'00' WHERE hash=?1",
        [ready.root.as_bytes().as_slice()],
    )
    .expect("inject corruption");
    assert!(matches!(
        ensure(&source, &store, &StateClosure, id(1), Limits::default()),
        Err(DerivedError::Corrupt(_))
    ));
    assert_eq!(store.lookup(StateClosure.descriptor().key(id(1)))?, None);
    Ok(())
}

#[test]
fn insufficient_working_budget_does_not_read_source() -> DerivedResult<()> {
    let store = LocalDerivedStore::open(":memory:")?;
    let source = History::linear(1);
    let limits = Limits {
        max_working_bytes: 1024,
        ..Limits::default()
    };
    assert!(matches!(
        ensure(&source, &store, &StateClosure, id(1), limits),
        Err(DerivedError::Limit(_))
    ));
    assert_eq!(
        source.reads.get(),
        0,
        "reject before discovering/allocating source input"
    );
    Ok(())
}
