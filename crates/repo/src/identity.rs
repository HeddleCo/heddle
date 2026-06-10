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
    link_device_key_at(&device_identity_path(), public_key, private_key_pem, server)
}

/// Write the device identity at `path` while holding the device write-lock, so
/// the link serializes with logout's [`unlink_device_key`] (heddle#482). The
/// shared lock closes a TOCTOU window: without it, a concurrent logout that read
/// a *matching* identity could `remove_file` the identity this link atomically
/// renames in — deleting a *different* server's key.
fn link_device_key_at(
    path: &Path,
    public_key: &[u8],
    private_key_pem: &str,
    server: &str,
) -> std::io::Result<()> {
    let _lock = acquire_device_lock(path)?;
    let identity = DeviceIdentity {
        public_key: hex::encode(public_key),
        private_key_pem: private_key_pem.to_string(),
        server: server.to_string(),
        linked_at: now_rfc3339(),
    };
    write_device(path, &identity)
}

/// Remove the recorded device signing identity when it belongs to `server` —
/// the inverse of [`link_device_key`], called by `heddle auth logout`.
///
/// Reads the device identity's recorded `server` and deletes
/// `<heddle_home>/device-identity.toml` only when it matches the logged-out
/// server; a device identity bound to a *different* server is left intact.
/// Returns `true` if a matching identity was removed, `false` if there was
/// nothing to remove (no file, or it belongs to another server).
///
/// Fail-closed (heddle#482): if a matching identity file is present but cannot
/// be deleted, the error propagates so the logout reports the device key as
/// still on disk rather than falsely claiming a clean removal. Leaving it would
/// let [`resolve_signer`] keep preferring the logged-out private key for every
/// subsequent capture.
///
/// Race-safe (heddle#482): the whole read→compare→remove runs under the device
/// write-lock that [`link_device_key`] also takes, and the on-disk `server` is
/// RE-READ under that lock immediately before the remove. A concurrent
/// `auth login` that atomically swaps in a *different* server's identity can
/// therefore never be the casualty of this unlink — the revalidation sees the
/// new server and leaves it intact.
pub fn unlink_device_key(server: &str) -> std::io::Result<bool> {
    unlink_device_key_at(&device_identity_path(), server)
}

