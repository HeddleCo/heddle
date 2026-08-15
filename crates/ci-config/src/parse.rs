// SPDX-License-Identifier: Apache-2.0
//! Parsing, validation, and digesting for CI definitions.

use std::collections::{BTreeMap, BTreeSet};

use heddle_object_model::object::Blob;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use crate::model::{
    Check, CheckClass, CiConfig, DEFAULT_TIMEOUT_SECS, Meta, Retry, SUPPORTED_SCHEMA, Service,
    Trigger,
};

/// A definition parse or validation error.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// TOML syntax or shape is invalid.
    #[error("invalid TOML: {0}")]
    Toml(String),
    /// Schema version is unsupported.
    #[error("unsupported schema version {found}: this build supports schema {supported}")]
    UnsupportedSchema {
        /// Version in the definition.
        found: u32,
        /// Version supported by this build.
        supported: u32,
    },
    /// Check names must be unique.
    #[error("duplicate check name {name:?}")]
    DuplicateCheckName {
        /// Repeated name.
        name: String,
    },
    /// Commands are non-empty argv arrays.
    #[error("check {name:?} has an empty command; provide an argv array")]
    EmptyCommand {
        /// Offending check.
        name: String,
    },
    /// A flake regex did not compile.
    #[error("check {name:?} has invalid flake signature {pattern:?}: {reason}")]
    InvalidFlakeRegex {
        /// Offending check.
        name: String,
        /// Regex source.
        pattern: String,
        /// Regex compiler error.
        reason: String,
    },
    /// Trigger token is unknown.
    #[error("check {name:?} has unknown trigger {trigger:?}")]
    UnknownTrigger {
        /// Offending check.
        name: String,
        /// Authored trigger.
        trigger: String,
    },
    /// Cron syntax is invalid.
    #[error("check {name:?} has invalid cron expression {expression:?}: {reason}")]
    InvalidCron {
        /// Offending check.
        name: String,
        /// Authored expression.
        expression: String,
        /// Validation detail.
        reason: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    meta: Meta,
    #[serde(default, rename = "check")]
    checks: Vec<RawCheck>,
}

#[derive(Debug, Deserialize)]
struct RawCheck {
    name: String,
    #[serde(default)]
    class: Option<CheckClass>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    services: Vec<Service>,
    #[serde(default)]
    cache_paths: Vec<String>,
    #[serde(default)]
    retry: Option<RawRetry>,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    supersede: Option<bool>,
    #[serde(default)]
    isolation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRetry {
    #[serde(default)]
    max: u32,
    #[serde(default)]
    flake_signatures: Vec<String>,
}

const TOP_LEVEL_KEYS: &[&str] = &["meta", "check"];
const CHECK_KEYS: &[&str] = &[
    "name",
    "class",
    "command",
    "timeout_secs",
    "env",
    "services",
    "cache_paths",
    "retry",
    "triggers",
    "supersede",
    "isolation",
];

/// Parse and validate a definition.
pub fn parse(source: &str) -> Result<CiConfig, ConfigError> {
    let table: toml::Table = source
        .parse()
        .map_err(|error: toml::de::Error| ConfigError::Toml(error.to_string()))?;
    let raw: RawConfig =
        toml::from_str(source).map_err(|error| ConfigError::Toml(error.to_string()))?;
    if raw.meta.schema != SUPPORTED_SCHEMA {
        return Err(ConfigError::UnsupportedSchema {
            found: raw.meta.schema,
            supported: SUPPORTED_SCHEMA,
        });
    }

    let warnings = unknown_key_warnings(&table);
    let mut names = BTreeSet::new();
    let mut checks = Vec::with_capacity(raw.checks.len());
    for raw_check in raw.checks {
        if !names.insert(raw_check.name.clone()) {
            return Err(ConfigError::DuplicateCheckName {
                name: raw_check.name,
            });
        }
        checks.push(validate_check(raw_check)?);
    }
    Ok(CiConfig {
        meta: raw.meta,
        checks,
        warnings,
    })
}

fn validate_check(raw: RawCheck) -> Result<Check, ConfigError> {
    if raw.command.is_empty() {
        return Err(ConfigError::EmptyCommand { name: raw.name });
    }
    let retry = raw.retry.unwrap_or_default();
    for pattern in &retry.flake_signatures {
        Regex::new(pattern).map_err(|error| ConfigError::InvalidFlakeRegex {
            name: raw.name.clone(),
            pattern: pattern.clone(),
            reason: error.to_string(),
        })?;
    }
    let triggers = raw
        .triggers
        .iter()
        .map(|token| parse_trigger(&raw.name, token))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Check {
        name: raw.name,
        class: raw.class.unwrap_or(CheckClass::Required),
        command: raw.command,
        timeout_secs: raw.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        env: raw.env,
        services: raw.services,
        cache_paths: raw.cache_paths,
        retry: Retry {
            max: retry.max,
            flake_signatures: retry.flake_signatures,
        },
        triggers,
        supersede: raw.supersede.unwrap_or(true),
        isolation: raw.isolation,
    })
}

fn parse_trigger(name: &str, token: &str) -> Result<Trigger, ConfigError> {
    match token {
        "push" => Ok(Trigger::Push),
        "manual" => Ok(Trigger::Manual),
        _ => match token.strip_prefix("cron:") {
            Some(expression) => {
                validate_cron(name, expression)?;
                Ok(Trigger::Cron(expression.to_string()))
            }
            None => Err(ConfigError::UnknownTrigger {
                name: name.to_string(),
                trigger: token.to_string(),
            }),
        },
    }
}

fn validate_cron(name: &str, expression: &str) -> Result<(), ConfigError> {
    let fields: Vec<_> = expression.split_whitespace().collect();
    let valid = fields.len() == 5
        && fields.iter().all(|field| {
            !field.is_empty()
                && field
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'*' | b',' | b'-' | b'/'))
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidCron {
            name: name.to_string(),
            expression: expression.to_string(),
            reason: "expected five numeric cron fields using *, comma, dash, or slash".to_string(),
        })
    }
}

fn unknown_key_warnings(table: &toml::Table) -> Vec<String> {
    let mut warnings = Vec::new();
    for key in table.keys() {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            warnings.push(format!("unknown top-level key {key:?} (ignored)"));
        }
    }
    if let Some(toml::Value::Array(checks)) = table.get("check") {
        for check in checks.iter().filter_map(toml::Value::as_table) {
            let name = check
                .get("name")
                .and_then(toml::Value::as_str)
                .unwrap_or("<unnamed>");
            for key in check.keys() {
                if !CHECK_KEYS.contains(&key.as_str()) {
                    warnings.push(format!("unknown key {key:?} in check {name:?} (ignored)"));
                }
            }
        }
    }
    warnings
}

/// Heddle's canonical typed-blob hash of the raw definition bytes.
#[must_use]
pub fn definition_digest(raw: &[u8]) -> String {
    Blob::from_slice(raw).hash().to_hex()
}
