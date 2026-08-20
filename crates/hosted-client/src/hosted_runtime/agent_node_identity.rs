//! Persisted transport identity for the agent's Iroh endpoint.
//!
//! The claim URL names this endpoint by its cryptographic node id. Minting a
//! fresh key at each bind therefore invalidates every outstanding URL. Keep a
//! distinct transport key beside the agent's other credential material under
//! `<heddle_home>` and fail closed if that key can no longer be trusted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use objects::{fs_atomic::write_file_atomic_secret, lock::RepoLock};
use serde::{Deserialize, Serialize};

const IDENTITY_FORMAT: &str = "heddle-agent-node-identity";
const IDENTITY_VERSION: u32 = 1;
const IDENTITY_FILE: &str = "agent-node-identity.toml";

#[derive(Clone, Debug)]
pub(crate) struct AgentNodeIdentity {
    secret_key: SecretKey,
}

impl AgentNodeIdentity {
    pub(crate) fn secret_key(&self) -> SecretKey {
        self.secret_key.clone()
    }

    pub(crate) fn node_id(&self) -> EndpointId {
        self.secret_key.public()
    }
}

#[derive(Serialize, Deserialize)]
struct OnDiskIdentity {
    format: String,
    version: u32,
    secret_key: String,
    node_id: String,
}

pub(crate) fn identity_path() -> PathBuf {
    repo::identity::heddle_home_dir().join(IDENTITY_FILE)
}

pub(crate) fn load() -> Result<Option<AgentNodeIdentity>> {
    load_at(&identity_path())
}

/// Load the agent node identity, minting it exactly once when absent.
///
/// The write lock serializes first use by concurrent CLI processes. A corrupt
/// or exposed identity is never replaced: silently doing so would print a new
/// node id while old claim links still name the lost one.
pub(crate) fn load_or_create() -> Result<AgentNodeIdentity> {
    load_or_create_at(&identity_path())
}

fn load_or_create_at(path: &Path) -> Result<AgentNodeIdentity> {
    if let Some(parent) = path.parent() {
        objects::fs_atomic::create_private_dir_all(parent)
            .with_context(|| format!("creating agent credential directory {}", parent.display()))?;
    }
    let lock = identity_lock(path);
    let _guard = lock
        .write()
        .map_err(|error| anyhow::anyhow!("acquiring agent node identity lock: {error}"))?;
    if let Some(identity) = load_at(path)? {
        return Ok(identity);
    }

    let identity = AgentNodeIdentity {
        secret_key: SecretKey::generate(),
    };
    persist_at(path, &identity)?;
    Ok(identity)
}

fn load_at(path: &Path) -> Result<Option<AgentNodeIdentity>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!(
                "agent node identity {} is not a regular file",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting agent node identity {}", path.display()));
        }
    }

    crypto::reject_group_or_world_readable_key(path)
        .with_context(|| format!("refusing exposed agent node identity {}", path.display()))?;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading agent node identity {}", path.display()))?;
    let stored: OnDiskIdentity = toml::from_str(&contents)
        .with_context(|| format!("parsing agent node identity {}", path.display()))?;
    if stored.format != IDENTITY_FORMAT {
        bail!("{} is not a Heddle agent node identity", path.display());
    }
    if stored.version != IDENTITY_VERSION {
        bail!(
            "agent node identity {} has unsupported version {}",
            path.display(),
            stored.version
        );
    }

    let secret_bytes = hex::decode(&stored.secret_key)
        .with_context(|| format!("decoding secret key in {}", path.display()))?;
    let secret_bytes: [u8; 32] = secret_bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::anyhow!(
            "agent node identity {} has a {}-byte secret key; expected 32",
            path.display(),
            bytes.len()
        )
    })?;
    let secret_key = SecretKey::from_bytes(&secret_bytes);
    let node_id = secret_key.public();
    if stored.node_id != node_id.to_string() {
        bail!(
            "agent node identity {} has a public node id that does not match its secret key",
            path.display()
        );
    }
    Ok(Some(AgentNodeIdentity { secret_key }))
}

