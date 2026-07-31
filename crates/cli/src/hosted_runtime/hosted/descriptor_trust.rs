//! Per-server descriptor-signing trust continuity.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use objects::{
    fs_atomic::{create_private_dir_all, write_file_atomic_secret},
    lock::RepoLock,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STORE_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorTrustRecord {
    pub key_id: String,
    pub public_key: String,
    pub first_verified_unix_millis: i64,
}

impl DescriptorTrustRecord {
    pub fn public_key_bytes(&self) -> Result<[u8; 32]> {
        parse_descriptor_public_key(&self.public_key)
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(descriptor_public_key_fingerprint(&self.public_key_bytes()?))
    }

    fn validate(&self) -> Result<()> {
        validate_key_id(&self.key_id)?;
        self.public_key_bytes()?;
        if self.first_verified_unix_millis < 0 {
            bail!("descriptor trust record has an invalid first verification time");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorTrustStore {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, DescriptorTrustRecord>,
}

impl Default for DescriptorTrustStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DescriptorTrustSource {
    Explicit,
    Automatic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DescriptorTrustReport {
    pub canonical_server: String,
    pub source: DescriptorTrustSource,
    pub key_id: String,
    pub public_key: String,
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinInsertOutcome {
    Created,
    AlreadyPresent,
}

pub fn descriptor_trust_path() -> PathBuf {
    repo::identity::heddle_home_dir().join("descriptor-trust.toml")
}

pub fn canonical_server_authority(server: &str) -> Result<String> {
    let candidate = if let Some(authority) = server.strip_prefix("heddle://") {
        format!("https://{authority}")
    } else if server.starts_with("https://") {
        server.to_string()
    } else if server.contains("://") {
        bail!("native hosted bootstrap requires an HTTPS server authority");
    } else {
        format!("https://{server}")
    };
    let mut url = reqwest::Url::parse(&candidate)
        .context("native hosted bootstrap requires a valid HTTPS server authority")?;
    if url.scheme() != "https" {
        bail!("native hosted bootstrap requires an HTTPS server authority");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "native hosted bootstrap requires an HTTPS server authority without userinfo, path, query, or fragment"
        );
    }
    url.set_path("");
    if url.port() == Some(443) {
        url.set_port(None)
            .map_err(|_| anyhow::anyhow!("invalid HTTPS server port"))?;
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub fn validate_descriptor_pair(key_id: &str, public_key: &str) -> Result<[u8; 32]> {
    validate_key_id(key_id)?;
    parse_descriptor_public_key(public_key)
}

pub fn parse_descriptor_public_key(public_key: &str) -> Result<[u8; 32]> {
    if public_key.len() != 64
        || !public_key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("descriptor public key must be exactly 64 lowercase hexadecimal characters");
    }
    let decoded = hex::decode(public_key).context("decoding descriptor public key")?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("descriptor public key must decode to exactly 32 bytes"))
}

pub fn descriptor_public_key_fingerprint(public_key: &[u8; 32]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(public_key)))
}

pub fn load_automatic_pin(canonical_server: &str) -> Result<Option<DescriptorTrustRecord>> {
    let store = load_store()?;
    Ok(store.servers.get(canonical_server).cloned())
}

pub fn insert_verified_pin(
    canonical_server: &str,
    key_id: &str,
    public_key: &[u8; 32],
) -> Result<PinInsertOutcome> {
    let path = descriptor_trust_path();
    prepare_store_directory(&path)?;
    let _guard = RepoLock::at(lock_path(&path))
        .write()
        .context("locking descriptor trust store")?;
    let mut store = load_store_from(&path)?;
    let candidate_key = hex::encode(public_key);
    if let Some(current) = store.servers.get(canonical_server) {
        if current.key_id == key_id && current.public_key == candidate_key {
            return Ok(PinInsertOutcome::AlreadyPresent);
        }
        bail!("{}", pin_change_message(canonical_server, current, key_id)?);
    }
    let record = DescriptorTrustRecord {
        key_id: key_id.to_string(),
        public_key: candidate_key,
        first_verified_unix_millis: now_unix_millis()?,
    };
    record.validate()?;
    store.servers.insert(canonical_server.to_string(), record);
    save_store_to(&path, &store)?;
    Ok(PinInsertOutcome::Created)
}

pub fn replace_descriptor_trust(
    canonical_server: &str,
    expected_current_public_key: &str,
    new_key_id: &str,
    new_public_key: &str,
) -> Result<DescriptorTrustRecord> {
    let expected = parse_descriptor_public_key(expected_current_public_key)?;
    let new_key = validate_descriptor_pair(new_key_id, new_public_key)?;
    let path = descriptor_trust_path();
    prepare_store_directory(&path)?;
    let _guard = RepoLock::at(lock_path(&path))
        .write()
        .context("locking descriptor trust store")?;
    let mut store = load_store_from(&path)?;
    let current = store.servers.get(canonical_server).ok_or_else(|| {
        anyhow::anyhow!("no automatic descriptor trust pin exists for {canonical_server}")
    })?;
    if current.public_key_bytes()? != expected {
        bail!(
            "descriptor trust replacement refused for {canonical_server}: \
             --expect-current-public-key does not match the current descriptor public key"
        );
    }
    let replacement = DescriptorTrustRecord {
        key_id: new_key_id.to_string(),
        public_key: hex::encode(new_key),
        first_verified_unix_millis: now_unix_millis()?,
    };
    store
        .servers
        .insert(canonical_server.to_string(), replacement.clone());
    save_store_to(&path, &store)?;
    Ok(replacement)
}

pub fn trust_report(
    server: &str,
    explicit: Option<(&str, &[u8; 32])>,
) -> Result<DescriptorTrustReport> {
    let canonical_server = canonical_server_authority(server)?;
    let (source, key_id, public_key) = match explicit {
        Some((key_id, public_key)) => (
            DescriptorTrustSource::Explicit,
            key_id.to_string(),
            hex::encode(public_key),
        ),
        None => {
            let record = load_automatic_pin(&canonical_server)?.ok_or_else(|| {
                anyhow::anyhow!("no descriptor trust is configured for {canonical_server}")
            })?;
            (
                DescriptorTrustSource::Automatic,
                record.key_id,
                record.public_key,
            )
        }
    };
    let public_key_bytes = parse_descriptor_public_key(&public_key)?;
    Ok(DescriptorTrustReport {
        canonical_server,
        source,
        key_id,
        public_key,
        fingerprint: descriptor_public_key_fingerprint(&public_key_bytes),
    })
}

pub fn pin_change_message(
    canonical_server: &str,
    current: &DescriptorTrustRecord,
    observed_key_id: &str,
) -> Result<String> {
    Ok(format!(
        "descriptor trust changed for {canonical_server}: pinned key id `{}` \
         with descriptor public key fingerprint {}; observed descriptor key id `{observed_key_id}`. \
         Automatic re-pinning was refused; verify the new descriptor public key out of band, then run \
         `heddle auth trust replace --server {canonical_server} \
         --expect-current-public-key {} --key-id <new-id> --public-key <64-hex>`",
        current.key_id,
        current.fingerprint()?,
        current.public_key,
    ))
}

fn validate_key_id(key_id: &str) -> Result<()> {
    if key_id.trim().is_empty() {
        bail!("descriptor key id must not be empty");
    }
    Ok(())
}

fn load_store() -> Result<DescriptorTrustStore> {
    load_store_from(&descriptor_trust_path())
}

fn load_store_from(path: &Path) -> Result<DescriptorTrustStore> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DescriptorTrustStore::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let store: DescriptorTrustStore =
        toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    if store.version != STORE_VERSION {
        bail!(
            "unsupported descriptor trust store version {} in {}",
            store.version,
            path.display()
        );
    }
    for (server, record) in &store.servers {
        if canonical_server_authority(server)? != *server {
            bail!("descriptor trust store contains non-canonical server authority `{server}`");
        }
        record
            .validate()
            .with_context(|| format!("validating descriptor trust for {server}"))?;
    }
    Ok(store)
}

fn save_store_to(path: &Path, store: &DescriptorTrustStore) -> Result<()> {
    let contents = toml::to_string_pretty(store).context("serializing descriptor trust store")?;
    write_file_atomic_secret(path, contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn prepare_store_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("descriptor trust store path has no parent"))?;
    create_private_dir_all(parent)
        .with_context(|| format!("creating descriptor trust directory {}", parent.display()))
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("toml.lock")
}

fn now_unix_millis() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    i64::try_from(millis).context("system clock exceeds supported Unix time")
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    fn with_isolated_home<T>(test: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = cli_shared::credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temporary Heddle home");
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
        }
        let result = catch_unwind(AssertUnwindSafe(|| test(home.path())));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn canonical_aliases_share_default_port_and_non_default_ports_do_not() {
        for alias in [
            "API.Example",
            "https://api.example",
            "heddle://api.example:443",
        ] {
            assert_eq!(
                canonical_server_authority(alias).unwrap(),
                "https://api.example"
            );
        }
        assert_eq!(
            canonical_server_authority("api.example:8421").unwrap(),
            "https://api.example:8421"
        );
        assert_eq!(
            canonical_server_authority("[2001:db8::1]:443").unwrap(),
            "https://[2001:db8::1]"
        );
    }

