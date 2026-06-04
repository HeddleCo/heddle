// SPDX-License-Identifier: Apache-2.0
//! The machine's signing identity — the ed25519 key Heddle uses to auto-sign
//! every captured state.
//!
//! Two key objects, resolved in precedence order:
//!
//! 1. **Device identity** (`<heddle_home>/device-identity.toml`) — the
//!    ed25519 device-binding key minted by `heddle auth login`. Recorded here
//!    at login so the offline capture path can sign with it without a network
//!    round-trip or a dependency on the hosted-client crate. When present it
//!    *supersedes* the local key for new states.
//! 2. **Local identity** (`<heddle_dir>/identity.toml`) — a per-repo ed25519
//!    key auto-minted on first capture in a repo that has never authenticated.
//!    Guarantees universal signing offline, with zero user-managed keys.
//!
//! Both key files are written `0600` via `write_file_atomic_secret`, the same
//! protection the credential store uses. Nothing in this module ever logs key
//! bytes.
//!
//! The reconciliation model (heddle#482): the device key supersedes the local
//! key for *new* states; both are recorded (the local key stays in the repo's
//! `identity.toml`, the device key in the global `device-identity.toml`).
//! States already signed by a local key keep verifying forever — their public
//! key is embedded in the state and verification recomputes the content hash,
//! so it needs neither the private key nor any registry.

use std::path::{Path, PathBuf};

use crypto::{Ed25519Signer, Signer, SignerError};
use objects::fs_atomic::write_file_atomic_secret;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// File name for the per-repo local identity, under `<heddle_dir>`.
pub const LOCAL_IDENTITY_FILE: &str = "identity.toml";
/// File name for the global device identity, under `<heddle_home>`.
pub const DEVICE_IDENTITY_FILE: &str = "device-identity.toml";

/// Per-repo auto-minted signing identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalIdentity {
    /// Hex-encoded ed25519 public key.
    pub public_key: String,
    /// PKCS#8 PEM private key. `0600` on disk.
    pub private_key_pem: String,
    /// RFC 3339 mint timestamp.
    pub created_at: String,
}

/// Globally-recorded device identity, written at `heddle auth login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Hex-encoded ed25519 public key (the server-registered device key).
    pub public_key: String,
    /// PKCS#8 PEM private key, copied from the credential store at login so
    /// the offline capture path can resolve it without re-reading credentials
    /// (which would couple `repo` to the hosted-client crate). Same `0600`
    /// protection and same `~/.heddle` directory as `credentials.toml`.
    pub private_key_pem: String,
    /// The server this device key authenticated against.
    pub server: String,
    /// RFC 3339 link timestamp.
    pub linked_at: String,
}

/// `<heddle_home>` — `$HEDDLE_HOME` if set, else `$HOME/.heddle`, else
/// `./.heddle`. Mirrors the credential store location so the device identity
/// sits beside `credentials.toml`. `HEDDLE_HOME` exists primarily as a test
/// and power-user override; production resolves to `$HOME/.heddle`.
pub fn heddle_home_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os("HEDDLE_HOME") {
        return PathBuf::from(explicit);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".heddle")
}

/// Path to the global device identity file.
pub fn device_identity_path() -> PathBuf {
    heddle_home_dir().join(DEVICE_IDENTITY_FILE)
}

/// Record the device key as the machine's active signing identity. Called by
/// `heddle auth login` after the device keypair is minted and registered with
/// the server. The device key supersedes any per-repo local key for *new*
/// states; states already signed by a local key keep verifying (their public
/// key is embedded in the state).
pub fn link_device_key(
    public_key: &[u8],
    private_key_pem: &str,
    server: &str,
) -> std::io::Result<()> {
    let identity = DeviceIdentity {
        public_key: hex::encode(public_key),
        private_key_pem: private_key_pem.to_string(),
        server: server.to_string(),
        linked_at: now_rfc3339(),
    };
    write_device(&device_identity_path(), &identity)
}

