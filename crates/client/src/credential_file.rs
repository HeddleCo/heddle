//! Single-file `.hcred` credential artifact.
//!
//! A `.hcred` file collapses the two coupled artifacts a non-browser Heddle
//! credential used to require — a biscuit `token` and its device-key PEM —
//! into one self-verifying JSON object. It is packaging only: the token and
//! proof key the server validates are unchanged, and so is the offline,
//! no-server-round-trip property of `derive-agent`.
//!
//! # Trust boundary
//!
//! There is exactly ONE verifying load chokepoint, [`load_credential_file`].
//! It never trusts the file's self-reported `subject`/`expires_at` for
//! authorization: it re-derives the effective proof key and subject from the
//! token bytes (the same walk the hosted server performs) and rejects any file
//! whose `proof_key_pem` or `subject` disagrees, or whose token has already
//! expired. The `provenance` block is audit-only and is NEVER consulted for
//! enforcement.
//!
//! # Secret handling
//!
//! [`VerifiedCredential`] is the in-memory shape. It has a MANUAL [`Debug`]
//! impl that redacts `token`/`proof_key_pem` and deliberately derives no
//! [`serde::Serialize`], so the secret material cannot leak into logs,
//! tracing, or `--output json`. Serialization to disk goes through the private
//! [`OnDiskCredential`] serde mirror, reachable only via
//! [`write_credential_file`].

use std::{
    fmt,
    fs::File,
    io::Read,
    path::Path,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use crypto::{Ed25519Signer, Signer};
use serde::{Deserialize, Serialize};

/// Stable magic string in every `.hcred` file.
const CREDENTIAL_FORMAT: &str = "heddle-credential";
/// Only on-disk schema version this build reads and writes.
const CREDENTIAL_VERSION: u32 = 1;
/// Upper bound on a `.hcred` file read. A credential file is tiny (a token,
/// a PEM, and a little metadata); this cap keeps a planted huge file at a
/// `--credential` / `HEDDLE_CREDENTIAL` path from ballooning memory on load.
const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;

/// The role a `.hcred` credential plays. Audit/provenance metadata only — the
/// server enforces authority from the token, not this label.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    /// An offline-derived, TTL/operation/scope-limited child credential.
    Agent,
    /// A namespace-scoped service-account credential for CI/automation.
    Service,
    /// An operator-provisioned, device-bound bootstrap credential.
    Device,
}

/// Audit-only provenance recorded beside a derived credential. NEVER trusted
/// for enforcement — the token's own attenuation blocks are the authority.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CredentialProvenance {
    /// The `--template` preset a derived agent was built from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Declared resource scopes (`repo:org/name`, `namespace:org`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// The exact operation ceiling recorded in the token's attenuation block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_operations: Option<Vec<String>>,
    /// The agent id recorded in the delegation chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl CredentialProvenance {
    /// Whether this provenance carries any recorded field.
    fn is_empty(&self) -> bool {
        self.template.is_none()
            && self.scopes.is_none()
            && self.allowed_operations.is_none()
            && self.agent_id.is_none()
    }
}

/// A cryptographically verified credential, in memory.
///
/// Obtained ONLY from [`load_credential_file`] (which re-verifies against the
/// token) or constructed by a writer that just minted the token/key. The
/// `token` and `proof_key_pem` fields are secrets — this type intentionally
/// does not implement [`serde::Serialize`] and redacts them in [`Debug`].
pub struct VerifiedCredential {
    /// Server address the credential authenticates against.
    pub server: String,
    /// Credential role (audit only).
    pub kind: CredentialKind,
    /// Authenticated subject, re-derived from the token on load.
    pub subject: String,
    /// Base64 biscuit token. Secret.
    pub token: String,
    /// PEM of the Ed25519 proof key bound to `token`. Secret.
    pub proof_key_pem: String,
    /// Effective RFC 3339 expiry, re-computed from the token on load. `None`
    /// for tokens the server never expires.
    pub expires_at: Option<String>,
    /// Authority credential id, when the token carries one (used for rotation).
    pub credential_id: Option<String>,
    /// Audit-only provenance. Never trusted for enforcement.
    pub provenance: Option<CredentialProvenance>,
}

impl fmt::Debug for VerifiedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedCredential")
            .field("server", &self.server)
            .field("kind", &self.kind)
            .field("subject", &self.subject)
            .field("token", &"<redacted>")
            .field("proof_key_pem", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("credential_id", &self.credential_id)
            .field("provenance", &self.provenance)
            .finish()
    }
}