fn persist_at(path: &Path, identity: &AgentNodeIdentity) -> Result<()> {
    if let Some(parent) = path.parent() {
        objects::fs_atomic::create_private_dir_all(parent)
            .with_context(|| format!("creating agent credential directory {}", parent.display()))?;
    }
    let stored = OnDiskIdentity {
        format: IDENTITY_FORMAT.to_string(),
        version: IDENTITY_VERSION,
        secret_key: hex::encode(identity.secret_key.to_bytes()),
        node_id: identity.node_id().to_string(),
    };
    let contents = toml::to_string_pretty(&stored).context("serializing agent node identity")?;
    write_file_atomic_secret(path, contents.as_bytes())
        .with_context(|| format!("writing agent node identity {}", path.display()))?;
    Ok(())
}

fn identity_lock(identity_path: &Path) -> RepoLock {
    let parent = identity_path.parent().unwrap_or_else(|| Path::new("."));
    RepoLock::at(parent.join("locks").join("agent-node-identity.lock"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn clean_agent_home_is_created_on_first_load() {
        let temp = TempDir::new().expect("temp dir");
        let home = temp.path().join("new-heddle-home");
        let path = home.join(IDENTITY_FILE);

        let identity = load_or_create_at(&path).expect("mint identity in clean home");

        assert!(path.is_file());
        assert_eq!(
            identity.node_id(),
            load_or_create_at(&path).expect("reload identity").node_id()
        );
    }

    #[test]
    fn node_id_survives_independent_loads() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(IDENTITY_FILE);

        let first = load_or_create_at(&path).expect("mint identity");
        let second = load_or_create_at(&path).expect("reload identity");

        assert_eq!(first.node_id(), second.node_id());
        assert_eq!(
            first.secret_key().to_bytes(),
            second.secret_key().to_bytes()
        );
    }

    #[test]
    fn node_id_is_the_same_lower_hex_ed25519_public_key_used_for_consent() {
        let temp = TempDir::new().expect("temp dir");
        let identity = load_or_create_at(&temp.path().join(IDENTITY_FILE)).expect("identity");
        let signer = crypto::Ed25519Signer::from_seed(&identity.secret_key().to_bytes())
            .expect("consent signer");
        let node_id = identity.node_id().to_string();
        assert_eq!(node_id.len(), 64);
        assert!(
            node_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(node_id, hex::encode(crypto::Signer::public_key(&signer)));
    }

    #[test]
    fn concurrent_first_loads_choose_one_identity() {
        let temp = TempDir::new().expect("temp dir");
        let path = Arc::new(temp.path().join(IDENTITY_FILE));
        let barrier = Arc::new(Barrier::new(8));
        let loads = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_create_at(&path).expect("load identity").node_id()
                })
            })
            .collect::<Vec<_>>();
        let node_ids = loads
            .into_iter()
            .map(|load| load.join().expect("load thread"))
            .collect::<Vec<_>>();

        assert!(node_ids.iter().all(|node_id| *node_id == node_ids[0]));
    }

    #[test]
    fn corrupt_identity_is_refused_without_replacement() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(IDENTITY_FILE);
        let identity = load_or_create_at(&path).expect("mint identity");
        let mut stored: OnDiskIdentity =
            toml::from_str(&std::fs::read_to_string(&path).expect("read identity"))
                .expect("parse identity");
        stored.node_id = SecretKey::generate().public().to_string();
        let corrupt = toml::to_string_pretty(&stored).expect("encode corrupt identity");
        std::fs::write(&path, corrupt).expect("corrupt identity");

        let error = load_or_create_at(&path).expect_err("mismatch must fail closed");
        assert!(error.to_string().contains("does not match"));
        let after: OnDiskIdentity =
            toml::from_str(&std::fs::read_to_string(&path).expect("read after refusal"))
                .expect("parse after refusal");
        assert_ne!(after.node_id, identity.node_id().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn exposed_identity_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(IDENTITY_FILE);
        load_or_create_at(&path).expect("mint identity");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("expose identity");

        let error = load_or_create_at(&path).expect_err("exposed key must fail closed");
        assert!(error.to_string().contains("refusing exposed"));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_identity_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(IDENTITY_FILE);
        load_or_create_at(&path).expect("mint identity");

        let mode = std::fs::metadata(path)
            .expect("identity metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
