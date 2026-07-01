// SPDX-License-Identifier: Apache-2.0
//! Developer-facing performance profiling helpers.
//!
//! The profile surface is intentionally env-gated so the public CLI
//! stays focused. `HEDDLE_PROFILE=1` writes human-readable timings to
//! stderr, while `HEDDLE_PROFILE=jsonl` writes one structured trace line to
//! stderr. stdout remains reserved for normal text/JSON command output.

use std::{cell::RefCell, collections::BTreeMap, time::Duration};

use serde::Serialize;

const PROFILE_SCHEMA: &str = "heddle-cli-profile/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileMode {
    Off,
    Human,
    Jsonl,
}

#[derive(Clone, Copy, Debug)]
pub struct ProfileField {
    pub name: &'static str,
    pub value: u128,
    unit: ProfileMetricUnit,
}

impl ProfileField {
    pub fn millis(name: &'static str, value_ms: u128) -> Self {
        Self {
            name,
            value: value_ms,
            unit: ProfileMetricUnit::Milliseconds,
        }
    }

    pub fn duration(name: &'static str, value: Duration) -> Self {
        Self {
            name,
            value: value.as_millis(),
            unit: ProfileMetricUnit::Milliseconds,
        }
    }

    pub fn count(name: &'static str, value: impl Into<u128>) -> Self {
        Self {
            name,
            value: value.into(),
            unit: ProfileMetricUnit::Count,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMetricUnit {
    Milliseconds,
    Count,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileMetricValue {
    value: u128,
    unit: ProfileMetricUnit,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfilePhaseRecord {
    name: String,
    metrics: BTreeMap<String, ProfileMetricValue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileTraceRecord {
    schema: &'static str,
    command: String,
    exit_status: &'static str,
    phases: Vec<ProfilePhaseRecord>,
    totals: BTreeMap<String, ProfileMetricValue>,
}

thread_local! {
    static JSONL_PHASES: RefCell<Vec<ProfilePhaseRecord>> = const { RefCell::new(Vec::new()) };
}

pub fn profile_mode() -> ProfileMode {
    std::env::var("HEDDLE_PROFILE")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "" | "0" | "false" | "no" | "off" => ProfileMode::Off,
                "jsonl" => ProfileMode::Jsonl,
                _ => ProfileMode::Human,
            }
        })
        .unwrap_or(ProfileMode::Off)
}

pub fn profile_enabled() -> bool {
    profile_mode() != ProfileMode::Off
}

pub fn emit_profile(command: &str, fields: &[ProfileField]) {
    match profile_mode() {
        ProfileMode::Off => {}
        ProfileMode::Human => emit_human_profile(command, fields),
        ProfileMode::Jsonl => record_phase(command, fields),
    }
}

pub fn emit_command_profile(command: &str, exit_status: i32, totals: &[ProfileField]) {
    match profile_mode() {
        ProfileMode::Off => {}
        ProfileMode::Human => emit_human_profile(command, totals),
        ProfileMode::Jsonl => emit_jsonl_trace(command, exit_status, totals),
    }
}

fn emit_human_profile(command: &str, fields: &[ProfileField]) {
    eprintln!("heddle profile:");
    eprintln!("  command: {command}");
    for field in fields {
        eprintln!("  {}: {}", field.name, field.value);
    }
}

fn record_phase(command: &str, fields: &[ProfileField]) {
    let phase = ProfilePhaseRecord {
        name: command.to_string(),
        metrics: fields_to_metrics(fields),
    };
    JSONL_PHASES.with(|phases| phases.borrow_mut().push(phase));
}

fn emit_jsonl_trace(command: &str, exit_status: i32, totals: &[ProfileField]) {
    let phases = JSONL_PHASES.with(|records| std::mem::take(&mut *records.borrow_mut()));
    let trace = ProfileTraceRecord {
        schema: PROFILE_SCHEMA,
        command: command.to_string(),
        exit_status: if exit_status == 0 { "ok" } else { "error" },
        phases,
        totals: fields_to_metrics(totals),
    };
    match serde_json::to_string(&trace) {
        Ok(line) => eprintln!("{line}"),
        Err(err) => eprintln!("heddle profile jsonl error: {err}"),
    }
}

fn fields_to_metrics(fields: &[ProfileField]) -> BTreeMap<String, ProfileMetricValue> {
    fields
        .iter()
        .map(|field| {
            (
                field.name.to_string(),
                ProfileMetricValue {
                    value: field.value,
                    unit: field.unit,
                },
            )
        })
        .collect()
}
