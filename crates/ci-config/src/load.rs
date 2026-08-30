// SPDX-License-Identifier: Apache-2.0
//! Decode a canonical TreadleDefinition, map it onto the engine, and admit host-exec.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use api::{
    heddle::api::v1alpha1::{
        TreadleCheck, TreadleCheckClass, TreadleDefinition, TreadleNetworkAccess,
        TreadleSecretTier, TreadleServiceContainer, TreadleTriggerKind, treadle_env_entry,
    },
    treadle::{
        TREADLE_DEFINITION_FORMAT_VERSION, TreadleDefinitionError,
        decode_canonical_treadle_definition, treadle_definition_blake3,
    },
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Check, CheckClass, CiConfig, Retry, Service, Trigger};

/// Default on-disk definition: canonical protobuf bytes under the shared `.heddle/`.
pub const DEFAULT_DEFINITION_FILE: &str = "treadle.definition.bin";
/// Lockfile next to the definition. Present lock is fail-closed against the digest.
pub const DEFAULT_LOCK_FILE: &str = "treadle.lock.json";

/// A definition decode, mapping, or host-exec admission error.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Protobuf bytes are not a valid message.
    #[error("invalid treadle protobuf: {0}")]
    Decode(String),
    /// The definition failed v1 validation.
    #[error("invalid treadle definition: {0}")]
    Invalid(String),
    /// Bytes decoded but are not the canonical encoding.
    #[error("treadle definition bytes are not canonical")]
    NonCanonicalBytes,
    /// Format version is not the current reader version.
    #[error(
        "unsupported treadle definition format version {found}: this build supports version {supported}"
    )]
    UnsupportedVersion {
        /// Version in the definition.
        found: u32,
        /// Version supported by this build.
        supported: u32,
    },
    /// Lockfile JSON is invalid or the wrong format version.
    #[error("invalid treadle lockfile: {0}")]
    Lock(String),
    /// Lockfile digest does not match the definition bytes.
    #[error(
        "treadle.lock.json definition_digest {found} does not match definition {expected}; refuse to run"
    )]
    LockMismatch {
        /// Digest recorded in the lockfile.
        found: String,
        /// Digest of the definition bytes.
        expected: String,
    },
    /// Check names must be unique across the flattened definition.
    #[error("duplicate check name {name:?}")]
    DuplicateCheckName {
        /// Repeated name.
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
    /// Host OS/arch does not match the check's authoritative platform.
    #[error(
        "check {name:?} targets {wanted_os}/{wanted_arch}; this host is {host_os}/{host_arch} and host-exec will not pull the OCI image"
    )]
    PlatformMismatch {
        /// Offending check.
        name: String,
        /// Definition OS.
        wanted_os: String,
        /// Definition architecture.
        wanted_arch: String,
        /// Host OS (OCI/Go style).
        host_os: String,
        /// Host architecture (OCI/Go style).
        host_arch: String,
    },
    /// Local host-exec cannot satisfy a trusted-runner-only secret.
    #[error(
        "check {name:?} requires trusted-runner-only secret {secret:?}; refusing local host-exec"
    )]
    TrustedRunnerSecret {
        /// Offending check.
        name: String,
        /// Secret declaration name.
        secret: String,
    },
    /// Local host-exec does not pretend FULL network isolation.
    #[error(
        "check {name:?} requests network_access = FULL; refusing local host-exec rather than pretending hermeticity"
    )]
    FullNetwork {
        /// Offending check.
        name: String,
    },
    /// Local host-exec does not apply cgroups, rlimits, or named isolation profiles.
    #[error(
        "check {name:?} requests isolation {detail}; refusing local host-exec rather than running unbounded"
    )]
    UnsupportedIsolation {
        /// Offending check.
        name: String,
        /// The hint the host cannot apply (`cpu_millis=…`, `profile=…`, …).
        detail: String,
    },
    /// A declared cache path would escape the evaluated worktree.
    #[error(
        "check {name:?} cache path {path:?} is not a worktree-relative directory (absolute paths and .. are refused)"
    )]
    InvalidCachePath {
        /// Offending check.
        name: String,
        /// Declared path.
        path: String,
    },
    /// The lockfile next to the definition blob is required.
    #[error("treadle.lock.json is required next to the definition at {path}")]
    LockMissing {
        /// Expected lock path.
        path: String,
    },
}

