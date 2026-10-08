//! Address-check pages into a temporary pack as they arrive. Only parent edges
//! and page headers remain in memory; signed closure checks run after Complete.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::Path,
};

use heddle_object_model::object::{State, StateId};
use heddle_pack::store::pack::{ObjectType, PackObjectId, PackReader, StreamingPackBuilder};

use super::Error;
#[cfg(feature = "native")]
use crate::contract::ImportFloorTierSummary;
use crate::contract::{ImportAncestryPage, ThreadRef, import_ancestry_page::Coverage};

pub(super) struct ImportFloorInput {
    pub digest: Vec<u8>,
    pub tip: State,
    pub frontier: BTreeSet<StateId>,
}
pub(super) struct VerifiedFloor {
    pub tip: StateId,
    pub coverage: Coverage,
    pub members: BTreeSet<StateId>,
    #[cfg(feature = "native")]
    pub tiers: ImportFloorTierSummary,
}
#[derive(Default)]
pub(super) struct VerifiedAncestry {
    pub floors: Vec<VerifiedFloor>,
}

struct PageSet {
    header: ImportAncestryPage,
    indexes: BTreeSet<u32>,
    parents: HashMap<StateId, Box<[StateId]>>,
}
#[derive(Default)]
pub(super) struct AncestryInput {
    sets: BTreeMap<(StateId, Vec<u8>), PageSet>,
    pub excluded_tips: BTreeSet<StateId>,
    builder: Option<StreamingPackBuilder<std::fs::File>>,
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}
fn state_id(bytes: &[u8]) -> Result<StateId, Error> {
    Ok(StateId::from_bytes(bytes.try_into().map_err(|_| {
        Error::Invalid("import ancestry identity width")
    })?))
}
impl AncestryInput {
    pub fn new(excluded_tips: BTreeSet<StateId>) -> Self {
        Self {
            excluded_tips,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
    pub fn push(
        &mut self,
        mut page: ImportAncestryPage,
        thread: &ThreadRef,
        directory: &Path,
    ) -> Result<(), Error> {
        if page.thread.as_ref() != Some(thread) {
            return Err(Error::Invalid("import ancestry crosses Thread"));
        }
        let tip = state_id(
            &page
                .tip
                .as_ref()
                .ok_or(Error::Invalid("import ancestry tip absent"))?
                .value,
        )?;
        if !matches!(
            Coverage::try_from(page.coverage),
            Ok(Coverage::Floor | Coverage::Path)
        ) {
            return Err(Error::Invalid("import ancestry coverage unspecified"));
        }
        if page.page_count == 0
            || page.page_index >= page.page_count
            || page.states.is_empty()
            || page.states.len() > super::ANCESTRY_PAGE_STATES
            || page.member_count as usize > super::ANCESTRY_STATES
        {
            return Err(Error::Invalid("import ancestry page bounds"));
        }
        let summary = page
            .floor_tiers
            .as_ref()
            .ok_or(Error::Invalid("import floor tier summary absent"))?;
        let mut previous = None;
        for row in &summary.rows {
            let key = (row.tier, row.label.as_str());
            if !matches!(row.tier, 1 | 2)
                || row.tier_rows == 0
                || row.tier_rows > i64::MAX as u64
                || previous.is_some_and(|p| p >= key)
            {
                return Err(Error::Invalid("invalid import floor tier summary"));
            }
            previous = Some(key);
        }
        let states = std::mem::take(&mut page.states);
        let key = (tip, page.signed_operation_digest.clone());
        if let Some(set) = self.sets.get(&key) {
            let mut header = page.clone();
            header.page_index = set.header.page_index;
            if header != set.header || set.indexes.contains(&page.page_index) {
                return Err(Error::Invalid(
                    "import ancestry pages disagree about their floor",
                ));
            }
        }
        if self.builder.is_none() {
            let output = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(directory.join("ancestry.pack"))?;
            self.builder = Some(
                StreamingPackBuilder::new(
                    output,
                    directory.join("ancestry.idx"),
                    Default::default(),
                    directory.join("ancestry-buckets"),
                )
                .map_err(preparation)?,
            );
        }
        // Check duplicates across floors before adding to the shared pack.
        for ancestor in states {
            let id = state_id(
                &ancestor
                    .id
                    .as_ref()
                    .ok_or(Error::Invalid("import ancestor identity absent"))?
                    .value,
            )?;
            let state = State::decode_current_msgpack(&ancestor.canonical_state)
                .map_err(|_| Error::Invalid("import ancestor State is not canonical"))?;
            if state.id() != id
                || state.encode_current_msgpack().map_err(preparation)? != ancestor.canonical_state
            {
                return Err(Error::Invalid(
                    "import ancestor State differs from its address",
                ));
            }
            let shared = self.sets.values().any(|set| set.parents.contains_key(&id));
            let set = self.sets.entry(key.clone()).or_insert_with(|| PageSet {
                header: page.clone(),
                indexes: BTreeSet::new(),
                parents: HashMap::new(),
            });
            if set
                .parents
                .insert(id, state.parents.into_boxed_slice())
                .is_some()
            {
                return Err(Error::Invalid("duplicate import ancestor State"));
            }
            if !shared {
                self.builder
                    .as_mut()
                    .ok_or(Error::Invalid("ancestry writer absent"))?
                    .add_id(
                        PackObjectId::StateId(id),
                        ObjectType::State,
                        &ancestor.canonical_state,
                    )
                    .map_err(preparation)?;
            }
        }
        self.sets
            .get_mut(&key)
            .ok_or(Error::Invalid("ancestry page absent"))?
            .indexes
            .insert(page.page_index);
        Ok(())
    }
    pub fn finish(&mut self) -> Result<(), Error> {
        for set in self.sets.values() {
            if set.indexes.len() != set.header.page_count as usize
                || set.parents.len() != set.header.member_count as usize
            {
                return Err(Error::Invalid(
                    "import ancestry page set incomplete or member count differs from carried States",
                ));
            }
        }
        if let Some(builder) = self.builder.take() {
            let (output, _) = builder.finalize().map_err(preparation)?;
            drop(output);
        }
        Ok(())
    }
    pub fn selected_page_tip(
        &self,
        selected: StateId,
        directory: &Path,
    ) -> Result<Option<(StateId, Vec<u8>)>, Error> {
        let Some((tip, _)) = self
            .sets
            .iter()
            .find(|(_, set)| set.parents.contains_key(&selected))
            .map(|(key, _)| key)
        else {
            return Ok(None);
        };
        let reader = PackReader::open(
            &directory.join("ancestry.pack"),
            &directory.join("ancestry.idx"),
            directory,
        )
        .map_err(preparation)?;
        let bytes = reader
            .get_object(&PackObjectId::StateId(selected))
            .map_err(preparation)?
            .ok_or(Error::Invalid("selected import ancestor absent"))?;
        Ok(Some((*tip, bytes.1)))
    }
    #[cfg(all(test, feature = "native"))]
    pub fn retained_allocations(&self) -> usize {
        self.sets
            .values()
            .map(|s| {
                s.parents.capacity()
                    * (std::mem::size_of::<StateId>() + std::mem::size_of::<Box<[StateId]>>() + 1)
                    + s.parents
                        .values()
                        .map(|p| p.len() * std::mem::size_of::<StateId>())
                        .sum::<usize>()
            })
            .sum()
    }
}
pub(super) fn verify(
    input: &AncestryInput,
    floors: &[ImportFloorInput],
    thread: &ThreadRef,
    selected: Option<StateId>,
    require_import_ancestry: bool,
) -> Result<VerifiedAncestry, Error> {
    let seed =
        heddle_object_model::object::thread_replication::initial_base::synthetic_initial_base()
            .map_err(preparation)?
            .id();
    let mut verified = VerifiedAncestry::default();
    let mut used = BTreeSet::new();
    let mut selected_found = false;
    for floor in floors {
        let tip = floor.tip.id();
        let required: BTreeSet<_> = floor
            .tip
            .parents
            .iter()
            .copied()
            .filter(|p| !floor.frontier.contains(p))
            .collect();
        let key = (tip, floor.digest.clone());
        let Some(set) = input.sets.get(&key) else {
            if !require_import_ancestry || required.is_empty() || input.excluded_tips.contains(&tip)
            {
                continue;
            }
            return Err(Error::Invalid(
                "import ancestry absent for a converted Git history",
            ));
        };
        used.insert(key);
        if set.header.thread.as_ref() != Some(thread) {
            return Err(Error::Invalid("import ancestry crosses Thread"));
        }
        if required.is_empty() {
            return Err(Error::Invalid(
                "import ancestry carried for an import without converted ancestors",
            ));
        }
        let coverage = Coverage::try_from(set.header.coverage)
            .map_err(|_| Error::Invalid("import ancestry coverage unspecified"))?;
        if set
            .parents
            .keys()
            .any(|id| *id == tip || *id == seed || floor.frontier.contains(id))
        {
            return Err(Error::Invalid(
                "import ancestry carries the tip, frontier or genesis base",
            ));
        }
        let mut reached = BTreeSet::new();
        let mut pending: VecDeque<_> = required.into_iter().collect();
        while let Some(id) = pending.pop_front() {
            if !reached.insert(id) {
                continue;
            }
            if id == tip || id == seed || floor.frontier.contains(&id) {
                return Err(Error::Invalid(
                    "import ancestry carries the tip, frontier or genesis base",
                ));
            }
            let Some(parents) = set.parents.get(&id) else {
                if coverage == Coverage::Floor {
                    return Err(Error::Invalid(
                        "import ancestry is incomplete below the tip",
                    ));
                }
                reached.remove(&id);
                continue;
            };
            for parent in parents.iter() {
                if *parent == seed || *parent == tip {
                    return Err(Error::Invalid(
                        "import ancestor names the genesis base or its own tip",
                    ));
                }
                if !floor.frontier.contains(parent) {
                    pending.push_back(*parent);
                }
            }
        }
        if reached.len() != set.parents.len() {
            return Err(Error::Invalid(
                "import ancestry carries a State outside the signed floor",
            ));
        }
        if selected.is_some_and(|id| set.parents.contains_key(&id)) {
            selected_found = true;
        } else if coverage == Coverage::Path {
            return Err(Error::Invalid(
                "import ancestry path does not reach the selected revision",
            ));
        }
        let _tiers = set
            .header
            .floor_tiers
            .clone()
            .ok_or(Error::Invalid("import floor tier summary absent"))?;
        verified.floors.push(VerifiedFloor {
            tip,
            coverage,
            members: reached,
            #[cfg(feature = "native")]
            tiers: _tiers,
        });
    }
    if used.len() != input.sets.len() {
        return Err(Error::Invalid(
            "import ancestry names an operation outside the selected ancestry",
        ));
    }
    if selected.is_some() && !selected_found {
        return Err(Error::Invalid(
            "selected revision is not proved inside an import floor",
        ));
    }
    Ok(verified)
}