/// Resolve the active signing key: the device key if one has been linked,
/// otherwise the per-repo local key (minted on first call). Returns `None`
/// only when no key can be produced (e.g. an unwritable home), so the caller
/// degrades to an unsigned-but-marked state instead of failing the capture.
pub fn resolve_signer(local_path: &Path, device_path: &Path) -> Option<Box<dyn Signer>> {
    match load_device(device_path) {
        Ok(Some(device)) => match signer_from_pem(&device.private_key_pem) {
            Ok(signer) => return Some(signer),
            Err(error) => warn!(
                %error,
                "device signing key unreadable; falling back to local identity"
            ),
        },
        Ok(None) => {}
        Err(error) => warn!(
            %error,
            "device identity unreadable; falling back to local identity"
        ),
    }

    match load_or_mint_local(local_path) {
        Ok(local) => match signer_from_pem(&local.private_key_pem) {
            Ok(signer) => Some(signer),
            Err(error) => {
                warn!(%error, "local signing key unreadable; capturing unsigned");
                None
            }
        },
        Err(error) => {
            warn!(%error, "could not mint local signing identity; capturing unsigned");
            None
        }
    }
}

/// Load the per-repo local identity at `path`, minting and persisting a fresh
/// ed25519 key if the file is absent.
pub fn load_or_mint_local(path: &Path) -> std::io::Result<LocalIdentity> {
    if let Some(existing) = load_local(path)? {
        return Ok(existing);
    }
    let signer = Ed25519Signer::generate()
        .map_err(|error| std::io::Error::other(format!("ed25519 keygen failed: {error}")))?;
    let pem = signer
        .to_pem()
        .map_err(|error| std::io::Error::other(format!("ed25519 PEM export failed: {error}")))?;
    let identity = LocalIdentity {
        public_key: hex::encode(signer.public_key()),
        private_key_pem: pem,
        created_at: now_rfc3339(),
    };
    persist_local(path, &identity)?;
    Ok(identity)
}