    #[test]
    fn canonical_authority_rejects_ambiguous_url_components() {
        for invalid in [
            "http://api.example",
            "https://user@api.example",
            "https://api.example/path",
            "https://api.example?query",
            "https://api.example#fragment",
        ] {
            assert!(canonical_server_authority(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn store_is_fail_closed_and_replacement_is_compare_and_swap() {
        with_isolated_home(|_| {
            let server = "https://api.example";
            let old = [0x11; 32];
            let new = [0x22; 32];
            insert_verified_pin(server, "old-id", &old).unwrap();
            let before = fs::read(descriptor_trust_path()).unwrap();

            assert!(
                replace_descriptor_trust(
                    server,
                    &hex::encode([0x33; 32]),
                    "new-id",
                    &hex::encode(new)
                )
                .is_err()
            );
            assert_eq!(fs::read(descriptor_trust_path()).unwrap(), before);
            assert!(
                replace_descriptor_trust(server, &hex::encode(old), "", &hex::encode(new)).is_err()
            );
            assert_eq!(fs::read(descriptor_trust_path()).unwrap(), before);

            let replacement =
                replace_descriptor_trust(server, &hex::encode(old), "new-id", &hex::encode(new))
                    .unwrap();
            assert_eq!(replacement.key_id, "new-id");
            assert_eq!(replacement.public_key, hex::encode(new));

            fs::write(descriptor_trust_path(), "not valid toml").unwrap();
            assert!(load_automatic_pin(server).is_err());
        });
    }

    #[test]
    fn same_pair_concurrent_first_contact_converges() {
        with_isolated_home(|_| {
            let barrier = Arc::new(Barrier::new(2));
            let handles = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        insert_verified_pin("https://api.example", "same-id", &[0x44; 32])
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = handles
                .into_iter()
                .map(|handle| handle.join().unwrap().unwrap())
                .collect::<Vec<_>>();
            assert!(outcomes.contains(&PinInsertOutcome::Created));
            assert!(outcomes.contains(&PinInsertOutcome::AlreadyPresent));
            assert_eq!(
                load_automatic_pin("https://api.example")
                    .unwrap()
                    .unwrap()
                    .public_key,
                hex::encode([0x44; 32])
            );
        });
    }

    #[test]
    fn different_pair_concurrent_first_contact_preserves_the_winner() {
        with_isolated_home(|_| {
            let barrier = Arc::new(Barrier::new(2));
            let handles = [(0x55, "key-a"), (0x66, "key-b")]
                .into_iter()
                .map(|(byte, key_id)| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        (
                            byte,
                            insert_verified_pin("https://api.example", key_id, &[byte; 32]),
                        )
                    })
                })
                .collect::<Vec<_>>();
            let outcomes = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
                1
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|(_, result)| result.is_err())
                    .count(),
                1
            );
            let winner = outcomes
                .iter()
                .find_map(|(byte, result)| result.is_ok().then_some(*byte))
                .unwrap();
            assert_eq!(
                load_automatic_pin("https://api.example")
                    .unwrap()
                    .unwrap()
                    .public_key,
                hex::encode([winner; 32])
            );
        });
    }

    #[test]
    fn report_distinguishes_explicit_and_automatic_trust() {
        with_isolated_home(|_| {
            insert_verified_pin("https://api.example", "automatic-id", &[0x77; 32]).unwrap();
            let automatic = trust_report("heddle://API.example:443", None).unwrap();
            assert_eq!(automatic.source, DescriptorTrustSource::Automatic);
            assert_eq!(automatic.key_id, "automatic-id");

            let explicit_key = [0x88; 32];
            let explicit =
                trust_report("api.example", Some(("explicit-id", &explicit_key))).unwrap();
            assert_eq!(explicit.source, DescriptorTrustSource::Explicit);
            assert_eq!(explicit.public_key, hex::encode(explicit_key));
        });
    }
}
