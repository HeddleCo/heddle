// SPDX-License-Identifier: Apache-2.0
//! Content-addressed cache key bound to env, inputs, and check identity.

use std::collections::BTreeMap;

use ci_config::Check;
use crypto::{Basis, CheckClass, StateRef};
use serde::Serialize;

use crate::{cache::CACHE_ENV_PREFIX, model::ExecutionContext};

/// Addresses a cached check result.
///
/// The key contains no machine-local state (paths, host identity, cache-slot
/// directories). Changing any component yields a different [`CacheKey::id`].
/// Identity fields bind a hit to one check; output digest alone is not enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheKey {
    /// Digest of the content-addressed execution environment `E`.
    pub env_digest: String,
    /// Sorted, tagged content-addresses of the evaluated inputs.
    pub input_digests: Vec<String>,
    /// Digest of the authored definition (`ci.toml` typed-blob hash).
    pub definition_digest: String,
    /// Repository the verdict was produced for.
    pub repo: String,
    /// Source state the verdict was produced for.
    pub state: StateRef,
    /// Evaluated basis the verdict was produced for.
    pub basis: Basis,
    /// Exact argv executed by the check.
    pub command: Vec<String>,
    /// Gating class of the check.
    pub class: CheckClass,
}

#[derive(Serialize)]
struct EnvMaterial<'a> {
    os: &'a str,
    arch: &'a str,
    image_digest: Option<&'a str>,
    toolchain: Option<&'a str>,
    env: BTreeMap<&'a str, &'a str>,
}

impl CacheKey {
    /// Derive the key from the portable projection of `environment`, the
    /// execution context, and the check identity (command, class, repo, state,
    /// basis, definition).
    #[must_use]
    pub fn derive(
        environment: &BTreeMap<String, String>,
        context: &ExecutionContext,
        check: &Check,
    ) -> Self {
        Self {
            env_digest: env_digest(environment, context),
            input_digests: input_digests(context),
            definition_digest: context.definition_digest.clone(),
            repo: context.repo.clone(),
            state: context.state.clone(),
            basis: context.basis.clone(),
            command: check.command.clone(),
            class: map_class(check.class),
        }
    }

    /// Domain-separated digest of the triple alone (no check name).
    #[must_use]
    pub fn id(&self) -> String {
        digest_json(b"key", self)
    }
}

pub(super) fn entry_id(key: &CacheKey, check_name: &str) -> String {
    digest_json(b"entry", &(key, check_name))
}

pub(super) fn entry_id_bytes(key: &CacheKey, check_name: &str) -> [u8; 32] {
    hash_json(b"entry", &(key, check_name))
}

fn map_class(class: ci_config::CheckClass) -> CheckClass {
    match class {
        ci_config::CheckClass::Required => CheckClass::Required,
        ci_config::CheckClass::Advisory => CheckClass::Advisory,
        ci_config::CheckClass::Informational => CheckClass::Informational,
    }
}

fn env_digest(environment: &BTreeMap<String, String>, context: &ExecutionContext) -> String {
    let env = portable_env(environment);
    digest_json(
        b"env",
        &EnvMaterial {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            image_digest: context.image_digest.as_deref(),
            toolchain: context.toolchain.as_deref(),
            env,
        },
    )
}

fn input_digests(context: &ExecutionContext) -> Vec<String> {
    let mut inputs = vec![
        format!("state:{}", context.state.content_hash),
        format!("tree:{}", context.basis.evaluated_tree_digest),
    ];
    inputs.sort();
    inputs
}