fn unlink_device_key_at(path: &Path, server: &str) -> std::io::Result<bool> {
    // Hold the device write-lock across the entire decision: a concurrent login
    // takes the same lock in `link_device_key_at`, so the file cannot be swapped
    // between our read and our remove (heddle#482 TOCTOU).
    let _lock = acquire_device_lock(path)?;
    // RE-READ under the lock and remove ONLY if the on-disk identity STILL
    // belongs to the logged-out server — never delete one a login swapped in.
    let Some(identity) = read_device_record(path)? else {
        return Ok(false);
    };
    if identity.server != server {
        return Ok(false);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        // Lost a race with another remover — the key is gone, which is the goal.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Process-wide write lock serializing device-identity mutations — login's
/// [`link_device_key`] and logout's [`unlink_device_key`] (heddle#482). The lock
/// file sits beside the device identity, derived from its parent, so the global
/// path (`<heddle_home>/locks/device-identity.lock`) and any test path stay in
/// lockstep with whichever `device-identity.toml` they guard.
fn device_lock(device_path: &Path) -> objects::lock::RepoLock {
    let dir = device_path.parent().unwrap_or_else(|| Path::new("."));
    objects::lock::RepoLock::at(dir.join("locks").join("device-identity.lock"))
}

fn acquire_device_lock(device_path: &Path) -> std::io::Result<objects::lock::WriteLockGuard> {
    device_lock(device_path)
        .write()
        .map_err(|error| std::io::Error::other(format!("acquiring device-identity lock: {error}")))
}

/// Read the device-identity record at `path` for the logout/unlink decision,
/// WITHOUT the signing-path permission gate. [`load_device`] refuses an
/// insecure (group/world-readable) file because it must not be *trusted* for
/// signing; logout instead needs to *remove* it, and an exposed key is all the
/// more reason to delete it. Returns `None` when the file is absent.
fn read_device_record(path: &Path) -> std::io::Result<Option<DeviceIdentity>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents).map(Some).map_err(|error| {
            std::io::Error::other(format!("parsing {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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

/// Load the per-repo local signing key at `path` as a ready signer, WITHOUT
/// minting one when absent. Returns `None` when there is no local key, it is
/// unreadable, or its PEM can't be parsed.
///
/// Used to recognise a state signed with the per-repo local key as
/// owner-reproducible even after `auth login` has linked a device key that now
/// supersedes it for new states (heddle#570): such a state is still ours to
/// re-sign, and minting a fresh local key here would only ever produce a
/// non-matching key, so the lookup is load-only.
pub fn load_local_signer(local_path: &Path) -> Option<Box<dyn Signer>> {
    let local = load_local(local_path).ok().flatten()?;
    signer_from_pem(&local.private_key_pem).ok()
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

/// Reject an identity TOML whose embedded private key is exposed by
/// group/world-readable permissions (heddle#482). Single-sources the key-file
/// signer loader's rule via [`crypto::reject_group_or_world_readable_key`], so
/// auto-signing trusts an identity file only when it is as locked-down as a
/// `heddle sign --key` key file. A no-op when the file is absent (the mint
/// path handles that) or on platforms without a unix permission model.
fn reject_insecure_identity(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    crypto::reject_group_or_world_readable_key(path)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error))
}

fn load_local(path: &Path) -> std::io::Result<Option<LocalIdentity>> {
    // Refuse an exposed private key before reading its bytes — fail closed
    // rather than sign with a key any local user could have copied.
    reject_insecure_identity(path)?;
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
    // Same fail-closed permission gate as the local identity: an exposed
    // device key must not be trusted for auto-signing.
    reject_insecure_identity(path)?;
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

    /// A `0600` identity loads; loosening it to a group/world-readable mode
    /// (e.g. a backup restore that didn't preserve perms) makes the loader
    /// fail closed with the same insecure-permission refusal the key-file
    /// signer loader raises — never signing with the exposed key (heddle#482).
    #[cfg(unix)]
    #[test]
    fn load_local_rejects_group_or_world_readable_identity() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("identity.toml");

        // Minted 0600 — loads fine.
        load_or_mint_local(&path).expect("mint local identity");
        assert!(load_local(&path).expect("secure identity loads").is_some());

        // Loosen to world-readable.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen perms");

        let err =
            load_local(&path).expect_err("group/world-readable identity must be rejected");
        let signer_err = err
            .get_ref()
            .and_then(|source| source.downcast_ref::<SignerError>());
        assert!(
            matches!(signer_err, Some(SignerError::InsecureKeyPermissions { .. })),
            "rejection must be the insecure-permission refusal, got {err:?}",
        );

        // Fail closed: the resolver yields no signer rather than signing with
        // the exposed key, so a capture would be unsigned-but-marked.
        let absent_device = temp.path().join("absent-device.toml");
        assert!(
            resolve_signer(&path, &absent_device).is_none(),
            "an exposed local key must degrade to no signer, never sign",
        );

        // Tightening back to 0600 restores loadability.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("tighten perms");
        assert!(load_local(&path).expect("re-secured identity loads").is_some());
    }

    fn write_device_for(path: &Path, server: &str) -> Ed25519Signer {
        let signer = Ed25519Signer::generate().expect("device keypair");
        write_device(
            path,
            &DeviceIdentity {
                public_key: hex::encode(signer.public_key()),
                private_key_pem: signer.to_pem().expect("device pem"),
                server: server.to_string(),
                linked_at: now_rfc3339(),
            },
        )
        .expect("write device identity");
        signer
    }

    /// `auth logout S` removes the device identity recorded for `S`, and the
    /// resolver then falls back to the per-repo local key — the logged-out
    /// device key no longer signs (heddle#482). Pre-fix, logout never touched
    /// `device-identity.toml`, so the device key persisted and kept signing.
    #[test]
    fn unlink_removes_matching_server_device_identity_then_falls_back_to_local() {
        let temp = TempDir::new().expect("temp dir");
        let local = temp.path().join("identity.toml");
        let device = temp.path().join("device-identity.toml");

        let device_signer = write_device_for(&device, "grpc.S");
        let device_pubkey = hex::encode(device_signer.public_key());

        // Before logout the resolver prefers the device key.
        let pre = resolve_signer(&local, &device).expect("device signer");
        assert_eq!(hex::encode(pre.public_key()), device_pubkey);

        // Logout for the matching server removes the device identity.
        let removed = unlink_device_key_at(&device, "grpc.S").expect("unlink");
        assert!(removed, "matching-server device identity must be removed");
        assert!(!device.exists(), "device-identity file must be gone after logout");

        // The resolver now mints/uses the per-repo local key (a distinct key),
        // so the logged-out device key can no longer sign new states.
        let post = resolve_signer(&local, &device).expect("local signer");
        assert_ne!(
            hex::encode(post.public_key()),
            device_pubkey,
            "after logout the device key must no longer sign; resolver falls back to local",
        );
    }

    /// Logging out of a *different* server must not remove another server's
    /// device identity (heddle#482) — the unlink is gated on the recorded
    /// `server` field matching.
    #[test]
    fn unlink_leaves_non_matching_server_device_identity_intact() {
        let temp = TempDir::new().expect("temp dir");
        let device = temp.path().join("device-identity.toml");

        let device_signer = write_device_for(&device, "grpc.S");
        let device_pubkey = hex::encode(device_signer.public_key());

        let removed = unlink_device_key_at(&device, "grpc.OTHER").expect("unlink");
        assert!(
            !removed,
            "logging out of a different server must not remove this device identity",
        );
        assert!(device.exists(), "non-matching device identity must remain on disk");

        let still = load_device(&device).expect("load").expect("present");
        assert_eq!(still.public_key, device_pubkey);
        assert_eq!(still.server, "grpc.S");
    }

    /// Logout is a no-op (returns `false`, no error) when there is no device
    /// identity to remove — e.g. a device that only ever used the local key.
    #[test]
    fn unlink_is_noop_when_no_device_identity() {
        let temp = TempDir::new().expect("temp dir");
        let device = temp.path().join("device-identity.toml");
        let removed = unlink_device_key_at(&device, "grpc.S").expect("unlink");
        assert!(!removed, "nothing to remove when no device identity exists");
    }

    /// Probe whether the test runs as root: root bypasses unix directory
    /// permission bits, so the read-only-dir simulation below can't actually
    /// block a removal. Detect by checking if a `0o500` dir is still writable.
    #[cfg(unix)]
    fn running_as_root() -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = TempDir::new().expect("probe temp");
        let locked = probe.path().join("locked");
        std::fs::create_dir(&locked).expect("probe dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
            .expect("lock probe");
        let writable = std::fs::write(locked.join("x"), b"x").is_ok();
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));
        writable
    }

    /// Fail-closed (heddle#482): when a device identity matches the logged-out
    /// server but its file cannot be removed (here: a read-only parent dir),
    /// `unlink_device_key` returns an error and leaves the file in place, so
    /// logout surfaces the incompleteness instead of falsely reporting a clean
    /// removal while the private key is still on disk.
    #[cfg(unix)]
    #[test]
    fn unlink_fails_closed_when_matching_file_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt;

        if running_as_root() {
            // Root bypasses the read-only-dir guard; this simulation only holds
            // for an unprivileged user. The happy-path + no-op tests still
            // cover removal semantics under root.
            return;
        }

        let temp = TempDir::new().expect("temp dir");
        let dir = temp.path().join("home");
        std::fs::create_dir(&dir).expect("home dir");
        let device = dir.join("device-identity.toml");
        write_device_for(&device, "grpc.S");

        // Prime the device lock (materialises `<dir>/locks/device-identity.lock`)
        // while `dir` is still writable, so the unlink can ACQUIRE the lock and
        // the only thing that fails is the removal itself — this exercises the
        // fail-closed remove path under a held lock, not lock setup.
        drop(device_lock(&device).write().expect("prime device lock"));

        // Read-only parent dir: the file is still readable (r-x) but cannot be
        // unlinked (removal needs write on the directory).
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("lock dir");

        let result = unlink_device_key_at(&device, "grpc.S");

        // Restore perms before asserting so the TempDir can clean up.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore perms");

        assert!(
            result.is_err(),
            "a matching device key that cannot be removed must surface an error, not a clean removal",
        );
        assert!(
            device.exists(),
            "the logged-out key is still on disk after the failed removal",
        );
    }

    /// Race-safety (heddle#482 TOCTOU): a logout for server A, racing a fresh
    /// login for server B that atomically replaces the file, must NEVER delete
    /// B's identity. Because link and unlink both take the device write-lock,
    /// the logout either removes A's file before B is linked, or re-reads B
    /// under the lock and no-ops — so in EVERY interleaving B's freshly-linked
    /// identity is the survivor. Pre-fix, the logout's pre-lock read of A could
    /// `remove_file` the B file that the login had just renamed in.
    #[test]
    fn concurrent_login_and_logout_never_delete_a_different_servers_identity() {
        for _ in 0..64 {
            let temp = TempDir::new().expect("temp dir");
            let device = temp.path().join("device-identity.toml");

            // The file starts as server A's identity.
            write_device_for(&device, "grpc.A");

            // Mint B's keypair up front so the login thread just writes it.
            let b = Ed25519Signer::generate().expect("b keypair");
            let b_pubkey = hex::encode(b.public_key());
            let b_pub = b.public_key().to_vec();
            let b_pem = b.to_pem().expect("b pem");

            let logout_path = device.clone();
            let logout =
                std::thread::spawn(move || unlink_device_key_at(&logout_path, "grpc.A"));

            let login_path = device.clone();
            let login = std::thread::spawn(move || {
                link_device_key_at(&login_path, &b_pub, &b_pem, "grpc.B")
            });

            logout.join().expect("logout thread").expect("unlink ok");
            login.join().expect("login thread").expect("link ok");

            // B's identity must survive every interleaving — it is never the
            // unlink's casualty.
            let surviving = load_device(&device)
                .expect("load device")
                .expect("the concurrent login's identity must survive");
            assert_eq!(surviving.server, "grpc.B");
            assert_eq!(
                surviving.public_key, b_pubkey,
                "the concurrent login's identity must never be deleted by the unlink",
            );
        }
    }

    /// The device-identity load path enforces the same permission gate, so a
    /// group/world-readable device key is refused before signing (heddle#482).
    #[cfg(unix)]
    #[test]
    fn load_device_rejects_group_or_world_readable_identity() {
        use std::os::unix::fs::PermissionsExt;

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

        // Written 0600 by `write_file_atomic_secret` — loads fine.
        assert!(load_device(&path).expect("secure device loads").is_some());

        // Loosen to group-readable -> rejected with the insecure-permission
        // refusal, and no key bytes are trusted for signing.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("loosen perms");
        let err =
            load_device(&path).expect_err("group-readable device identity must be rejected");
        let signer_err = err
            .get_ref()
            .and_then(|source| source.downcast_ref::<SignerError>());
        assert!(
            matches!(signer_err, Some(SignerError::InsecureKeyPermissions { .. })),
            "rejection must be the insecure-permission refusal, got {err:?}",
        );
    }
}