fn load_local(path: &Path) -> std::io::Result<Option<LocalIdentity>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map(Some)
            .map_err(|error| std::io::Error::other(format!("parsing {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn persist_local(path: &Path, identity: &LocalIdentity) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(identity)
        .map_err(|error| std::io::Error::other(format!("serializing local identity: {error}")))?;
    write_file_atomic_secret(path, contents.as_bytes())
}

/// Load the global device identity at `path`, if present.
pub fn load_device(path: &Path) -> std::io::Result<Option<DeviceIdentity>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map(Some)
            .map_err(|error| std::io::Error::other(format!("parsing {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_device(path: &Path, identity: &DeviceIdentity) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(identity)
        .map_err(|error| std::io::Error::other(format!("serializing device identity: {error}")))?;
    write_file_atomic_secret(path, contents.as_bytes())
}

fn signer_from_pem(pem: &str) -> Result<Box<dyn Signer>, SignerError> {
    Ed25519Signer::from_pem(pem).map(|signer| Box::new(signer) as Box<dyn Signer>)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use crypto::StateSigningExt;
    use objects::object::{Attribution, Principal, State, Tree};
    use tempfile::TempDir;

    use super::*;

    fn signed_state_with(signer: &dyn Signer) -> State {
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let mut state = State::new(Tree::new().hash(), vec![], attribution);
        state.sign(signer).expect("sign state");
        state
    }

    #[test]
    fn mint_is_idempotent_and_stable() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("identity.toml");

        let first = load_or_mint_local(&path).expect("mint local identity");
        assert!(path.exists(), "identity file should be created");
        let second = load_or_mint_local(&path).expect("reload local identity");
        assert_eq!(
            first.public_key, second.public_key,
            "second load must reuse the minted key, not re-mint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn minted_local_key_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("identity.toml");
        load_or_mint_local(&path).expect("mint local identity");

        let mode = std::fs::metadata(&path)
            .expect("identity metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "private key file must be 0600");
    }

    #[test]
    fn resolve_mints_local_when_no_device() {
        let temp = TempDir::new().expect("temp dir");
        let local = temp.path().join("identity.toml");
        let device = temp.path().join("device-identity.toml");

        let signer = resolve_signer(&local, &device).expect("resolve a local signer");
        let state = signed_state_with(signer.as_ref());
        state
            .verify_signature()
            .expect("locally-signed state must verify");

        // The resolved key is the persisted local key.
        let persisted = load_local(&local).expect("read local").expect("present");
        assert_eq!(persisted.public_key, hex::encode(signer.public_key()));
    }

    #[test]
    fn device_supersedes_local_and_prior_local_still_verifies() {
        let temp = TempDir::new().expect("temp dir");
        let local = temp.path().join("identity.toml");
        let device = temp.path().join("device-identity.toml");

        // First capture: no device key yet -> local key signs.
        let local_signer = resolve_signer(&local, &device).expect("local signer");
        let local_pubkey = hex::encode(local_signer.public_key());
        let prior_state = signed_state_with(local_signer.as_ref());
        prior_state.verify_signature().expect("local state verifies");

        // Reconcile: record a device key (a distinct keypair).
        let device_signer = Ed25519Signer::generate().expect("device keypair");
        write_device(
            &device,
            &DeviceIdentity {
                public_key: hex::encode(device_signer.public_key()),
                private_key_pem: device_signer.to_pem().expect("device pem"),
                server: "grpc.example".to_string(),
                linked_at: now_rfc3339(),
            },
        )
        .expect("link device key");

        // Subsequent capture: device key supersedes local.
        let resolved = resolve_signer(&local, &device).expect("device signer");
        assert_eq!(
            hex::encode(resolved.public_key()),
            hex::encode(device_signer.public_key()),
            "device key must supersede the local key for new states"
        );
        assert_ne!(
            hex::encode(resolved.public_key()),
            local_pubkey,
            "device key is distinct from the local key"
        );

        // The prior local-signed state still verifies — its public key is
        // embedded, so reconciliation does not invalidate it.
        prior_state
            .verify_signature()
            .expect("prior local-signed state still verifies after device link");
    }

    #[test]
    fn resolve_returns_none_when_local_mint_fails() {
        let temp = TempDir::new().expect("temp dir");
        // A regular file where the local-identity *parent directory* should be,
        // so `create_dir_all`/`read_to_string` can't produce a key.
        let blocker = temp.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let local = blocker.join("identity.toml");
        let device = temp.path().join("absent-device.toml");

        assert!(
            resolve_signer(&local, &device).is_none(),
            "an unmintable local key must degrade to None, not panic",
        );
    }

    #[test]
    fn resolve_falls_back_to_local_when_device_key_is_corrupt() {
        let temp = TempDir::new().expect("temp dir");
        let local = temp.path().join("identity.toml");
        let device = temp.path().join("device-identity.toml");

        // A device record whose PEM cannot be parsed.
        write_device(
            &device,
            &DeviceIdentity {
                public_key: "deadbeef".to_string(),
                private_key_pem: "not a valid pem".to_string(),
                server: "grpc.example".to_string(),
                linked_at: now_rfc3339(),
            },
        )
        .expect("write device identity");

        let signer = resolve_signer(&local, &device).expect("fall back to local signer");
        let persisted = load_local(&local).expect("read local").expect("present");
        assert_eq!(
            persisted.public_key,
            hex::encode(signer.public_key()),
            "an unreadable device key falls back to the local identity",
        );
    }

    #[test]
    fn device_roundtrips_through_disk() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("device-identity.toml");
        let signer = Ed25519Signer::generate().expect("keypair");

        write_device(
            &path,
            &DeviceIdentity {
                public_key: hex::encode(signer.public_key()),
                private_key_pem: signer.to_pem().expect("pem"),
                server: "grpc.example".to_string(),
                linked_at: now_rfc3339(),
            },
        )
        .expect("write device identity");

        let loaded = load_device(&path).expect("load").expect("present");
        assert_eq!(loaded.public_key, hex::encode(signer.public_key()));
        assert_eq!(loaded.server, "grpc.example");
    }
}
