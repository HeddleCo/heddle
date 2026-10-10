// SPDX-License-Identifier: Apache-2.0
//! Exact ordinary capture ancestry and a complete-history decoded-read budget.
use super::{GitProjectionResult, HistorySelection, failure};
use crypto::thread_operation::SignedOperation;
use objects::{
    error::HeddleError,
    object::{
        Blob, ContentHash, ObjectSource, State, StateId, Tree,
        source_target::capture::ReferenceProof,
        thread_replication::{ThreadGenesis, ThreadOperation, ThreadOperationBody},
    },
};
use prost::Message;
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};
use thread_api::{
    contract::{ThreadGenesisRecord, ThreadRef},
    publication::PublicationOriginals,
};

pub(super) struct Selection {
    genesis: ThreadGenesis,
    record: ThreadGenesisRecord,
    operations: BTreeMap<ContentHash, (SignedOperation, ThreadOperation)>,
    states: BTreeMap<StateId, ContentHash>,
}
impl Selection {
    pub fn new(
        input: &HistorySelection<'_>,
        reference: &ThreadRef,
        limit: usize,
        metadata_limit: usize,
    ) -> GitProjectionResult<Self> {
        if input.originals.is_empty()
            || input.originals.len() > limit
            || !input.genesis.ownership_claims.is_empty()
            || !input.genesis.ownership_resolutions.is_empty()
            || input.genesis.admission.is_some()
            || input.genesis.native_genesis_authority.is_some()
            || !input.genesis.boundary_acceptances.is_empty()
        {
            return Err(failure(
                "bounded ordinary capture history required; retained hosted/ownership evidence needs its own adapter",
            ));
        }
        let mut metadata = input.genesis.encoded_len();
        if metadata > 256 * 1024 {
            return Err(failure("genesis metadata limit"));
        }
        for signed in input.originals {
            if signed.canonical.len() > 256 * 1024 || signed.signature.len() != 64 {
                return Err(failure("bounded signed source original required"));
            }
            metadata = metadata
                .checked_add(signed.canonical.len() + signed.signature.len() + 512)
                .ok_or_else(|| failure("history metadata overflow"))?;
            if metadata > metadata_limit {
                return Err(failure("complete history input metadata limit"));
            }
        }
        let genesis =
            thread_api::replication::opening::verify_genesis_record(&input.genesis, reference)
                .map_err(failure)?;
        let thread = genesis.id().map_err(failure)?;
        let mut operations = BTreeMap::new();
        let mut states = BTreeMap::new();
        for signed in input.originals {
            let operation = signed.verify().map_err(failure)?;
            if operation.thread != thread
                || !matches!(operation.body, ThreadOperationBody::Capture(_))
            {
                return Err(failure("same-Thread ordinary capture history required"));
            }
            let id = operation.id().map_err(failure)?;
            let state = operation
                .source_state()
                .map_err(failure)?
                .ok_or_else(|| failure("source State absent"))?;
            if states.insert(state.id(), id).is_some()
                || operations.insert(id, (signed.clone(), operation)).is_some()
            {
                return Err(failure("duplicate or ambiguous source original"));
            }
        }
        let value = Self {
            genesis,
            record: input.genesis.clone(),
            operations,
            states,
        };
        let selected = value.closure(input.tip)?;
        if selected.len() != value.operations.len() {
            return Err(failure("unselected source originals"));
        }
        for (_, operation) in value.operations.values() {
            let parents = operation
                .parents
                .iter()
                .map(|id| {
                    value
                        .operations
                        .get(id)
                        .map(|(_, operation)| operation.clone())
                        .ok_or_else(|| failure("missing source ancestor"))
                })
                .collect::<GitProjectionResult<Vec<_>>>()?;
            operation
                .validate_parents(&value.genesis, &parents)
                .map_err(failure)?;
        }
        Ok(value)
    }
    pub fn states(&self) -> GitProjectionResult<Vec<StateId>> {
        // Source hashes are not chronology. The selected operation's entire closure must
        // precede it, and native State parents must also be present before each child.
        let mut remaining = self.states.clone();
        let mut emitted_operations = BTreeSet::new();
        let mut emitted_states = BTreeSet::new();
        let mut ordered = Vec::new();
        while !remaining.is_empty() {
            let mut ready = None;
            for (state, id) in &remaining {
                let (_, operation) = self
                    .operations
                    .get(id)
                    .ok_or_else(|| failure("source original absent"))?;
                let native = operation
                    .source_state()
                    .map_err(failure)?
                    .ok_or_else(|| failure("source State absent"))?;
                if operation
                    .parents
                    .iter()
                    .all(|p| emitted_operations.contains(p))
                    && native
                        .parents
                        .iter()
                        .filter(|p| self.states.contains_key(p))
                        .all(|p| emitted_states.contains(p))
                {
                    ready = Some((*state, *id));
                    break;
                }
            }
            let (state, id) = ready.ok_or_else(|| failure("cyclic native publication history"))?;
            remaining.remove(&state);
            emitted_states.insert(state);
            emitted_operations.insert(id);
            ordered.push(state);
        }
        Ok(ordered)
    }
    fn closure(&self, state: StateId) -> GitProjectionResult<BTreeSet<ContentHash>> {
        let id = self
            .states
            .get(&state)
            .ok_or_else(|| failure("selected State has no exact original"))?;
        let mut pending = vec![*id];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                let (_, op) = self
                    .operations
                    .get(&id)
                    .ok_or_else(|| failure("missing source ancestor"))?;
                pending.extend(&op.parents);
            }
        }
        Ok(seen)
    }
    pub fn revision(
        &self,
        id: StateId,
        source: &impl ObjectSource,
        metadata_remaining: &mut usize,
    ) -> GitProjectionResult<(State, PublicationOriginals, Vec<ReferenceProof>)> {
        let selected = self
            .states
            .get(&id)
            .ok_or_else(|| failure("selected State absent"))?;
        let (_, op) = self
            .operations
            .get(selected)
            .ok_or_else(|| failure("selected original absent"))?;
        let state = op
            .source_state()
            .map_err(failure)?
            .ok_or_else(|| failure("capture absent"))?;
        let stored = source
            .get_state(&id)?
            .ok_or_else(|| failure("historical State is not hydrated"))?;
        if state.encode_current_msgpack()? != stored.encode_current_msgpack()? {
            return Err(failure("historical State differs from exact signed bytes"));
        }
        let closure = self.closure(id)?;
        let mut bytes = self.record.encoded_len() + 1024;
        for id in &closure {
            let (signed, _) = self
                .operations
                .get(id)
                .ok_or_else(|| failure("ancestor absent"))?;
            bytes = bytes
                .checked_add(signed.canonical.len() + signed.signature.len() + 512)
                .ok_or_else(|| failure("history metadata overflow"))?;
        }
        *metadata_remaining = metadata_remaining
            .checked_sub(bytes)
            .ok_or_else(|| failure("complete history proposal metadata limit"))?;
        let mut references = Vec::new();
        let mut originals = Vec::new();
        for ancestor in closure {
            let (signed, operation) = self
                .operations
                .get(&ancestor)
                .ok_or_else(|| failure("ancestor absent"))?;
            if let Some(reference) = operation.reference_proof(&self.genesis).map_err(failure)? {
                references.push(reference);
            }
            originals.push(signed.clone().into());
        }
        let operations = thread_api::authority_admission::batches(originals, 256 * 1024, 128)
            .map_err(failure)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failure)?;
        Ok((
            state,
            PublicationOriginals {
                geneses: vec![self.record.clone()],
                operations,
            },
            references,
        ))
    }
}