impl From<TreadleDefinitionError> for ConfigError {
    fn from(error: TreadleDefinitionError) -> Self {
        match error {
            TreadleDefinitionError::UnsupportedVersion { actual, expected } => {
                Self::UnsupportedVersion {
                    found: actual,
                    supported: expected,
                }
            }
            TreadleDefinitionError::Invalid(message) => Self::Invalid(message),
            TreadleDefinitionError::Decode(error) => Self::Decode(error.to_string()),
            TreadleDefinitionError::NonCanonicalBytes => Self::NonCanonicalBytes,
        }
    }
}

/// `format_version` + hex BLAKE3 of the canonical definition bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreadleLockfile {
    /// Lockfile / definition-format version. Must be 1.
    pub format_version: u32,
    /// Lowercase hex BLAKE3 of the canonical protobuf bytes.
    pub definition_digest: String,
}

/// A decoded definition plus the engine mapping and content address.
#[derive(Debug, Clone)]
pub struct LoadedDefinition {
    /// Canonical decoded proto.
    pub definition: TreadleDefinition,
    /// Engine-facing checks.
    pub config: CiConfig,
    /// Lowercase hex of [`treadle_definition_blake3`].
    pub definition_digest: String,
}

/// Decode only canonical current-version bytes and map them onto [`CiConfig`].
pub fn load(bytes: &[u8]) -> Result<LoadedDefinition, ConfigError> {
    let definition = decode_canonical_treadle_definition(bytes)?;
    let definition_digest = definition_digest(&definition)?;
    let config = map_definition(&definition)?;
    Ok(LoadedDefinition {
        definition,
        config,
        definition_digest,
    })
}

/// Hex BLAKE3 of the canonical protobuf encoding. Reuses the api hasher.
pub fn definition_digest(definition: &TreadleDefinition) -> Result<String, ConfigError> {
    Ok(hex::encode(treadle_definition_blake3(definition)?))
}

/// Read and parse the lockfile that must sit next to a definition blob.
pub fn load_lock_file(path: &Path) -> Result<TreadleLockfile, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => read_lock(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ConfigError::LockMissing {
                path: path.display().to_string(),
            })
        }
        Err(error) => Err(ConfigError::Lock(format!(
            "read {}: {error}",
            path.display()
        ))),
    }
}

/// Parse a `treadle.lock.json` body.
pub fn read_lock(bytes: &[u8]) -> Result<TreadleLockfile, ConfigError> {
    let lock: TreadleLockfile =
        serde_json::from_slice(bytes).map_err(|error| ConfigError::Lock(error.to_string()))?;
    if lock.format_version != TREADLE_DEFINITION_FORMAT_VERSION {
        return Err(ConfigError::Lock(format!(
            "unsupported format_version {}; this build supports {}",
            lock.format_version, TREADLE_DEFINITION_FORMAT_VERSION
        )));
    }
    if lock.definition_digest.len() != 64
        || !lock
            .definition_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ConfigError::Lock(
            "definition_digest must be 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    Ok(lock)
}

/// Fail closed when a present lockfile disagrees with the definition digest.
pub fn verify_lock(lock: &TreadleLockfile, expected: &str) -> Result<(), ConfigError> {
    if lock.definition_digest != expected {
        return Err(ConfigError::LockMismatch {
            found: lock.definition_digest.clone(),
            expected: expected.to_string(),
        });
    }
    Ok(())
}

/// Lockfile path next to a definition blob: `<dir>/treadle.lock.json`.
#[must_use]
pub fn lock_path(definition_path: &Path) -> PathBuf {
    match definition_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(DEFAULT_LOCK_FILE),
        _ => PathBuf::from(DEFAULT_LOCK_FILE),
    }
}

