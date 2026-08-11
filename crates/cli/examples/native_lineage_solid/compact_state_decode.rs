// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use objects::object::{
    Agent, Attribution, ChangeId, ChangeLineage, ChangeLineageKind, ContentHash, Principal, State,
    StateId, Status, Verification,
};

use crate::{
    compact_io::Reader,
    compact_state_dictionary::{AgentKey, PrincipalKey},
};

const STATE_MAGIC: &[u8; 4] = b"HCS1";

pub fn decode_state_frame(bytes: &[u8]) -> Result<Vec<State>> {
    let mut input = Reader::new(bytes);
    if &input.get_fixed::<4>()? != STATE_MAGIC {
        bail!("invalid compact state frame magic");
    }
    let count = usize::try_from(input.get_u64()?)?;
    let (principals, agents) = decode_dictionaries(&mut input)?;
    let mut states = (0..count).map(|_| blank_state()).collect::<Vec<_>>();

    for state in &mut states {
        state.change_id = ChangeId::from_bytes(input.get_fixed()?);
    }
    for state in &mut states {
        state.tree = ContentHash::from_bytes(input.get_fixed()?);
    }
    for state in &mut states {
        let parent_count = usize::try_from(input.get_u64()?)?;
        state.parents = (0..parent_count)
            .map(|_| Ok(StateId::from_bytes(input.get_fixed()?)))
            .collect::<Result<Vec<_>>>()?;
    }
    for state in &mut states {
        state.attribution.principal = principal_at(&principals, input.get_u64()?)?;
    }
    for state in &mut states {
        state.attribution.agent = optional_at(&agents, input.get_u64()?)?;
    }
    for state in &mut states {
        state.intent = input
            .get_optional_bytes()?
            .map(String::from_utf8)
            .transpose()
            .context("state intent is not UTF-8")?;
    }
    for state in &mut states {
        state.confidence = get_optional_f32(&mut input)?;
    }
    for state in &mut states {
        state.verification = decode_verification(&mut input)?;
    }
    for state in &mut states {
        state.status = Status::from_byte(input.get_u8()?).context("invalid state status")?;
    }

    decode_timestamps(&mut input, &mut states)?;
    for state in &mut states {
        state.authored_tz_offset = i32::try_from(input.get_i64()?)?;
        state.committer_tz_offset = i32::try_from(input.get_i64()?)?;
    }
    for state in &mut states {
        state.provenance = match input.get_u8()? {
            0 => None,
            1 => Some(ContentHash::from_bytes(input.get_fixed()?)),
            value => bail!("invalid provenance option tag {value}"),
        };
    }
    for state in &mut states {
        state.committer = optional_at(&principals, input.get_u64()?)?;
    }
    for state in &mut states {
        state.raw_message = input.get_optional_bytes()?;
    }
    for state in &mut states {
        let count = usize::try_from(input.get_u64()?)?;
        state.extra_headers = (0..count)
            .map(|_| Ok((input.get_bytes()?, input.get_bytes()?)))
            .collect::<Result<Vec<_>>>()?;
    }
    for state in &mut states {
        state.git_lossy = input.get_bool()?;
    }
    for state in &mut states {
        let count = usize::try_from(input.get_u64()?)?;
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
    input.finish()?;
    for state in &mut states {
        state.state_id = state.id();
    }
    Ok(states)
}

fn decode_timestamps(input: &mut Reader<'_>, states: &mut [State]) -> Result<()> {
    let mut previous = 0i64;
    for (index, state) in states.iter_mut().enumerate() {
        let encoded = input.get_i64()?;
        let seconds = if index == 0 {
            encoded
        } else {
            previous + encoded
        };
        let nanos = u32::try_from(input.get_u64()?)?;
        state.created_at = timestamp(seconds, nanos)?;
        previous = seconds;
    }
    for state in states {
        state.authored_at = match input.get_u8()? {
            0 => None,
            1 => {
                let seconds = state.created_at.timestamp() + input.get_i64()?;
                Some(timestamp(seconds, u32::try_from(input.get_u64()?)?)?)
            }
            value => bail!("invalid authored timestamp option tag {value}"),
        };
    }
    Ok(())
}

fn timestamp(seconds: i64, nanos: u32) -> Result<DateTime<Utc>> {
    Utc.timestamp_opt(seconds, nanos)
        .single()
        .context("compact timestamp is out of range")
}

fn decode_verification(input: &mut Reader<'_>) -> Result<Option<Verification>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => {
            let custom_count = {
                let tests_passed = get_optional_bool(input)?;
                let tests_failed = get_optional_u32(input)?;
                let coverage_pct = get_optional_f32(input)?;
                let coverage_delta = get_optional_f32(input)?;
                let lint_warnings = get_optional_u32(input)?;
                let count = usize::try_from(input.get_u64()?)?;
                (
                    tests_passed,
                    tests_failed,
                    coverage_pct,
                    coverage_delta,
                    lint_warnings,
                    count,
                )
            };
            let mut custom = BTreeMap::new();
            for _ in 0..custom_count.5 {
                let key = String::from_utf8(input.get_bytes()?)
                    .context("verification key is not UTF-8")?;
                custom.insert(key, rmp_serde::from_slice(&input.get_bytes()?)?);
            }
            Ok(Some(Verification {
                tests_passed: custom_count.0,
                tests_failed: custom_count.1,
                coverage_pct: custom_count.2,
                coverage_delta: custom_count.3,
                lint_warnings: custom_count.4,
                custom,
            }))
        }
        value => bail!("invalid verification option tag {value}"),
    }
}

