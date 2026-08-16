//! Persisted authorization state for the browser-to-agent claim ceremony.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use objects::{
    fs_atomic::write_file_atomic_secret,
    lock::{RepoLock, WriteLockGuard},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STATE_FORMAT: &str = "heddle-agent-claim";
const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "agent-claim.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaimStatus {
    Active,
    Consented,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ClaimState {
    format: String,
    version: u32,
    pub(crate) server: String,
    pub(crate) account_id: String,
    pub(crate) subject: String,
    pub(crate) pet_name: String,
    pub(crate) node_id: String,
    pub(crate) created_at: String,
    secret_hash: String,
    pub(crate) expires_at_millis: i64,
    status: ClaimStatus,
}

impl ClaimState {
    pub(crate) fn new(
        server: String,
        account_id: String,
        subject: String,
        pet_name: String,
        node_id: String,
    ) -> Self {
        Self {
            format: STATE_FORMAT.to_string(),
            version: STATE_VERSION,
            server,
            account_id,
            subject,
            pet_name,
            node_id,
            created_at: chrono::Utc::now().to_rfc3339(),
            secret_hash: String::new(),
            expires_at_millis: 0,
            status: ClaimStatus::Consented,
        }
    }

    pub(crate) fn reissue(&mut self, secret: &[u8], expires_at_millis: i64) {
        self.secret_hash = hex::encode(Sha256::digest(secret));
        self.expires_at_millis = expires_at_millis;
        self.status = ClaimStatus::Active;
    }

    pub(crate) fn is_active(&self, now: i64) -> bool {
        matches!(self.status, ClaimStatus::Active) && now < self.expires_at_millis
    }

    pub(crate) fn accepts(&self, secret: &[u8], now: i64) -> bool {
        if now >= self.expires_at_millis {
            return false;
        }
        let Ok(expected) = hex::decode(&self.secret_hash) else {
            return false;
        };
        constant_time_eq(&expected, Sha256::digest(secret).as_slice())
    }

    pub(crate) fn authorization_hash(&self) -> &str {
        &self.secret_hash
    }

    pub(crate) fn is_consented(&self) -> bool {
        matches!(self.status, ClaimStatus::Consented)
    }

    pub(crate) fn mark_consented(&mut self) {
        self.status = ClaimStatus::Consented;
    }
}

pub(crate) fn state_path() -> PathBuf {
    repo::identity::heddle_home_dir().join(STATE_FILE)
}

pub(crate) fn load() -> Result<Option<ClaimState>> {
    load_at(&state_path())
}

pub(crate) fn store(state: &ClaimState) -> Result<()> {
    store_at(&state_path(), state)
}

pub(crate) fn write_lock() -> Result<WriteLockGuard> {
    claim_lock(&state_path())
        .write()
        .map_err(|error| anyhow::anyhow!(error))
}

fn load_at(path: &Path) -> Result<Option<ClaimState>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    crypto::reject_group_or_world_readable_key(path)
        .with_context(|| format!("refusing exposed claim state {}", path.display()))?;
    let state: ClaimState =
        toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    if state.format != STATE_FORMAT || state.version != STATE_VERSION {
        bail!("{} is not supported Heddle claim state", path.display());
    }
    Ok(Some(state))
}

fn store_at(path: &Path, state: &ClaimState) -> Result<()> {
    if let Some(parent) = path.parent() {
        objects::fs_atomic::create_private_dir_all(parent)?;
    }
    let _guard = claim_lock(path)
        .write()
        .map_err(|error| anyhow::anyhow!(error))?;
    let encoded = toml::to_string_pretty(state).context("serializing claim state")?;
    write_file_atomic_secret(path, encoded.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn claim_lock(path: &Path) -> RepoLock {
    RepoLock::at(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("locks/agent-claim.lock"),
    )
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(*left.get(index).unwrap_or(&0) ^ *right.get(index).unwrap_or(&0));
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ClaimState {
        ClaimState::new(
            "api.heddle.test".into(),
            "account-1".into(),
            "subject-1".into(),
            "quiet-otter".into(),
            "11".repeat(32),
        )
    }

    #[test]
    fn reissue_invalidates_the_previous_secret() {
        let mut state = state();
        state.reissue(b"first-secret", 2_000);
        assert!(state.accepts(b"first-secret", 1_000));
        state.reissue(b"second-secret", 2_000);
        assert!(!state.accepts(b"first-secret", 1_000));
        assert!(state.accepts(b"second-secret", 1_000));
    }

    #[test]
    fn expired_secret_is_rejected_and_not_persisted_in_plaintext() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("claim.toml");
        let mut state = state();
        state.reissue(b"super-secret-value", 2_000);
        assert!(!state.accepts(b"super-secret-value", 2_000));
        store_at(&path, &state).expect("store claim state");
        let contents = std::fs::read_to_string(path).expect("read claim state");
        assert!(!contents.contains("super-secret-value"));
    }
}