/// OCI/Go-style (`linux`/`darwin`/`windows`, `amd64`/`arm64`) host platform.
#[must_use]
pub fn host_oci_platform() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    (os.to_string(), arch.to_string())
}

/// Refuse host-exec for selected checks that this slice cannot run honestly.
///
/// `selected` is the check names that will run. An empty slice admits every
/// check in the definition.
pub fn admit_host_exec(
    definition: &TreadleDefinition,
    selected: &[String],
) -> Result<(), ConfigError> {
    let run_all = selected.is_empty();
    for job in &definition.jobs {
        for check in &job.checks {
            if !run_all && !selected.iter().any(|name| name == &check.name) {
                continue;
            }
            admit_check(check, definition)?;
        }
    }
    Ok(())
}

fn admit_check(check: &TreadleCheck, definition: &TreadleDefinition) -> Result<(), ConfigError> {
    let (host_os, host_arch) = host_oci_platform();
    let platform = check
        .target_environment
        .as_ref()
        .and_then(|environment| environment.platform.as_ref());
    if let Some(platform) = platform
        && (platform.os != host_os || platform.arch != host_arch)
    {
        return Err(ConfigError::PlatformMismatch {
            name: check.name.clone(),
            wanted_os: platform.os.clone(),
            wanted_arch: platform.arch.clone(),
            host_os,
            host_arch,
        });
    }

    if let Some(secret) = trusted_runner_secret(check, definition) {
        return Err(ConfigError::TrustedRunnerSecret {
            name: check.name.clone(),
            secret,
        });
    }

    let network = check
        .isolation
        .as_ref()
        .and_then(|isolation| TreadleNetworkAccess::try_from(isolation.network_access).ok());
    if network == Some(TreadleNetworkAccess::Full) {
        return Err(ConfigError::FullNetwork {
            name: check.name.clone(),
        });
    }
    // NONE is admitted: v0 has no netns; that stay is explicit.
    if let Some(detail) = unenforceable_isolation(check) {
        return Err(ConfigError::UnsupportedIsolation {
            name: check.name.clone(),
            detail,
        });
    }
    for path in &check.cache_paths {
        if !crate::cache_path_is_worktree_relative(path) {
            return Err(ConfigError::InvalidCachePath {
                name: check.name.clone(),
                path: path.clone(),
            });
        }
    }
    Ok(())
}

fn unenforceable_isolation(check: &TreadleCheck) -> Option<String> {
    let isolation = check.isolation.as_ref()?;
    if isolation.cpu_millis != 0 {
        return Some(format!("cpu_millis={}", isolation.cpu_millis));
    }
    if isolation.memory_bytes != 0 {
        return Some(format!("memory_bytes={}", isolation.memory_bytes));
    }
    if isolation.process_limit != 0 {
        return Some(format!("process_limit={}", isolation.process_limit));
    }
    if !isolation.profile.is_empty() {
        return Some(format!("profile={}", isolation.profile));
    }
    None
}

fn trusted_runner_secret(check: &TreadleCheck, definition: &TreadleDefinition) -> Option<String> {
    let trusted: BTreeSet<&str> = definition
        .secret_refs
        .iter()
        .filter(|secret| {
            TreadleSecretTier::try_from(secret.tier).ok()
                == Some(TreadleSecretTier::TrustedRunnerOnly)
        })
        .map(|secret| secret.name.as_str())
        .collect();
    env_trusted_secret(&check.env, &trusted).or_else(|| {
        definition
            .services
            .iter()
            .filter(|service| {
                check
                    .service_dependencies
                    .iter()
                    .any(|name| name == &service.name)
            })
            .find_map(|service| env_trusted_secret(&service.env, &trusted))
    })
}

fn env_trusted_secret(
    env: &[api::heddle::api::v1alpha1::TreadleEnvEntry],
    trusted: &BTreeSet<&str>,
) -> Option<String> {
    env.iter().find_map(|entry| match &entry.source {
        Some(treadle_env_entry::Source::SecretRef(name)) if trusted.contains(name.as_str()) => {
            Some(name.clone())
        }
        _ => None,
    })
}

