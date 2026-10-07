//! Converted Git ancestry of HYBRID import tips (api alpha.42 `ImportAncestryPage`).
//!
//! An import is one signed native operation per branch; its Git ancestors are
//! States in the tip State's parent closure, not operations. The signed
//! operation commits to the tip Capture, the tip commits to its parent
//! StateIds and every member to its own, so the floor is a Merkle closure
//! rooted at the signed tip. This module checks exactly that: addresses, reach
//! from the tip, completeness down to the signed frontier, and page framing.
//! It grants nothing; the caller already authenticated the carrier.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use heddle_object_model::object::{State, StateId};

use super::Error;
use crate::contract::{ImportAncestryPage, ThreadRef, import_ancestry_page::Coverage};

/// One carried delegated import operation whose floor pages must verify.
pub(super) struct ImportFloorInput {
    pub digest: Vec<u8>,
    pub tip: State,
    /// Source States of the operation's causal parents: the signed frontier.
    pub frontier: BTreeSet<StateId>,
}

pub(super) struct VerifiedFloor {
    pub tip: StateId,
    pub coverage: Coverage,
    pub members: BTreeSet<StateId>,
}

#[derive(Default)]
pub(super) struct VerifiedAncestry {
    /// Unique address-checked canonical States to install, in no particular order.
    pub states: BTreeMap<StateId, Vec<u8>>,
    pub floors: Vec<VerifiedFloor>,
}

#[derive(Default)]
pub(super) struct AncestryInput {
    pub pages: Vec<ImportAncestryPage>,
    /// Import tips whose floor the client told the endpoint it already holds.
    pub excluded_tips: BTreeSet<StateId>,
}

fn state_id(bytes: &[u8]) -> Result<StateId, Error> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::Invalid("import ancestry identity width"))?;
    Ok(StateId::from_bytes(bytes))
}

struct PageSet<'a> {
    coverage: Coverage,
    pages: Vec<&'a ImportAncestryPage>,
}

/// Group pages by (tip, signed operation) and check the paging contract:
/// identical headers, contiguous indexes, declared member count.
fn page_sets<'a>(
    pages: &'a [ImportAncestryPage],
    thread: &ThreadRef,
) -> Result<BTreeMap<(StateId, Vec<u8>), PageSet<'a>>, Error> {
    let mut sets: BTreeMap<(StateId, Vec<u8>), Vec<&ImportAncestryPage>> = BTreeMap::new();
    for page in pages {
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
        sets.entry((tip, page.signed_operation_digest.clone()))
            .or_default()
            .push(page);
    }
    let mut verified = BTreeMap::new();
    for (key, mut pages) in sets {
        pages.sort_by_key(|page| page.page_index);
        let first = pages[0];
        let coverage = Coverage::try_from(first.coverage)
            .ok()
            .filter(|c| matches!(c, Coverage::Floor | Coverage::Path))
            .ok_or(Error::Invalid("import ancestry coverage unspecified"))?;
        if pages.len() != first.page_count as usize {
            return Err(Error::Invalid("import ancestry page set incomplete"));
        }
        let mut carried = 0usize;
        for (index, page) in pages.iter().enumerate() {
            if page.page_index as usize != index
                || page.page_count != first.page_count
                || page.member_count != first.member_count
                || page.coverage != first.coverage
            {
                return Err(Error::Invalid(
                    "import ancestry pages disagree about their floor",
                ));
            }
            carried = carried
                .checked_add(page.states.len())
                .ok_or(Error::Invalid("import ancestry count overflow"))?;
        }
        if carried != first.member_count as usize {
            return Err(Error::Invalid(
                "import ancestry member count differs from carried States",
            ));
        }
        verified.insert(key, PageSet { coverage, pages });
    }
    Ok(verified)
}

