// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};

use super::{
    Result,
    dictionary::{AgentKey, PrincipalKey},
    invalid,
    io::Reader,
    limits::{
        MAX_COMPACT_STATE_COUNT, MIN_AGENT_BYTES, MIN_EXTRA_HEADER_BYTES, MIN_LINEAGE_BYTES,
        MIN_PRINCIPAL_BYTES, MIN_STATE_COLUMN_BYTES, MIN_STATE_PARENT_BYTES,
        MIN_VERIFICATION_CUSTOM_BYTES, admit_count,
    },
    state::STATE_MAGIC,
};
use crate::object::{
    Agent, Attribution, ChangeId, ChangeLineage, ChangeLineageKind, ContentHash, Principal, State,
    StateId, Status, Verification,
};

/// Decode and whole-frame-verify every state, recomputing each state id.
pub fn decode_state_frame(bytes: &[u8]) -> Result<Vec<State>> {
    let mut input = Reader::verified(bytes, STATE_MAGIC)?;
    let count = input.get_count_at_most("state frame", 1, MAX_COMPACT_STATE_COUNT)?;
    let (principals, agents) = decode_dictionaries(&mut input)?;
    admit_count(
        "state frame",
        count,
        input.remaining(),
        MIN_STATE_COLUMN_BYTES,
        MAX_COMPACT_STATE_COUNT,
    )?;
    let mut states = (0..count).map(|_| blank_state()).collect::<Vec<_>>();
    decode_structure(&mut input, &mut states)?;
    decode_attribution(&mut input, &mut states, &principals, &agents)?;
    decode_intent_and_verification(&mut input, &mut states)?;
    decode_timestamps(&mut input, &mut states)?;
    decode_fidelity(&mut input, &mut states, &principals)?;
    decode_lineage(&mut input, &mut states)?;
    input.finish()?;
    for state in &mut states {
        state.state_id = state.id();
    }
    Ok(states)
}

fn decode_structure(input: &mut Reader<'_>, states: &mut [State]) -> Result<()> {
    for state in &mut *states {
        state.change_id = ChangeId::from_bytes(input.get_fixed()?);
    }
    for state in &mut *states {
        state.tree = ContentHash::from_bytes(input.get_fixed()?);
    }
    for state in states {
        let count = input.get_count("state parent", MIN_STATE_PARENT_BYTES)?;
        state.parents = (0..count)
            .map(|_| Ok(StateId::from_bytes(input.get_fixed()?)))
            .collect::<Result<Vec<_>>>()?;
    }
    Ok(())
}

fn decode_attribution(
    input: &mut Reader<'_>,
    states: &mut [State],
    principals: &[Principal],
    agents: &[Agent],
) -> Result<()> {
    for state in &mut *states {
        state.attribution.principal = principal_at(principals, input.get_u64()?)?;
    }
    for state in states {
        state.attribution.agent = optional_at(agents, input.get_u64()?)?;
    }
    Ok(())
}

fn decode_intent_and_verification(input: &mut Reader<'_>, states: &mut [State]) -> Result<()> {
    for state in &mut *states {
        state.intent = input
            .get_optional_bytes()?
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| invalid("state intent is not UTF-8"))?;
    }
    for state in &mut *states {
        state.confidence = get_optional_f32(input)?;
    }
    for state in &mut *states {
        state.verification = decode_verification(input)?;
    }
    for state in states {
        state.status =
            Status::from_byte(input.get_u8()?).ok_or_else(|| invalid("invalid state status"))?;
    }
    Ok(())
}