impl VerifiedCredential {
    /// Map into the keystore's [`ServerCredential`] shape so a loaded `.hcred`
    /// round-trips through `credentials.toml`.
    pub fn into_server_credential(self) -> crate::credentials::ServerCredential {
        crate::credentials::ServerCredential {
            token: self.token,
            subject: self.subject,
            device_id: None,
            credential_id: self.credential_id,
            private_key_pem: Some(self.proof_key_pem),
            expires_at: self.expires_at,
        }
    }
}

/// Private on-disk serde mirror. The only path that serializes the secret
/// fields — kept separate from [`VerifiedCredential`] so those secrets can
/// never be emitted through an accidental `Serialize` on the in-memory type.
#[derive(Serialize, Deserialize)]
struct OnDiskCredential {
    format: String,
    version: u32,
    server: String,
    kind: CredentialKind,
    subject: String,
    token: String,
    proof_key_pem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    /// Always emitted (as `null` when absent) so the schema is stable.
    #[serde(default)]
    credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<CredentialProvenance>,
}

/// Write `credential` to `path` as a `0600` `.hcred` file via the atomic
/// secret writer. Refuses to overwrite an existing path (mirroring the old
/// bundle's dir-exists guard) so a writer never clobbers another credential.
pub fn write_credential_file(path: &Path, credential: &VerifiedCredential) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "credential destination {} already exists; choose a new --out path",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("checking credential destination {}", path.display()));
        }
    }

    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        objects::fs_atomic::create_private_dir_all(parent)
            .with_context(|| format!("creating credential parent {}", parent.display()))?;
    }

    let provenance = credential
        .provenance
        .as_ref()
        .filter(|provenance| !provenance.is_empty())
        .cloned();
    let on_disk = OnDiskCredential {
        format: CREDENTIAL_FORMAT.to_string(),
        version: CREDENTIAL_VERSION,
        server: credential.server.clone(),
        kind: credential.kind,
        subject: credential.subject.clone(),
        token: credential.token.clone(),
        proof_key_pem: credential.proof_key_pem.clone(),
        expires_at: credential.expires_at.clone(),
        credential_id: credential.credential_id.clone(),
        provenance,
    };

    let mut bytes = serde_json::to_vec_pretty(&on_disk).context("serializing credential file")?;
    bytes.push(b'\n');
    objects::fs_atomic::write_file_atomic_secret(path, &bytes)
        .with_context(|| format!("writing credential file {}", path.display()))?;
    Ok(())
}

/// Load and cryptographically verify a `.hcred` file — the single trusted
/// entry point for on-disk credentials.
///
/// Verification steps (all fail-closed):
/// 1. `format`/`version` match this build;
/// 2. a fail-closed permissions/ownership check (Unix): the file must be a
///    regular file (a symlink to a non-file target is refused) and must not be
///    group/other-accessible;
/// 3. the token's effective proof key equals `proof_key_pem`'s public key;
/// 4. the token's authenticated subject equals `subject`;
/// 5. the effective expiry re-computed from the token is not already past
///    (the file's `expires_at` is only a hint and is replaced by the
///    re-computed value).
pub fn load_credential_file(path: &Path) -> Result<VerifiedCredential> {
    // Open through the fail-closed security gate and read from the SAME handle
    // so the permission check and the read observe one inode (no TOCTOU).
    let file = open_credential_file_checked(path)?;
    // Cap the read: a `.hcred` is tiny, so reading one byte past the cap and
    // finding content there means the file is oversized — reject rather than
    // OOM on a planted huge file.
    let mut contents = String::new();
    let mut limited = file.take(MAX_CREDENTIAL_FILE_BYTES + 1);
    limited
        .read_to_string(&mut contents)
        .with_context(|| format!("reading credential file {}", path.display()))?;
    if contents.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        bail!(
            "credential file {} exceeds the {} KiB size cap; a credential file is tiny — refusing to load a suspiciously large file",
            path.display(),
            MAX_CREDENTIAL_FILE_BYTES / 1024,
        );
    }

    let on_disk: OnDiskCredential = serde_json::from_str(&contents)
        .with_context(|| format!("parsing credential file {}", path.display()))?;
    if on_disk.format != CREDENTIAL_FORMAT {
        bail!(
            "{} is not a Heddle credential file (format {:?}, expected {CREDENTIAL_FORMAT:?})",
            path.display(),
            on_disk.format
        );
    }
    if on_disk.version != CREDENTIAL_VERSION {
        bail!(
            "credential file {} has unsupported version {} (this build reads version {CREDENTIAL_VERSION})",
            path.display(),
            on_disk.version
        );
    }
    // The `server` field reaches terminal output (`auth status`, error
    // messages, the server-mismatch diagnostic). Reject control characters /
    // newlines so a crafted `.hcred` can't inject escape sequences there.
    if on_disk.server.chars().any(|c| c.is_control()) {
        bail!(
            "credential file {} has a server field containing control characters",
            path.display()
        );
    }

    // Re-derive the ground truth from the token bytes — never trust the file's
    // self-reported identity for authorization.
    let metadata = crate::auth_cmd::headless_token_metadata(&on_disk.token)
        .with_context(|| format!("verifying token in credential file {}", path.display()))?;

    let signer = Ed25519Signer::from_pem(&on_disk.proof_key_pem)
        .map_err(|error| anyhow::anyhow!("credential proof key is not a valid Ed25519 PEM: {error}"))?;
    let proof_public_key_hex = hex::encode(signer.public_key());
    if !metadata
        .proof_public_key_hex
        .eq_ignore_ascii_case(&proof_public_key_hex)
    {
        bail!(
            "credential file {} proof key does not match the token's effective proof key",
            path.display()
        );
    }

    if on_disk.subject != metadata.subject {
        bail!(
            "credential file {} claims subject {:?} but the token authenticates {:?}",
            path.display(),
            on_disk.subject,
            metadata.subject
        );
    }

    // Effective expiry comes from the token, not the file. Reject if past.
    if let Some(expires_at) = metadata.expires_at.as_deref() {
        let parsed = chrono::DateTime::parse_from_rfc3339(expires_at)
            .with_context(|| format!("parsing token expiry {expires_at}"))?
            .with_timezone(&Utc);
        if parsed <= Utc::now() {
            bail!(
                "credential file {} is expired (token expired at {expires_at})",
                path.display()
            );
        }
    }

    Ok(VerifiedCredential {
        server: on_disk.server,
        kind: on_disk.kind,
        subject: metadata.subject,
        token: on_disk.token,
        proof_key_pem: on_disk.proof_key_pem,
        expires_at: metadata.expires_at,
        // Prefer the file's explicit credential id; fall back to the one the
        // token carries so a device credential keeps its rotation anchor.
        credential_id: on_disk.credential_id.or(metadata.credential_id),
        provenance: on_disk.provenance,
    })
}

