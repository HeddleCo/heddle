// SPDX-License-Identifier: Apache-2.0
//! Native signal-health queries used by the local CLI.

use std::collections::{HashMap, HashSet};

use objects::{
    object::{RiskSignal, RiskSignalBlob, State, StateAttachmentBody},
    store::ObjectStore,
};
use repo::{Repository, StateAttachmentKind};

const DEFAULT_HEALTH_WINDOW: usize = 200;
const MAX_HEALTH_WINDOW: u32 = 5_000;

#[derive(Debug, Clone, PartialEq)]
pub struct SignalHealthEntry {
    pub module_id: String,
    pub fire_rate: f32,
    pub warn: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignalHealthReport {
    pub entries: Vec<SignalHealthEntry>,
    pub window_states: u32,
}

/// Report per-module signal fire rates over the repository's recent
/// first-parent history. This is deliberately a native query: signal health
/// is local CLI behavior, not part of the shared hosted contract.
pub fn get_repo_signal_health(
    repo: &Repository,
    requested_window: u32,
) -> objects::error::Result<SignalHealthReport> {
    let window = if requested_window == 0 {
        DEFAULT_HEALTH_WINDOW
    } else {
        requested_window.min(MAX_HEALTH_WINDOW) as usize
    };
    let states = walk_recent_states(repo, window)?;
    let visited = states.len() as u32;
    let mut per_module: HashMap<String, u32> = HashMap::new();
    for state in &states {
        let mut seen_modules = HashSet::new();
        for signal in load_signals(repo, state)? {
            let module = signal.producer.module;
            if seen_modules.insert(module.clone()) {
                *per_module.entry(module).or_default() += 1;
            }
        }
    }
    let mut entries: Vec<_> = per_module
        .into_iter()
        .map(|(module_id, hit_count)| {
            let fire_rate = if visited == 0 {
                0.0
            } else {
                hit_count as f32 / visited as f32
            };
            SignalHealthEntry {
                module_id,
                fire_rate,
                warn: fire_rate > 0.5,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.module_id.cmp(&b.module_id));
    Ok(SignalHealthReport {
        entries,
        window_states: visited,
    })
}

fn load_signals(repo: &Repository, state: &State) -> objects::error::Result<Vec<RiskSignal>> {
    let Some(attachment) =
        repo.latest_state_attachment(&state.state_id, StateAttachmentKind::RiskSignals)?
    else {
        return Ok(Vec::new());
    };
    let StateAttachmentBody::RiskSignals(hash) = attachment.body else {
        unreachable!()
    };
    let blob = repo.store().get_blob(&hash)?.ok_or_else(|| {
        objects::error::HeddleError::InvalidObject(format!(
            "risk-signals blob {hash} referenced by state {} is missing",
            state.state_id
        ))
    })?;
    RiskSignalBlob::decode(blob.content())
        .map(|parsed| parsed.signals)
        .map_err(|error| objects::error::HeddleError::InvalidObject(error.to_string()))
}

fn walk_recent_states(repo: &Repository, window: usize) -> objects::error::Result<Vec<State>> {
    let mut states = Vec::new();
    let mut cursor = repo.head()?;
    while let Some(id) = cursor {
        if states.len() >= window {
            break;
        }
        let Some(state) = repo.store().get_state(&id)? else {
            break;
        };
        cursor = state.parents.first().copied();
        states.push(state);
    }
    Ok(states)
}