fn decode_dictionaries(input: &mut Reader<'_>) -> Result<(Vec<Principal>, Vec<Agent>)> {
    let principal_count = usize::try_from(input.get_u64()?)?;
    let principals = (0..principal_count)
        .map(|_| principal_from_key(PrincipalKey(input.get_bytes()?, input.get_bytes()?)))
        .collect::<Result<Vec<_>>>()?;
    let agent_count = usize::try_from(input.get_u64()?)?;
    let agents = (0..agent_count)
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
        name: String::from_utf8(value.0).context("principal name is not UTF-8")?,
        email: String::from_utf8(value.1).context("principal email is not UTF-8")?,
    })
}

fn agent_from_key(value: AgentKey) -> Result<Agent> {
    Ok(Agent {
        provider: String::from_utf8(value.provider).context("agent provider is not UTF-8")?,
        model: String::from_utf8(value.model).context("agent model is not UTF-8")?,
        session_id: optional_string(value.session_id)?,
        segment_id: optional_string(value.segment_id)?,
        policy_id: optional_string(value.policy_id)?,
    })
}

fn optional_string(value: Option<Vec<u8>>) -> Result<Option<String>> {
    value
        .map(String::from_utf8)
        .transpose()
        .context("agent identity field is not UTF-8")
}

fn principal_at(values: &[Principal], index: u64) -> Result<Principal> {
    values
        .get(usize::try_from(index)?)
        .cloned()
        .context("principal dictionary index is out of range")
}

fn optional_at<T: Clone>(values: &[T], encoded: u64) -> Result<Option<T>> {
    if encoded == 0 {
        return Ok(None);
    }
    Ok(Some(
        values
            .get(usize::try_from(encoded - 1)?)
            .cloned()
            .context("dictionary index is out of range")?,
    ))
}

fn get_optional_bool(input: &mut Reader<'_>) -> Result<Option<bool>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => Ok(Some(false)),
        2 => Ok(Some(true)),
        value => bail!("invalid optional boolean {value}"),
    }
}

fn get_optional_u32(input: &mut Reader<'_>) -> Result<Option<u32>> {
    match input.get_u64()? {
        0 => Ok(None),
        value => Ok(Some(u32::try_from(value - 1)?)),
    }
}

fn get_optional_f32(input: &mut Reader<'_>) -> Result<Option<f32>> {
    match input.get_u8()? {
        0 => Ok(None),
        1 => Ok(Some(f32::from_le_bytes(input.get_fixed()?))),
        value => bail!("invalid optional f32 tag {value}"),
    }
}

fn decode_lineage_kind(value: u8) -> Result<ChangeLineageKind> {
    match value {
        1 => Ok(ChangeLineageKind::CherryPick),
        2 => Ok(ChangeLineageKind::Collapse),
        3 => Ok(ChangeLineageKind::Revert),
        4 => Ok(ChangeLineageKind::GitProjection),
        value => bail!("invalid lineage kind {value}"),
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
