//! Global credential store for Heddle authentication.
//!
//! Manages `~/.heddle/credentials.toml` for persistent server credentials.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use objects::fs_atomic::write_file_atomic_secret;
use serde::{Deserialize, Serialize};

static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many seconds before expiry we proactively rotate.
/// 7 days gives plenty of buffer for intermittent CLI usage — if someone
/// pushes once a week, the token stays fresh indefinitely.
const ROTATION_WINDOW_SECS: u64 = 7 * 24 * 3600; // 7 days

/// Top-level credential store.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CredentialStore {
    #[serde(default)]
    pub defaults: CredentialDefaults,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerCredential>,
}

/// Default settings for credential resolution.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CredentialDefaults {
    pub server: Option<String>,
}

/// Credential for a single Heddle server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCredential {
    pub token: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Path to the global credentials file: `<heddle_home>/credentials.toml`.
///
/// Uses the same home resolution as device identity (`$HEDDLE_HOME` if set,
/// else `$HOME/.heddle`), so credentials and device keys stay co-located.
pub fn credentials_path() -> PathBuf {
    repo::identity::heddle_home_dir().join("credentials.toml")
}

/// Load the credential store from disk. Returns an empty store if the file
/// does not exist.
pub fn load_credentials() -> Result<CredentialStore> {
    let path = credentials_path();
    match fs::read_to_string(&path) {
        Ok(contents) => {
            toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CredentialStore::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the credential store to disk, creating the parent directory if needed.
pub fn save_credentials(store: &CredentialStore) -> Result<()> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        objects::fs_atomic::create_private_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(store).context("serializing credentials")?;
    write_file_atomic_secret(&path, contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;

    Ok(())
}

/// Look up a credential by server hostname.
pub fn get_server_credential(server: &str) -> Result<Option<ServerCredential>> {
    let store = load_credentials()?;
    Ok(store.servers.get(server).cloned())
}

/// Insert or update a credential for the given server. Also sets the default
/// server if none is configured.
pub fn store_server_credential(server: &str, cred: ServerCredential) -> Result<()> {
    let mut store = load_credentials()?;
    store.servers.insert(server.to_string(), cred);
    if store.defaults.server.is_none() {
        store.defaults.server = Some(server.to_string());
    }
    save_credentials(&store)
}

/// Resolve a credential for a server key, trying common key variations.
///
/// The credential store key may include a scheme prefix (e.g. `http://host:port`)
/// while the remote URL parser strips scheme prefixes (producing just `host:port`).
/// This function tries the bare key first, then common scheme-prefixed variants.
pub fn resolve_credential_for_server(server_key: &str) -> Result<Option<ServerCredential>> {
    let store = load_credentials()?;

    // Try exact match first.
    if let Some(cred) = store.servers.get(server_key) {
        return Ok(Some(cred.clone()));
    }

    // Try with scheme prefixes (auth login stores the full --server URL as the key).
    for prefix in &["http://", "https://", "heddle://"] {
        let prefixed = format!("{prefix}{server_key}");
        if let Some(cred) = store.servers.get(&prefixed) {
            return Ok(Some(cred.clone()));
        }
    }

    // Try stripping scheme prefixes (in case the key has a scheme but the store doesn't).
    let stripped = server_key
        .strip_prefix("http://")
        .or_else(|| server_key.strip_prefix("https://"))
        .or_else(|| server_key.strip_prefix("heddle://"));
    if let Some(bare) = stripped
        && let Some(cred) = store.servers.get(bare)
    {
        return Ok(Some(cred.clone()));
    }

    Ok(None)
}

/// Remove the credential for a server.
pub fn remove_server_credential(server: &str) -> Result<()> {
    let mut store = load_credentials()?;
    store.servers.remove(server);
    if store.defaults.server.as_deref() == Some(server) {
        store.defaults.server = None;
    }
    save_credentials(&store)
}

/// Resolve the default server from the credential store.
pub fn default_server() -> Result<Option<String>> {
    let store = load_credentials()?;
    Ok(store.defaults.server)
}

/// Returns `true` if the credential's stored expiry is within the
/// next [`ROTATION_WINDOW_SECS`] seconds.
///
/// Reads `cred.expires_at` (RFC 3339) rather than the token bytes
/// directly: Biscuit tokens are intentionally opaque, but we
/// already cache the expiry alongside the token at issue time, which
/// is the source of truth the CLI needs for rotation decisions.
/// Returns `false` on any parse failure so a stale credential row
/// doesn't block normal CLI operation.
pub fn token_needs_rotation(cred: &ServerCredential) -> bool {
    let Some(expires_str) = cred.expires_at.as_deref() else {
        // No stored expiry — older credential row, or a token type
        // (e.g. service-account credential issued without one) that
        // the server doesn't expire. Skip rotation.
        return false;
    };
    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(expires_str) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    let exp = expires_at.timestamp();
    exp.saturating_sub(now) <= ROTATION_WINDOW_SECS as i64
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        panic::{AssertUnwindSafe, catch_unwind},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static TEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let counter = TEST_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "{prefix}-{unique}-{}-{counter}",
            std::process::id()
        ))
    }

    fn with_home_dir<T>(home: PathBuf, f: impl FnOnce() -> T) -> T {
        let _guard = lock_test_env();
        let original_home = std::env::var_os("HOME");
        let original_heddle_home = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HOME", &home);
            // Prefer HOME-derived path in these tests unless a case sets HEDDLE_HOME.
            std::env::remove_var("HEDDLE_HOME");
        }
        let result = catch_unwind(AssertUnwindSafe(f));
        match original_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match original_heddle_home {
            Some(value) => unsafe {
                std::env::set_var("HEDDLE_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HEDDLE_HOME");
            },
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn save_credentials_round_trips_through_atomic_write() {
        let home = unique_temp_dir("heddle-credentials-test");
        fs::create_dir_all(&home).expect("create temp home");

        with_home_dir(home.clone(), || {
            let mut store = CredentialStore::default();
            store.servers.insert(
                "heddle.example:8421".to_string(),
                ServerCredential {
                    token: "token-123".to_string(),
                    subject: "dev".to_string(),
                    device_id: Some("device-1".to_string()),
                    credential_id: Some("cred-1".to_string()),
                    private_key_pem: Some("pem".to_string()),
                    expires_at: Some("2026-01-01T00:00:00Z".to_string()),
                },
            );
            save_credentials(&store).expect("save credentials");

            let path = credentials_path();
            assert!(path.exists(), "expected credentials file to exist");

            let loaded = load_credentials().expect("load credentials");
            let cred = loaded
                .servers
                .get("heddle.example:8421")
                .expect("stored credential");
            assert_eq!(cred.subject, "dev");
            assert_eq!(cred.token, "token-123");
        });

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_private_key_loads_and_saves_as_private_key_pem() {
        let legacy = r#"
[servers."heddle.example:8421"]
token = "token-123"
subject = "dev"
private_key = "legacy-pem"
"#;

        let store: CredentialStore = toml::from_str(legacy).expect("load legacy credential");
        let credential = store
            .servers
            .get("heddle.example:8421")
            .expect("legacy credential");
        assert_eq!(credential.private_key_pem.as_deref(), Some("legacy-pem"));

        let canonical = toml::to_string_pretty(&store).expect("serialize canonical credential");
        assert!(canonical.contains("private_key_pem = \"legacy-pem\""));
        assert!(!canonical.contains("\nprivate_key ="));
    }

    #[cfg(unix)]
    #[test]
    fn save_credentials_writes_credential_file_0600() {
        use std::os::unix::fs::PermissionsExt;

        let home = unique_temp_dir("heddle-credentials-mode-test");
        fs::create_dir_all(&home).expect("create temp home");

        with_home_dir(home.clone(), || {
            let mut store = CredentialStore::default();
            store.servers.insert(
                "heddle.example:8421".to_string(),
                ServerCredential {
                    token: "token-123".to_string(),
                    subject: "dev".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: Some("pem".to_string()),
                    expires_at: None,
                },
            );
            save_credentials(&store).expect("save credentials");

            let mode = fs::metadata(credentials_path())
                .expect("credentials metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        });

        let _ = fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn save_credentials_permission_failure_returns_error() {
        use std::os::unix::fs::PermissionsExt;

        let home = unique_temp_dir("heddle-credentials-permission-test");
        let heddle_dir = home.join(".heddle");
        fs::create_dir_all(&heddle_dir).expect("create credentials dir");
        fs::set_permissions(&heddle_dir, fs::Permissions::from_mode(0o500))
            .expect("make credentials dir unwritable");

        with_home_dir(home.clone(), || {
            let mut store = CredentialStore::default();
            store.servers.insert(
                "heddle.example:8421".to_string(),
                ServerCredential {
                    token: "token-123".to_string(),
                    subject: "dev".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: Some("pem".to_string()),
                    expires_at: None,
                },
            );

            let err = save_credentials(&store).expect_err("permission failure must propagate");
            assert!(
                err.to_string().contains("writing") || err.to_string().contains("Permission"),
                "unexpected error: {err:?}"
            );
            assert!(
                !credentials_path().exists(),
                "failed write must not publish credentials"
            );
        });

        fs::set_permissions(&heddle_dir, fs::Permissions::from_mode(0o700))
            .expect("restore credentials dir");
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_credential_for_server_accepts_scheme_prefixed_keys() {
        let home = unique_temp_dir("heddle-credentials-test");
        fs::create_dir_all(&home).expect("create temp home");

        with_home_dir(home.clone(), || {
            let mut store = CredentialStore::default();
            store.servers.insert(
                "http://heddle.example:8421".to_string(),
                ServerCredential {
                    token: "token-abc".to_string(),
                    subject: "dev".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            );
            save_credentials(&store).expect("save credentials");

            let resolved = resolve_credential_for_server("heddle.example:8421")
                .expect("resolve credential")
                .expect("scheme-prefixed credential");
            assert_eq!(resolved.token, "token-abc");
            assert_eq!(resolved.subject, "dev");
        });

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn credentials_path_honors_heddle_home() {
        let home = unique_temp_dir("heddle-credentials-heddle-home");
        fs::create_dir_all(&home).expect("create temp home");
        let heddle_home = home.join("custom-heddle");

        let _guard = lock_test_env();
        let original_home = std::env::var_os("HOME");
        let original_heddle_home = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HEDDLE_HOME", &heddle_home);
        }
        let path = credentials_path();
        match original_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match original_heddle_home {
            Some(value) => unsafe {
                std::env::set_var("HEDDLE_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HEDDLE_HOME");
            },
        }

        assert_eq!(path, heddle_home.join("credentials.toml"));
        let _ = fs::remove_dir_all(home);
    }
}