/// Verify every carried floor against the carried import operations.
///
/// `selected` is the Fetch's selected revision when it is not an operation's
/// State; it must then be proved inside a floor. `excluded_tips` are floors the
/// client declared it holds, so an absent page set is acceptable for them.
pub(super) fn verify(
    input: &AncestryInput,
    floors: &[ImportFloorInput],
    thread: &ThreadRef,
    selected: Option<StateId>,
) -> Result<VerifiedAncestry, Error> {
    let mut sets = page_sets(&input.pages, thread)?;
    let seed =
        heddle_object_model::object::thread_replication::initial_base::synthetic_initial_base()
            .map_err(|_| Error::Invalid("synthetic base unavailable"))?
            .id();
    let mut verified = VerifiedAncestry::default();
    let mut selected_found = false;
    for floor in floors {
        let tip = floor.tip.id();
        let required: BTreeSet<StateId> = floor
            .tip
            .parents
            .iter()
            .copied()
            .filter(|parent| !floor.frontier.contains(parent))
            .collect();
        let Some(set) = sets.remove(&(tip, floor.digest.clone())) else {
            if required.is_empty() || input.excluded_tips.contains(&tip) {
                continue;
            }
            return Err(Error::Invalid(
                "import ancestry absent for a converted Git history",
            ));
        };
        if required.is_empty() {
            return Err(Error::Invalid(
                "import ancestry carried for an import without converted ancestors",
            ));
        }
        let mut carried: BTreeMap<StateId, (State, &[u8])> = BTreeMap::new();
        for page in &set.pages {
            for ancestor in &page.states {
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
                    || state
                        .encode_current_msgpack()
                        .map_err(|_| Error::Invalid("import ancestor State is not canonical"))?
                        != ancestor.canonical_state
                {
                    return Err(Error::Invalid(
                        "import ancestor State differs from its address",
                    ));
                }
                if id == tip || floor.frontier.contains(&id) || id == seed {
                    return Err(Error::Invalid(
                        "import ancestry carries the tip, frontier or genesis base",
                    ));
                }
                if carried
                    .insert(id, (state, ancestor.canonical_state.as_slice()))
                    .is_some()
                {
                    return Err(Error::Invalid("duplicate import ancestor State"));
                }
            }
        }
        // Reach from the signed tip. Every carried State must be reached, and
        // under FLOOR coverage every referenced parent must be carried.
        let mut reached = BTreeSet::new();
        let mut pending: VecDeque<StateId> = required.iter().copied().collect();
        while let Some(id) = pending.pop_front() {
            if !reached.insert(id) {
                continue;
            }
            let Some((state, _)) = carried.get(&id) else {
                if set.coverage == Coverage::Floor {
                    return Err(Error::Invalid(
                        "import ancestry is incomplete below the tip",
                    ));
                }
                reached.remove(&id);
                continue;
            };
            for parent in &state.parents {
                if *parent == seed || *parent == tip {
                    return Err(Error::Invalid(
                        "import ancestor names the genesis base or its own tip",
                    ));
                }
                if floor.frontier.contains(parent) {
                    continue;
                }
                pending.push_back(*parent);
            }
        }
        if reached.len() != carried.len() {
            return Err(Error::Invalid(
                "import ancestry carries a State outside the signed floor",
            ));
        }
        if selected.is_some_and(|id| carried.contains_key(&id)) {
            selected_found = true;
        } else if set.coverage == Coverage::Path {
            return Err(Error::Invalid(
                "import ancestry path does not reach the selected revision",
            ));
        }
        for (id, (_, bytes)) in carried {
            verified.states.entry(id).or_insert_with(|| bytes.to_vec());
        }
        verified.floors.push(VerifiedFloor {
            tip,
            coverage: set.coverage,
            members: reached,
        });
    }
    if !sets.is_empty() {
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

/// Locate the floor whose pages carry `selected`, before any operation is
/// bound: its tip selects the operation whose page set must then prove it.
pub(super) fn selected_page_tip(
    pages: &[ImportAncestryPage],
    selected: StateId,
) -> Result<Option<(StateId, Vec<u8>)>, Error> {
    let mut found = None;
    for page in pages {
        for ancestor in &page.states {
            let id = state_id(
                &ancestor
                    .id
                    .as_ref()
                    .ok_or(Error::Invalid("import ancestor identity absent"))?
                    .value,
            )?;
            if id != selected {
                continue;
            }
            let tip = state_id(
                &page
                    .tip
                    .as_ref()
                    .ok_or(Error::Invalid("import ancestry tip absent"))?
                    .value,
            )?;
            let candidate = (tip, ancestor.canonical_state.clone());
            if found.replace(candidate).is_some() {
                return Err(Error::Invalid("ambiguous selected import ancestor"));
            }
        }
    }
    Ok(found)
}