fn decode_timestamps(input: &mut Reader<'_>, states: &mut [State]) -> Result<()> {
    let mut previous = 0i64;
    for (index, state) in states.iter_mut().enumerate() {
        let encoded = input.get_i64()?;
        let seconds = if index == 0 {
            encoded
        } else {
            previous
                .checked_add(encoded)
                .ok_or_else(|| invalid("created timestamp delta overflow"))?
        };
        state.created_at = timestamp(seconds, get_u32(input, "created timestamp nanos")?)?;
        previous = seconds;
    }
    for state in &mut *states {
        state.authored_at = match input.get_u8()? {
            0 => None,
            1 => {
                let seconds = state
                    .created_at
                    .timestamp()
                    .checked_add(input.get_i64()?)
                    .ok_or_else(|| invalid("authored timestamp delta overflow"))?;
                Some(timestamp(
                    seconds,
                    get_u32(input, "authored timestamp nanos")?,
                )?)
            }
            value => return Err(invalid(format!("invalid authored timestamp tag {value}"))),
        };
    }
    for state in states {
        state.authored_tz_offset = get_i32(input, "author timezone")?;
        state.committer_tz_offset = get_i32(input, "committer timezone")?;
    }
    Ok(())
}

fn decode_fidelity(
    input: &mut Reader<'_>,
    states: &mut [State],
    principals: &[Principal],
) -> Result<()> {
    for state in &mut *states {
        state.provenance = match input.get_u8()? {
            0 => None,
            1 => Some(ContentHash::from_bytes(input.get_fixed()?)),
            value => return Err(invalid(format!("invalid provenance option tag {value}"))),
        };
    }
    for state in &mut *states {
        state.committer = optional_at(principals, input.get_u64()?)?;
    }
    for state in &mut *states {
        state.raw_message = input.get_optional_bytes()?;
    }
    for state in &mut *states {
        let count = input.get_count("extra header", MIN_EXTRA_HEADER_BYTES)?;
        state.extra_headers = (0..count)
            .map(|_| Ok((input.get_bytes()?, input.get_bytes()?)))
            .collect::<Result<Vec<_>>>()?;
    }
    for state in states {
        state.git_lossy = input.get_bool()?;
    }
    Ok(())
}