/// Open a credential file behind a fail-closed security gate.
///
/// Follows the path to its target (so a k8s-style symlinked secret mount still
/// works) but rejects a symlink to a NON-file target, and — on Unix — rejects
/// any file the group or others can access. The check runs against the opened
/// handle's metadata so the permission verdict and the later read see the same
/// inode.
fn open_credential_file_checked(path: &Path) -> Result<File> {
    let file = File::open(path)
        .with_context(|| format!("opening credential file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting credential file {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "credential file {} is not a regular file (refusing a symlink to a non-file target)",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "credential file {} is group/other-accessible (mode {mode:o}); run `chmod 600 {}` to restrict it to your user",
                path.display(),
                path.display()
            );
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::KeyPair;

    /// Mint a device-bound authority token whose effective PoP key is `signer`.
    fn mint_token(subject: &str, signer: &Ed25519Signer, ttl: chrono::Duration) -> String {
        let expires_at = Utc::now() + ttl;
        biscuit_auth::Biscuit::builder()
            .fact(format!("user({})", quote(subject)).as_str())
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("device PoP fact")
            .fact(format!("expires_at({})", expires_at.to_rfc3339()).as_str())
            .expect("expiry fact")
            .check(format!("check if time($now), $now < {}", expires_at.to_rfc3339()).as_str())
            .expect("expiry check")
            .build(&KeyPair::new())
            .expect("build token")
            .to_base64()
            .expect("encode token")
    }

    fn quote(s: &str) -> String {
        format!("\"{s}\"")
    }

    fn sample_verified() -> (VerifiedCredential, Ed25519Signer) {
        let signer = Ed25519Signer::generate().expect("proof key");
        let token = mint_token("alice", &signer, chrono::Duration::hours(2));
        let proof_key_pem = signer.to_pem().expect("proof PEM");
        (
            VerifiedCredential {
                server: "grpc.heddle.test".to_string(),
                kind: CredentialKind::Agent,
                subject: "alice".to_string(),
                token,
                proof_key_pem,
                expires_at: Some((Utc::now() + chrono::Duration::hours(2)).to_rfc3339()),
                credential_id: None,
                provenance: Some(CredentialProvenance {
                    template: Some("reviewer".to_string()),
                    scopes: Some(vec!["repo:acme/heddle".to_string()]),
                    allowed_operations: Some(vec!["GetState".to_string()]),
                    agent_id: Some("agent-1".to_string()),
                }),
            },
            signer,
        )
    }

    #[test]
    fn round_trips_write_then_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (credential, _signer) = sample_verified();
        write_credential_file(&path, &credential).expect("write");

        let loaded = load_credential_file(&path).expect("load");
        assert_eq!(loaded.server, "grpc.heddle.test");
        assert_eq!(loaded.subject, "alice");
        assert_eq!(loaded.kind, CredentialKind::Agent);
        assert_eq!(loaded.token, credential.token);
        assert_eq!(loaded.proof_key_pem, credential.proof_key_pem);
        assert!(loaded.expires_at.is_some(), "expiry re-derived from token");
        let provenance = loaded.provenance.expect("provenance preserved");
        assert_eq!(provenance.template.as_deref(), Some("reviewer"));
        assert_eq!(provenance.agent_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn refuses_to_overwrite_existing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (credential, _signer) = sample_verified();
        write_credential_file(&path, &credential).expect("first write");
        let error = write_credential_file(&path, &credential).expect_err("second write refused");
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn debug_redacts_secret_material() {
        let (credential, _signer) = sample_verified();
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(
            !rendered.contains(&credential.token),
            "token must never appear in Debug output"
        );
        assert!(
            !rendered.contains("BEGIN PRIVATE KEY"),
            "proof key PEM must never appear in Debug output"
        );
    }

    #[test]
    fn rejects_tampered_proof_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (mut credential, _signer) = sample_verified();
        // Swap in a mismatched proof key: no longer binds the token.
        let attacker = Ed25519Signer::generate().expect("attacker key");
        credential.proof_key_pem = attacker.to_pem().expect("attacker PEM");
        write_credential_file(&path, &credential).expect("write");
        let error = load_credential_file(&path).expect_err("mismatched key must be rejected");
        assert!(error.to_string().contains("proof key does not match"));
    }

    #[test]
    fn rejects_wrong_subject() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (mut credential, _signer) = sample_verified();
        credential.subject = "mallory".to_string();
        write_credential_file(&path, &credential).expect("write");
        let error = load_credential_file(&path).expect_err("wrong subject must be rejected");
        assert!(error.to_string().contains("authenticates"));
    }

    #[test]
    fn rejects_expired_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let signer = Ed25519Signer::generate().expect("proof key");
        let token = mint_token("alice", &signer, chrono::Duration::hours(-1));
        let credential = VerifiedCredential {
            server: "grpc.heddle.test".to_string(),
            kind: CredentialKind::Agent,
            subject: "alice".to_string(),
            token,
            proof_key_pem: signer.to_pem().expect("proof PEM"),
            expires_at: None,
            credential_id: None,
            provenance: None,
        };
        write_credential_file(&path, &credential).expect("write");
        let error = load_credential_file(&path).expect_err("expired token must be rejected");
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn rejects_bad_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (credential, _signer) = sample_verified();
        write_credential_file(&path, &credential).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        let tampered = contents.replace("heddle-credential", "not-a-credential");
        std::fs::write(&path, tampered).expect("rewrite");
        let error = load_credential_file(&path).expect_err("bad format must be rejected");
        assert!(error.to_string().contains("not a Heddle credential file"));
    }

    #[test]
    fn rejects_bad_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (credential, _signer) = sample_verified();
        write_credential_file(&path, &credential).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        let tampered = contents.replace("\"version\": 1", "\"version\": 2");
        std::fs::write(&path, tampered).expect("rewrite");
        let error = load_credential_file(&path).expect_err("bad version must be rejected");
        assert!(error.to_string().contains("unsupported version"));
    }

    #[test]
    fn rejects_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        // A file past the 64 KiB cap is rejected before any parse/verify.
        let bloat = "x".repeat((MAX_CREDENTIAL_FILE_BYTES as usize) + 1);
        std::fs::write(&path, bloat).expect("write oversized file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("tighten perms");
        }
        let error = load_credential_file(&path).expect_err("oversized file must be rejected");
        assert!(
            error.to_string().contains("size cap"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_control_chars_in_server_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (mut credential, _signer) = sample_verified();
        // A bell control character in `server` would reach terminal output.
        credential.server = "grpc.heddle\u{0007}.test".to_string();
        write_credential_file(&path, &credential).expect("write");
        let error =
            load_credential_file(&path).expect_err("control chars in server must be rejected");
        assert!(
            error.to_string().contains("control characters"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.hcred");
        let (credential, _signer) = sample_verified();
        write_credential_file(&path, &credential).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("loosen perms");
        let error = load_credential_file(&path).expect_err("group-readable file must be rejected");
        let message = error.to_string();
        assert!(message.contains("group/other-accessible"));
        assert!(message.contains("chmod 600"));
    }
}
