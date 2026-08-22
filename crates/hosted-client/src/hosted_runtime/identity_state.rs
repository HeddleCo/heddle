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
const STATE_VERSION: u32 = 2;
const STATE_FILE: &str = "agent-claim.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaimStatus {
    Dormant,
    Active,
    Prepared,
    Claimed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ClaimState {
    format: String,
    version: u32,
    pub(crate) server: String,
    pub(crate) owner_id: uuid::Uuid,
    pub(crate) subject: String,
    pub(crate) pet_name: String,
    pub(crate) node_id: String,
    pub(crate) created_at: String,
    secret_hash: String,
    pub(crate) expires_at_millis: i64,
    status: ClaimStatus,
    prepared_handle: Option<String>,
    prepared_nonce_hash: Option<String>,
}

impl ClaimState {
    pub(crate) fn new(
        server: String,
        owner_id: uuid::Uuid,
        subject: String,
        pet_name: String,
        node_id: String,
    ) -> Self {
        Self {
            format: STATE_FORMAT.to_string(),
            version: STATE_VERSION,
            server,
            owner_id,
            subject,
            pet_name,
            node_id,
            created_at: chrono::Utc::now().to_rfc3339(),
            secret_hash: String::new(),
            expires_at_millis: 0,
            status: ClaimStatus::Dormant,
            prepared_handle: None,
            prepared_nonce_hash: None,
        }
    }

    pub(crate) fn reissue(&mut self, secret: &[u8], expires_at_millis: i64) -> bool {
        if matches!(self.status, ClaimStatus::Claimed) {
            return false;
        }
        self.secret_hash = hex::encode(Sha256::digest(secret));
        self.expires_at_millis = expires_at_millis;
        self.status = ClaimStatus::Active;
        self.prepared_handle = None;
        self.prepared_nonce_hash = None;
        true
    }

    pub(crate) fn is_active(&self, now: i64) -> bool {
        matches!(self.status, ClaimStatus::Active | ClaimStatus::Prepared)
            && self.consent_unexpired(now)
    }

    /// True when this issuance has a bound expiry that has not yet elapsed.
    ///
    /// Promote consent is signed after [`Self::claim`] flips status to
    /// `Claimed`, so callers that only need the signature TTL must use this
    /// instead of [`Self::is_active`].
    pub(crate) fn consent_unexpired(&self, now: i64) -> bool {
        self.expires_at_millis > 0 && now < self.expires_at_millis
    }

    pub(crate) fn accepts(&self, secret: &[u8], now: i64) -> bool {
        if !self.is_active(now) {
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

    pub(crate) fn is_claimed(&self) -> bool {
        matches!(self.status, ClaimStatus::Claimed)
    }

    pub(crate) fn prepare(&mut self, handle: &str, nonce: &[u8]) -> bool {
        let nonce_hash = hex::encode(Sha256::digest(nonce));
        match self.status {
            ClaimStatus::Active => {
                self.status = ClaimStatus::Prepared;
                self.prepared_handle = Some(handle.to_string());
                self.prepared_nonce_hash = Some(nonce_hash);
                true
            }
            ClaimStatus::Prepared => {
                self.prepared_handle.as_deref() == Some(handle)
                    && self.prepared_nonce_hash.as_deref() == Some(nonce_hash.as_str())
            }
            ClaimStatus::Dormant | ClaimStatus::Claimed => false,
        }
    }

    pub(crate) fn claim(&mut self, handle: &str) -> bool {
        if !matches!(self.status, ClaimStatus::Prepared)
            || self.prepared_handle.as_deref() != Some(handle)
        {
            return false;
        }
        self.status = ClaimStatus::Claimed;
        self.prepared_nonce_hash = None;
        true
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

pub(crate) fn load_while_locked() -> Result<Option<ClaimState>> {
    load_at(&state_path())
}

pub(crate) fn store_while_locked(state: &ClaimState) -> Result<()> {
    store_at_unlocked(&state_path(), state)
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
    store_at_unlocked(path, state)
}

fn store_at_unlocked(path: &Path, state: &ClaimState) -> Result<()> {
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
            uuid::Uuid::parse_str("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28").unwrap(),
            "subject-1".into(),
            "quiet-otter".into(),
            "11".repeat(32),
        )
    }

    #[test]
    fn reissue_invalidates_the_previous_secret() {
        let mut state = state();
        assert!(state.reissue(b"first-secret", 2_000));
        assert!(state.accepts(b"first-secret", 1_000));
        assert!(state.reissue(b"second-secret", 2_000));
        assert!(!state.accepts(b"first-secret", 1_000));
        assert!(state.accepts(b"second-secret", 1_000));
    }

    #[test]
    fn prepared_claim_is_bound_to_one_handle_and_nonce() {
        let mut state = state();
        assert!(state.reissue(b"claim-secret", 2_000));
        assert!(state.prepare("human-handle", b"nonce-one"));
        assert!(state.prepare("human-handle", b"nonce-one"));
        assert!(!state.prepare("other-handle", b"nonce-one"));
        assert!(!state.prepare("human-handle", b"nonce-two"));
        assert!(!state.claim("other-handle"));
        assert!(state.claim("human-handle"));
        assert!(!state.accepts(b"claim-secret", 1_000));
        assert!(!state.reissue(b"third-secret", 3_000));
    }

    #[test]
    fn expired_secret_is_rejected_and_not_persisted_in_plaintext() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("claim.toml");
        let mut state = state();
        assert!(state.reissue(b"super-secret-value", 2_000));
        assert!(state.consent_unexpired(1_999));
        assert!(!state.consent_unexpired(2_000));
        assert!(!state.accepts(b"super-secret-value", 2_000));
        store_at(&path, &state).expect("store claim state");
        let contents = std::fs::read_to_string(path).expect("read claim state");
        assert!(!contents.contains("super-secret-value"));
    }
}
