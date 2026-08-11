// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use objects::object::{ChangeLineageKind, State};
use serde::Serialize;

use crate::{
    compact_io::Writer,
    compact_state_dictionary::{PrincipalKey, StateDictionaries},
};

const STATE_MAGIC: &[u8; 4] = b"HCS1";

#[derive(Default, Serialize)]
pub struct StateBreakdown {
    pub framing: u64,
    pub structural_pointers: u64,
    pub identities: u64,
    pub timestamps: u64,
    pub intents: u64,
    pub raw_messages: u64,
    pub extra_headers: u64,
    pub other_fidelity: u64,
    pub total: u64,
}

impl StateBreakdown {
    pub fn add(&mut self, other: &Self) {
        self.framing += other.framing;
        self.structural_pointers += other.structural_pointers;
        self.identities += other.identities;
        self.timestamps += other.timestamps;
        self.intents += other.intents;
        self.raw_messages += other.raw_messages;
        self.extra_headers += other.extra_headers;
        self.other_fidelity += other.other_fidelity;
        self.total += other.total;
    }
}

pub fn encode_state_frame(states: &[State]) -> Result<(Vec<u8>, StateBreakdown)> {
    let dictionaries = StateDictionaries::from_states(states);
    let mut output = Writer::new();
    let mut breakdown = StateBreakdown::default();

    let before = output.len();
    output.put_fixed(STATE_MAGIC);
    output.put_u64(states.len() as u64);
    add_bytes(&mut breakdown.framing, before, output.len());

    let before = output.len();
    encode_dictionaries(&mut output, &dictionaries);
    add_bytes(&mut breakdown.identities, before, output.len());

    let before = output.len();
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
    add_bytes(&mut breakdown.structural_pointers, before, output.len());

    let before = output.len();
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
    add_bytes(&mut breakdown.identities, before, output.len());

    let before = output.len();
    for state in states {
        output.put_optional_bytes(state.intent.as_deref().map(str::as_bytes));
    }
    add_bytes(&mut breakdown.intents, before, output.len());

    let before = output.len();
    for state in states {
        put_optional_f32(&mut output, state.confidence);
    }
    encode_verifications(&mut output, states)?;
    for state in states {
        output.put_u8(state.status.to_byte());
    }
    add_bytes(&mut breakdown.other_fidelity, before, output.len());

    let before = output.len();
    encode_timestamps(&mut output, states);
    for state in states {
        output.put_i64(i64::from(state.authored_tz_offset));
        output.put_i64(i64::from(state.committer_tz_offset));
    }
    add_bytes(&mut breakdown.timestamps, before, output.len());

    let before = output.len();
    for state in states {
        match state.provenance {
            Some(hash) => {
                output.put_u8(1);
                output.put_fixed(hash.as_bytes());
            }
            None => output.put_u8(0),
        }
    }
    add_bytes(&mut breakdown.structural_pointers, before, output.len());

    let before = output.len();
    for state in states {
        output.put_u64(
            state
                .committer
                .as_ref()
                .map(|value| dictionaries.principal_index(value) + 1)
                .unwrap_or(0),
        );
    }
    add_bytes(&mut breakdown.identities, before, output.len());

    let before = output.len();
    for state in states {
        output.put_optional_bytes(state.raw_message.as_deref());
    }
    add_bytes(&mut breakdown.raw_messages, before, output.len());

    let before = output.len();
    for state in states {
        output.put_u64(state.extra_headers.len() as u64);
        for (name, value) in &state.extra_headers {
            output.put_bytes(name);
            output.put_bytes(value);
        }
    }
    add_bytes(&mut breakdown.extra_headers, before, output.len());

    let before = output.len();
    for state in states {
        output.put_bool(state.git_lossy);
    }
    add_bytes(&mut breakdown.other_fidelity, before, output.len());

    let before = output.len();
    for state in states {
        output.put_u64(state.lineage.len() as u64);
        for lineage in &state.lineage {
            output.put_u8(lineage_kind_tag(lineage.kind));
            output.put_fixed(lineage.source_change.as_bytes());
            output.put_fixed(lineage.source_state.as_bytes());
        }
    }
    add_bytes(&mut breakdown.structural_pointers, before, output.len());

    let bytes = output.finish();
    breakdown.total = bytes.len() as u64;
    Ok((bytes, breakdown))
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
}

fn encode_verifications(output: &mut Writer, states: &[State]) -> Result<()> {
    for state in states {
        let Some(value) = &state.verification else {
            output.put_u8(0);
            continue;
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
    }
}

fn add_bytes(count: &mut u64, before: usize, after: usize) {
    *count += (after - before) as u64;
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

pub(crate) fn lineage_kind_tag(kind: ChangeLineageKind) -> u8 {
    match kind {
        ChangeLineageKind::CherryPick => 1,
        ChangeLineageKind::Collapse => 2,
        ChangeLineageKind::Revert => 3,
        ChangeLineageKind::GitProjection => 4,
    }
}