pub(super) struct CountedSource<'a, S> {
    source: &'a S,
    remaining: Cell<u64>,
}
impl<'a, S> CountedSource<'a, S> {
    pub fn new(source: &'a S, budget: u64) -> Self {
        Self {
            source,
            remaining: Cell::new(budget),
        }
    }
    pub fn remaining(&self) -> u64 {
        self.remaining.get()
    }
    fn charge(&self, bytes: usize) -> objects::error::Result<()> {
        let next = self
            .remaining
            .get()
            .checked_sub(bytes as u64)
            .ok_or_else(|| {
                HeddleError::InvalidObject("complete history decoded byte limit".into())
            })?;
        self.remaining.set(next);
        Ok(())
    }
}
impl<S: ObjectSource> ObjectSource for CountedSource<'_, S> {
    fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
        let value = self.source.get_tree(hash)?;
        if let Some(value) = &value {
            self.charge(value.encode_canonical()?.len())?;
        }
        Ok(value)
    }
    fn get_state(&self, id: &StateId) -> objects::error::Result<Option<State>> {
        let value = self.source.get_state(id)?;
        if let Some(value) = &value {
            self.charge(value.encode_current_msgpack()?.len())?;
        }
        Ok(value)
    }
    fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
        let value = self.source.get_blob(hash)?;
        if let Some(value) = &value {
            self.charge(value.content().len())?;
        }
        Ok(value)
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> objects::error::Result<Option<u64>> {
        let size = self.source.decoded_blob_len(hash)?;
        if size.is_some_and(|size| size > self.remaining.get()) {
            return Err(HeddleError::InvalidObject(
                "complete history decoded byte limit".into(),
            ));
        }
        Ok(size)
    }
}
