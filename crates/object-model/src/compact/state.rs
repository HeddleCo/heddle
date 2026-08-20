// SPDX-License-Identifier: Apache-2.0

use super::{
    Result,
    dictionary::{PrincipalKey, StateDictionaries},
    invalid,
    io::Writer,
    limits::MAX_COMPACT_STATE_COUNT,
};
use crate::object::{ChangeLineageKind, State};

pub(super) const STATE_MAGIC: &[u8; 4] = b"HCS1";

/// Whether `bytes` begin with the compact-state frame discriminator.
pub fn is_state_frame(bytes: &[u8]) -> bool {
    bytes.starts_with(STATE_MAGIC)
}

/// Encode states as lossless columns, omitting only the derivable state id.
pub fn encode_state_frame(states: &[State]) -> Result<Vec<u8>> {
    if states.len() > MAX_COMPACT_STATE_COUNT {
        return Err(invalid(format!(
            "state frame count {} exceeds maximum {MAX_COMPACT_STATE_COUNT}",
            states.len()
        )));
    }
    let dictionaries = StateDictionaries::from_states(states);
    let mut output = Writer::new(STATE_MAGIC);
    output.put_u64(states.len() as u64);
    encode_dictionaries(&mut output, &dictionaries);
    encode_structure(&mut output, states);
    encode_attribution(&mut output, states, &dictionaries);
    encode_intent_and_verification(&mut output, states)?;
    encode_timestamps(&mut output, states);
    encode_fidelity(&mut output, states, &dictionaries);
    encode_lineage(&mut output, states);
    Ok(output.finish())
}

fn encode_structure(output: &mut Writer, states: &[State]) {
    for state in states {
        output.put_fixed(state.change_id.as_bytes());
    }
    for state in states {
        output.put_fixed(state.tree.as_bytes());
    }
    for state in states {
        output.put_u64(state.parents.len() as u64);
        for parent in &state.parents {
            output.put_fixed(parent.as_bytes());
        }
    }
}

fn encode_attribution(output: &mut Writer, states: &[State], dictionaries: &StateDictionaries) {
    for state in states {
        output.put_u64(dictionaries.principal_index(&state.attribution.principal));
    }
    for state in states {
        output.put_u64(
            state
                .attribution
                .agent
                .as_ref()
                .map(|agent| dictionaries.agent_index(agent) + 1)
                .unwrap_or(0),
        );
    }
}

fn encode_intent_and_verification(output: &mut Writer, states: &[State]) -> Result<()> {
    for state in states {
        output.put_optional_bytes(state.intent.as_deref().map(str::as_bytes));
    }
    for state in states {
        put_optional_f32(output, state.confidence);
    }
    for state in states {
        encode_verification(output, state)?;
    }
    for state in states {
        output.put_u8(state.status.to_byte());
    }
    Ok(())
}

fn encode_timestamps(output: &mut Writer, states: &[State]) {
    let mut previous = 0i64;
    for (index, state) in states.iter().enumerate() {
        let seconds = state.created_at.timestamp();
        output.put_i64(if index == 0 {
            seconds
        } else {
            seconds - previous
        });
        output.put_u64(u64::from(state.created_at.timestamp_subsec_nanos()));
        previous = seconds;
    }
    for state in states {
        match state.authored_at {
            Some(timestamp) => {
                output.put_u8(1);
                output.put_i64(timestamp.timestamp() - state.created_at.timestamp());
                output.put_u64(u64::from(timestamp.timestamp_subsec_nanos()));
            }
            None => output.put_u8(0),
        }
    }
    for state in states {
        output.put_i64(i64::from(state.authored_tz_offset));
        output.put_i64(i64::from(state.committer_tz_offset));
    }
}

fn encode_fidelity(output: &mut Writer, states: &[State], dictionaries: &StateDictionaries) {
    for state in states {
        match state.provenance {
            Some(hash) => {
                output.put_u8(1);
                output.put_fixed(hash.as_bytes());
            }
            None => output.put_u8(0),
        }
    }
    for state in states {
        output.put_u64(
            state
                .committer
                .as_ref()
                .map(|value| dictionaries.principal_index(value) + 1)
                .unwrap_or(0),
        );
    }
    for state in states {
        output.put_optional_bytes(state.raw_message.as_deref());
    }
    for state in states {
        output.put_u64(state.extra_headers.len() as u64);
        for (name, value) in &state.extra_headers {
            output.put_bytes(name);
            output.put_bytes(value);
        }
    }
    for state in states {
        output.put_bool(state.git_lossy);
    }
}

fn encode_lineage(output: &mut Writer, states: &[State]) {
    for state in states {
        output.put_u64(state.lineage.len() as u64);
        for lineage in &state.lineage {
            output.put_u8(lineage_kind_tag(lineage.kind));
            output.put_fixed(lineage.source_change.as_bytes());
            output.put_fixed(lineage.source_state.as_bytes());
        }
    }
}

fn encode_verification(output: &mut Writer, state: &State) -> Result<()> {
    let Some(value) = &state.verification else {
        output.put_u8(0);
        return Ok(());
    };
    output.put_u8(1);
    put_optional_bool(output, value.tests_passed);
    put_optional_u32(output, value.tests_failed);
    put_optional_f32(output, value.coverage_pct);
    put_optional_f32(output, value.coverage_delta);
    put_optional_u32(output, value.lint_warnings);
    output.put_u64(value.custom.len() as u64);
    for (key, json) in &value.custom {
        output.put_bytes(key.as_bytes());
        output.put_bytes(&rmp_serde::to_vec(json)?);
    }
    Ok(())
}

fn encode_dictionaries(output: &mut Writer, dictionaries: &StateDictionaries) {
    output.put_u64(dictionaries.principals.len() as u64);
    for PrincipalKey(name, email) in &dictionaries.principals {
        output.put_bytes(name);
        output.put_bytes(email);
    }
    output.put_u64(dictionaries.agents.len() as u64);
    for agent in &dictionaries.agents {
        output.put_bytes(&agent.provider);
        output.put_bytes(&agent.model);
        output.put_optional_bytes(agent.session_id.as_deref());
        output.put_optional_bytes(agent.segment_id.as_deref());
        output.put_optional_bytes(agent.policy_id.as_deref());
        output.put_optional_bytes(agent.thought_level.as_deref());
        output.put_optional_bytes(agent.parent.as_deref());
    }
}

fn put_optional_bool(output: &mut Writer, value: Option<bool>) {
    output.put_u8(match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    });
}

fn put_optional_u32(output: &mut Writer, value: Option<u32>) {
    output.put_u64(value.map(u64::from).map(|value| value + 1).unwrap_or(0));
}

fn put_optional_f32(output: &mut Writer, value: Option<f32>) {
    match value {
        Some(value) => {
            output.put_u8(1);
            output.put_fixed(&value.to_le_bytes());
        }
        None => output.put_u8(0),
    }
}

pub(super) fn lineage_kind_tag(kind: ChangeLineageKind) -> u8 {
    match kind {
        ChangeLineageKind::CherryPick => 1,
        ChangeLineageKind::Collapse => 2,
        ChangeLineageKind::Revert => 3,
        ChangeLineageKind::GitProjection => 4,
    }
}
