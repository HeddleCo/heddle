// SPDX-License-Identifier: Apache-2.0
//! Iterative derived-data execution and local persistence.
//!
//! Eager callers stream states through `ensure` in parent order. Lazy callers
//! use the same function when they need a missing result. Errors/limits leave
//! partial reusable nodes but never bind the requested result as ready. The
//! worklist spills to a temporary database; no recursive calls, futures, or
//! per-ancestor inputs are held on the Rust stack or executor heap. Async hosts
//! run this synchronous engine on a bounded blocking worker pool.

mod store;
mod work;

use objects::object::{ContentHash, StateId, derived::*};
pub use store::LocalDerivedStore;
use work::Work;

/// Per-call bounds. Byte accounting includes the 1 MiB worklist page cache and
/// conservative space for encoded/decoded inputs, parent values and output.
/// Source/store caches and implementation-internal compose scratch are owned
/// by those implementations and must be bounded separately by the host.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_steps: u64,
    pub max_parents: usize,
    pub max_input_bytes: usize,
    pub max_node_bytes: usize,
    pub max_working_bytes: usize,
    pub max_scratch_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 1_000_000,
            max_parents: 1024,
            max_input_bytes: 1024 * 1024,
            max_node_bytes: 1024 * 1024,
            max_working_bytes: 8 * 1024 * 1024,
            max_scratch_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct Ready {
    pub root: ContentHash,
    pub computed: u64,
    pub peak_working_bytes: usize,
    pub scratch_bytes: u64,
}

fn storage_error(error: impl std::fmt::Display) -> DerivedError {
    DerivedError::Storage(error.to_string())
}

fn step(steps: &mut u64, limits: Limits) -> DerivedResult<()> {
    if *steps >= limits.max_steps {
        return Err(DerivedError::Limit("work steps"));
    }
    *steps += 1;
    Ok(())
}

fn charge(used: &mut usize, bytes: usize, limits: Limits) -> DerivedResult<()> {
    *used = used
        .checked_add(bytes)
        .ok_or(DerivedError::Limit("working bytes"))?;
    if *used > limits.max_working_bytes {
        return Err(DerivedError::Limit("working bytes"));
    }
    Ok(())
}

fn read_input(
    source: &impl DerivedSource,
    state: StateId,
    limits: Limits,
) -> DerivedResult<DerivedInput> {
    let input = source
        .input(state, limits.max_input_bytes, limits.max_parents)?
        .ok_or(DerivedError::MissingState(state))?;
    if input.state != state {
        return Err(DerivedError::Corrupt("input identity"));
    }
    input.check_limits(limits.max_input_bytes, limits.max_parents)?;
    Ok(input)
}

fn read_node(
    store: &impl DerivedStore,
    root: ContentHash,
    max_bytes: usize,
) -> DerivedResult<DerivedNode> {
    let bytes = store
        .get_node(root, max_bytes)?
        .ok_or(DerivedError::MissingValue(root))?;
    if bytes.len() > max_bytes {
        return Err(DerivedError::Limit("node bytes"));
    }
    if node_hash(&bytes) != root {
        return Err(DerivedError::Corrupt("node hash"));
    }
    DerivedNode::decode(&bytes)
}

/// Load and validate a result without interpreting missing/corrupt data as an
/// empty value. A warm lookup reads one binding/node, never its entire closure.
pub fn load_node(
    store: &impl DerivedStore,
    key: DerivedKey,
    max_bytes: usize,
) -> DerivedResult<DerivedNode> {
    let root = store
        .lookup(key)?
        .ok_or(DerivedError::MissingValue(key.state.as_content_hash()))?;
    let node = read_node(store, root, max_bytes)?;
    node.check_key(key)?;
    Ok(node)
}