fn portable_env(environment: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    environment
        .iter()
        .filter(|(name, _)| !is_machine_local_key(name))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

fn is_machine_local_key(name: &str) -> bool {
    name.starts_with(CACHE_ENV_PREFIX)
        || matches!(
            name,
            "PATH"
                | "HOME"
                | "USER"
                | "SHELL"
                | "TERM"
                | "CARGO_HOME"
                | "RUSTUP_HOME"
                | "TMPDIR"
                | "TEMP"
                | "TMP"
                | "PWD"
                | "HOSTNAME"
                | "HOST"
                | "LOGNAME"
        )
}

fn digest_json(label: &[u8], value: &impl Serialize) -> String {
    blake3::Hash::from_bytes(hash_json(label, value))
        .to_hex()
        .to_string()
}

fn hash_json(label: &[u8], value: &impl Serialize) -> [u8; 32] {
    let payload = serde_json::to_vec(value).expect("cache key material is always serializable");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"heddle-ci-result-cache-v1\0");
    hasher.update(&(label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update(&(payload.len() as u64).to_le_bytes());
    hasher.update(&payload);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use ci_config::{CheckClass as ConfigClass, Retry};
    use crypto::{Basis, BasisKind, StateRef};

    use super::*;
    use crate::model::ExecutionContext;

    fn check() -> Check {
        Check {
            name: "marker".to_string(),
            class: ConfigClass::Required,
            command: vec!["echo".to_string(), "ok".to_string()],
            timeout_secs: 60,
            env: BTreeMap::new(),
            services: Vec::new(),
            cache_paths: Vec::new(),
            retry: Retry::default(),
            triggers: Vec::new(),
            supersede: false,
            isolation: None,
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext {
            repo: "test/repo".to_string(),
            state: StateRef {
                content_hash: "state-content".to_string(),
                change_id: "change".to_string(),
                logical_change_id: None,
            },
            basis: Basis {
                kind: BasisKind::Branch,
                evaluated_tree_digest: "tree".to_string(),
            },
            definition_digest: "definition".to_string(),
            toolchain: Some("rustc 1.97.0".to_string()),
            pick_id: None,
            attempt: 1,
            runner: None,
            image_digest: Some("sha256:image".to_string()),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn each_triple_component_changes_the_key() {
        let check = check();
        let base = CacheKey::derive(&env(&[("FOO", "1")]), &context(), &check);
        let mut changed_env = context();
        let env_miss = CacheKey::derive(&env(&[("FOO", "2")]), &changed_env, &check);
        assert_ne!(base.env_digest, env_miss.env_digest);
        assert_ne!(base.id(), env_miss.id());

        changed_env.basis.evaluated_tree_digest = "tree-2".to_string();
        let input_miss = CacheKey::derive(&env(&[("FOO", "1")]), &changed_env, &check);
        assert_ne!(base.input_digests, input_miss.input_digests);
        assert_ne!(base.id(), input_miss.id());

        let mut changed_definition = context();
        changed_definition.definition_digest = "definition-2".to_string();
        let definition_miss = CacheKey::derive(&env(&[("FOO", "1")]), &changed_definition, &check);
        assert_ne!(base.definition_digest, definition_miss.definition_digest);
        assert_ne!(base.id(), definition_miss.id());
    }

    #[test]
    fn check_identity_changes_the_key() {
        let env = env(&[("FOO", "1")]);
        let base = CacheKey::derive(&env, &context(), &check());

        let mut other_class = check();
        other_class.class = ConfigClass::Informational;
        assert_ne!(
            base.id(),
            CacheKey::derive(&env, &context(), &other_class).id()
        );

        let mut other_command = check();
        other_command.command = vec!["false".to_string()];
        assert_ne!(
            base.id(),
            CacheKey::derive(&env, &context(), &other_command).id()
        );

        let mut other_repo = context();
        other_repo.repo = "other/repo".to_string();
        assert_ne!(
            base.id(),
            CacheKey::derive(&env, &other_repo, &check()).id()
        );
    }

    #[test]
    fn machine_local_env_is_not_in_the_key() {
        let check = check();
        let portable = CacheKey::derive(&env(&[("FOO", "1"), ("LANG", "C")]), &context(), &check);
        let local = CacheKey::derive(
            &env(&[
                ("FOO", "1"),
                ("LANG", "C"),
                ("PATH", "/other/bin"),
                ("HOME", "/other/home"),
                ("HCI_CACHE_CARGO", "/tmp/machine-a/CARGO"),
            ]),
            &context(),
            &check,
        );
        assert_eq!(portable.env_digest, local.env_digest);
        assert_eq!(portable.id(), local.id());
    }

    #[test]
    fn image_and_toolchain_are_part_of_env_digest() {
        let check = check();
        let base = CacheKey::derive(&env(&[]), &context(), &check);
        let mut changed = context();
        changed.image_digest = Some("sha256:other".to_string());
        assert_ne!(
            base.env_digest,
            CacheKey::derive(&env(&[]), &changed, &check).env_digest
        );
        changed = context();
        changed.toolchain = Some("rustc 1.88.0".to_string());
        assert_ne!(
            base.env_digest,
            CacheKey::derive(&env(&[]), &changed, &check).env_digest
        );
    }
}