fn map_definition(definition: &TreadleDefinition) -> Result<CiConfig, ConfigError> {
    let services: BTreeMap<&str, &TreadleServiceContainer> = definition
        .services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect();
    let mut names = BTreeSet::new();
    let mut checks = Vec::new();
    for job in &definition.jobs {
        for check in &job.checks {
            if !names.insert(check.name.clone()) {
                return Err(ConfigError::DuplicateCheckName {
                    name: check.name.clone(),
                });
            }
            checks.push(map_check(check, &services)?);
        }
    }
    Ok(CiConfig {
        name: definition.name.clone(),
        format_version: definition.format_version,
        checks,
    })
}

fn map_check(
    check: &TreadleCheck,
    services: &BTreeMap<&str, &TreadleServiceContainer>,
) -> Result<Check, ConfigError> {
    let mut command = Vec::with_capacity(check.args.len() + 1);
    command.push(check.command.clone());
    command.extend(check.args.iter().cloned());
    let retry = check.retry.clone().unwrap_or_default();
    for pattern in &retry.flake_signatures {
        Regex::new(pattern).map_err(|error| ConfigError::InvalidFlakeRegex {
            name: check.name.clone(),
            pattern: pattern.clone(),
            reason: error.to_string(),
        })?;
    }
    let class = match TreadleCheckClass::try_from(check.class) {
        Ok(TreadleCheckClass::Advisory) => CheckClass::Advisory,
        Ok(TreadleCheckClass::Informational) => CheckClass::Informational,
        Ok(TreadleCheckClass::Required) | Ok(TreadleCheckClass::Unspecified) | Err(_) => {
            CheckClass::Required
        }
    };
    let isolation = check
        .isolation
        .as_ref()
        .and_then(|hints| (!hints.profile.is_empty()).then(|| hints.profile.clone()));
    Ok(Check {
        name: check.name.clone(),
        class,
        command,
        timeout_secs: u64::from(check.timeout_seconds),
        env: literal_env(&check.env),
        services: check
            .service_dependencies
            .iter()
            .filter_map(|name| services.get(name.as_str()).copied())
            .map(map_service)
            .collect(),
        cache_paths: check.cache_paths.clone(),
        retry: Retry {
            max: retry.max_retries,
            flake_signatures: retry.flake_signatures,
        },
        triggers: check
            .triggers
            .iter()
            .filter_map(|trigger| match TreadleTriggerKind::try_from(trigger.kind) {
                Ok(TreadleTriggerKind::Push) => Some(Trigger::Push),
                Ok(TreadleTriggerKind::Manual) => Some(Trigger::Manual),
                Ok(TreadleTriggerKind::Cron) => {
                    Some(Trigger::Cron(trigger.cron_expression.clone()))
                }
                Ok(TreadleTriggerKind::Unspecified) | Err(_) => None,
            })
            .collect(),
        supersede: check.supersede_older_runs,
        isolation,
        working_directory: check.working_directory.clone(),
    })
}

fn literal_env(env: &[api::heddle::api::v1alpha1::TreadleEnvEntry]) -> BTreeMap<String, String> {
    env.iter()
        .filter_map(|entry| match &entry.source {
            Some(treadle_env_entry::Source::LiteralValue(value)) => {
                Some((entry.name.clone(), value.clone()))
            }
            _ => None,
        })
        .collect()
}

fn map_service(service: &TreadleServiceContainer) -> Service {
    let ready_cmd = service.readiness.as_ref().map(|readiness| {
        let mut argv = Vec::with_capacity(readiness.args.len() + 1);
        argv.push(readiness.command.clone());
        argv.extend(readiness.args.iter().cloned());
        argv
    });
    Service {
        name: service.name.clone(),
        image: service.image.clone(),
        ports: service
            .ports
            .iter()
            .filter_map(|port| u16::try_from(*port).ok())
            .collect(),
        env: literal_env(&service.env),
        ready_cmd,
    }
}