/// Ensure a complete immutable result. Parent results are read from the store,
/// composed in recorded order and released after each state. Work limits are
/// explicit failure/coverage boundaries; retrying with more budget reuses any
/// completed results. A cached successful binding assumes the store's DAG
/// retention guarantee, rather than revalidating all old history on append.
pub fn ensure(
    source: &impl DerivedSource,
    store: &impl DerivedStore,
    derivation: &impl Derivation,
    state: StateId,
    limits: Limits,
) -> DerivedResult<Ready> {
    let descriptor = derivation.descriptor();
    let key = descriptor.key(state);
    if let Some(root) = store.lookup(key)? {
        let cap = limits.max_node_bytes.min(limits.max_working_bytes / 4);
        read_node(store, root, cap)?.check_key(key)?;
        return Ok(Ready {
            root,
            computed: 0,
            peak_working_bytes: cap * 4,
            scratch_bytes: 0,
        });
    }
    let work = Work::new(limits)?;
    let mut steps = 0;
    step(&mut steps, limits)?;
    let input = read_input(source, state, limits)?;
    let mut initial = 1024 * 1024;
    charge(&mut initial, input.encoded_len().saturating_mul(4), limits)?;
    work.push(0, &input)?;
    drop(input);
    let mut depth = 0;
    let mut computed = 0;
    let mut peak = initial;
    loop {
        step(&mut steps, limits)?;
        let (input, next) = work.top(depth)?;
        let mut used = 1024 * 1024;
        charge(&mut used, input.encoded_len().saturating_mul(4), limits)?;
        if let Some(parent) = input.parents.get(next).copied() {
            work.advance(depth)?;
            if let Some(root) = store.lookup(descriptor.key(parent))? {
                let cap = limits
                    .max_node_bytes
                    .min((limits.max_working_bytes - used) / 4);
                let node = read_node(store, root, cap)?;
                node.check_key(descriptor.key(parent))?;
                charge(&mut used, node.encoded_len().saturating_mul(4), limits)?;
            } else {
                let parent_input = read_input(source, parent, limits)?;
                charge(
                    &mut used,
                    parent_input.encoded_len().saturating_mul(4),
                    limits,
                )?;
                depth = depth
                    .checked_add(1)
                    .ok_or(DerivedError::Limit("work depth"))?;
                work.push(depth, &parent_input)?;
            }
            peak = peak.max(used);
            continue;
        }
        let mut parents = Vec::with_capacity(input.parents.len());
        for parent in &input.parents {
            step(&mut steps, limits)?;
            let cap = limits
                .max_node_bytes
                .min((limits.max_working_bytes - used) / 4);
            let node = load_node(store, descriptor.key(*parent), cap)?;
            charge(&mut used, node.encoded_len().saturating_mul(4), limits)?;
            parents.push(node);
        }
        let handles: Vec<_> = parents
            .iter()
            .map(|node| {
                Ok(ParentValue {
                    key: node.key,
                    root: node.hash()?,
                })
            })
            .collect::<DerivedResult<_>>()?;
        charge(
            &mut used,
            handles
                .len()
                .saturating_mul(100)
                .saturating_mul(4)
                .saturating_add(512),
            limits,
        )?;
        let header = 77usize.saturating_add(handles.len().saturating_mul(100));
        let output_cap = limits
            .max_node_bytes
            .checked_sub(header)
            .ok_or(DerivedError::Limit("node bytes"))?
            .min((limits.max_working_bytes - used) / 4);
        let value = derivation.compose(&input, &parents, output_cap)?;
        if value.len() > output_cap {
            return Err(DerivedError::Limit("result bytes"));
        }
        charge(&mut used, value.len().saturating_mul(4), limits)?;
        peak = peak.max(used);
        let node = DerivedNode {
            key: descriptor.key(input.state),
            parents: handles,
            value,
        };
        let bytes = node.encode()?;
        let root = node_hash(&bytes);
        store.put_node_if_absent(root, &bytes)?;
        store.bind_if_absent(node.key, root)?;
        computed += 1;
        work.pop(depth)?;
        if depth == 0 {
            return Ok(Ready {
                root,
                computed,
                peak_working_bytes: peak,
                scratch_bytes: work.bytes()?,
            });
        }
        depth -= 1;
    }
}

/// Budgeted exact state membership over shared closure nodes. An exhausted
/// budget or a missing dependency is an error, never `false`. The visited set
/// is disk-backed too, so diamonds are deduplicated without retaining history
/// in executor memory. `true` is an exact path witness; `false` exhausts the DAG.
pub fn closure_contains(
    store: &impl DerivedStore,
    root: ContentHash,
    state: StateId,
    limits: Limits,
) -> DerivedResult<bool> {
    let work = Work::new(limits)?;
    work.visit(root, None)?;
    let descriptor = StateClosure.descriptor();
    let mut steps = 0;
    while let Some((hash, expected)) = work.next_visit()? {
        step(&mut steps, limits)?;
        let cap = limits
            .max_node_bytes
            .min(limits.max_working_bytes.saturating_sub(1024 * 1024) / 4);
        let node = read_node(store, hash, cap)?;
        node.check_key(descriptor.key(node.key.state))?;
        if let Some(key) = expected {
            node.check_key(key)?;
        }
        if node.key.state == state {
            return Ok(true);
        }
        if node.parents.len() > limits.max_parents {
            return Err(DerivedError::Limit("parents"));
        }
        for parent in &node.parents {
            work.visit(parent.root, Some(parent.key))?;
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