fn decode_lineage(input: &mut Reader<'_>, states: &mut [State]) -> Result<()> {
    for state in states {
        let count = input.get_count("state lineage", MIN_LINEAGE_BYTES)?;
        state.lineage = (0..count)
            .map(|_| {
                Ok(ChangeLineage {
                    kind: decode_lineage_kind(input.get_u8()?)?,
                    source_change: ChangeId::from_bytes(input.get_fixed()?),
                    source_state: StateId::from_bytes(input.get_fixed()?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
    }
    Ok(())
}

fn decode_verification(input: &mut Reader<'_>) -> Result<Option<Verification>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => {
            let tests_passed = get_optional_bool(input)?;
            let tests_failed = get_optional_u32(input)?;
            let coverage_pct = get_optional_f32(input)?;
            let coverage_delta = get_optional_f32(input)?;
            let lint_warnings = get_optional_u32(input)?;
            let count = input.get_count("verification custom", MIN_VERIFICATION_CUSTOM_BYTES)?;
            let mut custom = BTreeMap::new();
            for _ in 0..count {
                let key = String::from_utf8(input.get_bytes()?)
                    .map_err(|_| invalid("verification key is not UTF-8"))?;
                custom.insert(key, rmp_serde::from_slice(&input.get_bytes()?)?);
            }
            Ok(Some(Verification {
                tests_passed,
                tests_failed,
                coverage_pct,
                coverage_delta,
                lint_warnings,
                custom,
            }))
        }
        value => Err(invalid(format!("invalid verification option tag {value}"))),
    }
}

fn decode_dictionaries(input: &mut Reader<'_>) -> Result<(Vec<Principal>, Vec<Agent>)> {
    let principals = (0..input.get_count("principal dictionary", MIN_PRINCIPAL_BYTES)?)
        .map(|_| principal_from_key(PrincipalKey(input.get_bytes()?, input.get_bytes()?)))
        .collect::<Result<Vec<_>>>()?;
    let agents = (0..input.get_count("agent dictionary", MIN_AGENT_BYTES)?)
        .map(|_| {
            agent_from_key(AgentKey {
                provider: input.get_bytes()?,
                model: input.get_bytes()?,
                session_id: input.get_optional_bytes()?,
                segment_id: input.get_optional_bytes()?,
                policy_id: input.get_optional_bytes()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((principals, agents))
}

fn principal_from_key(value: PrincipalKey) -> Result<Principal> {
    Ok(Principal {
        name: value.0,
        email: value.1,
    })
}

fn agent_from_key(value: AgentKey) -> Result<Agent> {
    Ok(Agent {
        provider: String::from_utf8(value.provider)
            .map_err(|_| invalid("agent provider is not UTF-8"))?,
        model: String::from_utf8(value.model).map_err(|_| invalid("agent model is not UTF-8"))?,
        session_id: optional_string(value.session_id, "agent session id")?,
        segment_id: optional_string(value.segment_id, "agent segment id")?,
        policy_id: optional_string(value.policy_id, "agent policy id")?,
        thought_level: None,
        parent: None,
    })
}

fn optional_string(value: Option<Vec<u8>>, field: &str) -> Result<Option<String>> {
    value
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| invalid(format!("{field} is not UTF-8")))
}

fn principal_at(values: &[Principal], index: u64) -> Result<Principal> {
    values
        .get(index_usize(index)?)
        .cloned()
        .ok_or_else(|| invalid("principal dictionary index is out of range"))
}

fn optional_at<T: Clone>(values: &[T], encoded: u64) -> Result<Option<T>> {
    if encoded == 0 {
        return Ok(None);
    }
    Ok(Some(
        values
            .get(index_usize(encoded - 1)?)
            .cloned()
            .ok_or_else(|| invalid("dictionary index is out of range"))?,
    ))
}

fn index_usize(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| invalid("dictionary index exceeds platform limits"))
}

fn get_optional_bool(input: &mut Reader<'_>) -> Result<Option<bool>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => Ok(Some(false)),
        2 => Ok(Some(true)),
        value => Err(invalid(format!("invalid optional boolean {value}"))),
    }
}

fn get_optional_u32(input: &mut Reader<'_>) -> Result<Option<u32>> {
    match input.get_u64()? {
        0 => Ok(None),
        value => Ok(Some(
            u32::try_from(value - 1).map_err(|_| invalid("optional u32 exceeds its range"))?,
        )),
    }
}

fn get_optional_f32(input: &mut Reader<'_>) -> Result<Option<f32>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => Ok(Some(f32::from_le_bytes(input.get_fixed()?))),
        value => Err(invalid(format!("invalid optional f32 tag {value}"))),
    }
}

fn get_u32(input: &mut Reader<'_>, field: &str) -> Result<u32> {
    u32::try_from(input.get_u64()?).map_err(|_| invalid(format!("{field} exceeds u32")))
}

fn get_i32(input: &mut Reader<'_>, field: &str) -> Result<i32> {
    i32::try_from(input.get_i64()?).map_err(|_| invalid(format!("{field} exceeds i32")))
}

fn timestamp(seconds: i64, nanos: u32) -> Result<DateTime<Utc>> {
    Utc.timestamp_opt(seconds, nanos)
        .single()
        .ok_or_else(|| invalid("compact timestamp is out of range"))
}

fn decode_lineage_kind(value: u8) -> Result<ChangeLineageKind> {
    match value {
        1 => Ok(ChangeLineageKind::CherryPick),
        2 => Ok(ChangeLineageKind::Collapse),
        3 => Ok(ChangeLineageKind::Revert),
        4 => Ok(ChangeLineageKind::GitProjection),
        value => Err(invalid(format!("invalid lineage kind {value}"))),
    }
}

fn blank_state() -> State {
    State {
        state_id: StateId::default(),
        change_id: ChangeId::from_bytes([0; 16]),
        tree: ContentHash::from_bytes([0; 32]),
        parents: Vec::new(),
        attribution: Attribution::human(Principal::new("", "")),
        intent: None,
        confidence: None,
        created_at: DateTime::UNIX_EPOCH,
        verification: None,
        status: Status::Draft,
        provenance: None,
        authored_at: None,
        committer: None,
        authored_tz_offset: 0,
        committer_tz_offset: 0,
        raw_message: None,
        git_lossy: false,
        extra_headers: Vec::new(),
        lineage: Vec::new(),
    }
}
