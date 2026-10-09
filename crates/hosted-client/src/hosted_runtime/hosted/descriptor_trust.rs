//! Per-server deployment descriptor ROOT pin.
//!
//! The stored `{key_id, public_key}` is the stable descriptor root, not an
//! instance endpoint key. Served ephemeral keys are accepted only when a
//! root attestation verifies against this pin; a fetched document cannot
//! replace the pin (root rotation is `heddle auth trust replace`).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
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

/// Root selected by local descriptor trust, retained separately from the
/// authenticated endpoint. Served witness metadata cannot construct this pin.
#[derive(Clone, Debug)]
pub struct HostedRootSelection {
    pub(crate) authority: String,
    pub(crate) root_id: String,
    pub(crate) public_key: [u8; 32],
    pub(crate) automatic_store: Option<PathBuf>,
}
impl HostedRootSelection {
    pub(crate) fn require_current(&self) -> Result<()> {
        if let Some(path) = &self.automatic_store {
            require_current_pin(path, &self.authority, &self.root_id, &self.public_key)?;
        }
        Ok(())
    }
    pub fn authority(&self) -> &str {
        &self.authority
    }
    pub fn root_id(&self) -> &str {
        &self.root_id
    }
    pub fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }
}

pub(crate) fn require_current_pin(
    path: &Path,
    authority: &str,
    root_id: &str,
    key: &[u8; 32],
) -> Result<()> {
    let store = load_store_from(path)?;
    let pin = store
        .servers
        .get(authority)
        .context("descriptor root selection no longer exists")?;
    if pin.key_id != root_id || pin.public_key_bytes()? != *key {
        bail!(
            "descriptor root selection changed; reconnect after explicit repository trust replacement"
        );
    }
    Ok(())
}

type ProofCache =
    BTreeMap<(Vec<u8>, Vec<u8>), api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofResponse>;

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TestWitnessResponses {
    pub set: Option<api::heddle::api::common::SignedHostedWitnessSetV1>,
    pub proofs: Vec<(
        api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofRequest,
        api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofResponse,
    )>,
}
/// Public witness metadata from the selected HTTPS authority. No credential,
/// original statement, Thread identity or account identity is sent to lookup.
pub struct HostedWitnessLookup {
    server: String,
    http: super::bootstrap::BootstrapHttp,
    proofs: Mutex<ProofCache>,
    #[cfg(test)]
    pub(crate) test_responses: Option<TestWitnessResponses>,
}

impl HostedWitnessLookup {
    pub fn new(server: &str, http: super::bootstrap::BootstrapHttp) -> super::Result<Self> {
        let server = canonical_server_authority(server)
            .map_err(|error| super::HostedError::DescriptorTrust(error.to_string()))?;
        Ok(Self {
            server,
            http,
            proofs: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            test_responses: None,
        })
    }

    /// This is an untrusted carrier. The receiver trust store verifies the
    /// independently pinned root and serializes high-water advancement.
    pub async fn fetch_set(
        &self,
    ) -> super::Result<api::heddle::api::common::SignedHostedWitnessSetV1> {
        #[cfg(test)]
        if let Some(responses) = &self.test_responses {
            return responses.set.clone().ok_or_else(|| {
                super::HostedError::BootstrapHttp("witness set unavailable".into())
            });
        }
        api::import_authority::canonical_https(&self.server, true)?;
        let url = format!("{}/.well-known/heddle/hosted-witnesses", self.server);
        let (client, url, host) = self.http.client(&url).await?;
        let mut request = client.get(url);
        if let Some(host) = host {
            request = request.header(reqwest::header::HOST, host);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(super::HostedError::BootstrapHttp(
                "witness set unavailable".into(),
            ));
        }
        let body = super::bootstrap::bounded_response_body(
            response,
            api::witness_trust::MAX_SET_BYTES,
            "witness set",
        )
        .await?;
        Ok(api::hybrid_codec::strict_decode(
            &body,
            api::witness_trust::MAX_SET_BYTES,
        )?)
    }
}

pub(crate) async fn refresh_import_proofs(
    lookup: &HostedWitnessLookup,
    bundle: &mut api::heddle::api::v1alpha2::ImportPublicProofBundleV1,
    selected: api::witness_trust::SetExpectation<'_>,
    previous: Option<&api::witness_trust::VerifiedWitnessSet>,
) -> super::Result<api::witness_trust::VerifiedWitnessSet> {
    let signed = lookup.fetch_set().await?;
    // A lookup may return a set issued after the request began. Verify at the
    // receiver's current time; commit still rechecks the serialized trust.
    let now = chrono::Utc::now().timestamp_millis();
    let selected = api::witness_trust::SetExpectation {
        now_unix_millis: now,
        ..selected
    };
    let verified = api::witness_trust::verify_set(&signed, &selected, previous)?;
    let mut prepared = bundle.clone();
    thread_api::hybrid::history::complete_bundle(lookup, &verified, &mut prepared, now)
        .await
        .map_err(|error| match error {
            thread_api::hybrid::history::Error::Rejected(error) => {
                super::HostedError::Hybrid(error)
            }
            thread_api::hybrid::history::Error::Lookup(error) => error,
            thread_api::hybrid::history::Error::NotFound => {
                super::HostedError::Hybrid(api::hybrid_codec::Reject::Proof)
            }
        })?;
    prepared.witness_set = Some(signed);
    thread_api::hybrid::history::replace_receiver_metadata(bundle, prepared)?;
    Ok(verified)
}

pub(crate) async fn refresh_native_proofs(
    lookup: &HostedWitnessLookup,
    bundle: &mut api::heddle::api::v1alpha2::NativePublicProofBundleV1,
    selected: api::witness_trust::SetExpectation<'_>,
    previous: Option<&api::witness_trust::VerifiedWitnessSet>,
) -> super::Result<api::witness_trust::VerifiedWitnessSet> {
    let signed = lookup.fetch_set().await?;
    // A lookup may return a set issued after the request began. Verify at the
    // receiver's current time; commit still rechecks the serialized trust.
    let now = chrono::Utc::now().timestamp_millis();
    let selected = api::witness_trust::SetExpectation {
        now_unix_millis: now,
        ..selected
    };
    let verified = api::witness_trust::verify_set(&signed, &selected, previous)?;
    let mut prepared = bundle.clone();
    thread_api::hybrid::history::complete_native_bundle(lookup, &verified, &mut prepared, now)
        .await
        .map_err(|error| match error {
            thread_api::hybrid::history::Error::Rejected(error) => {
                super::HostedError::Hybrid(error)
            }
            thread_api::hybrid::history::Error::Lookup(error) => error,
            thread_api::hybrid::history::Error::NotFound => {
                super::HostedError::Hybrid(api::hybrid_codec::Reject::Proof)
            }
        })?;
    prepared.witness_set = Some(signed);
    thread_api::hybrid::history::replace_native_receiver_metadata(bundle, prepared)?;
    Ok(verified)
}

impl thread_api::hybrid::history::HistoryProofLookup for HostedWitnessLookup {
    type Error = super::HostedError;

    async fn lookup(
        &self,
        request: &api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofRequest,
    ) -> super::Result<Option<api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofResponse>>
    {
        use api::hybrid_codec::Reject;
        use prost::Message;
        api::witness_trust::validate_lookup(request)?;
        #[cfg(test)]
        if let Some(responses) = &self.test_responses {
            return Ok(responses
                .proofs
                .iter()
                .find(|(r, _)| r == request)
                .map(|(_, response)| response.clone()));
        }
        api::import_authority::canonical_https(&self.server, true)?;
        let selector = (
            request.executor_id.clone(),
            request.statement_leaf_digest.clone(),
        );
        // Cache public bytes only. Every use still resolves the exact original
        // against the newest receiver-owned root/set under mutation serialization.
        if let Some(proof) = self
            .proofs
            .lock()
            .map_err(|_| Reject::StaleContext)?
            .get(&selector)
            .cloned()
        {
            return Ok(Some(proof));
        }
        let url = format!(
            "{}/.well-known/heddle/hosted-witness-history-proof",
            self.server
        );
        let (client, url, host) = self.http.client(&url).await?;
        let mut outgoing = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .header("Heddle-Protocol-Version", "2")
            .header(
                "Heddle-Mandatory-Features",
                "import-authority-host-witness-v1",
            )
            .body(request.encode_to_vec());
        if let Some(host) = host {
            outgoing = outgoing.header(reqwest::header::HOST, host);
        }
        let response = outgoing.send().await?;
        require_hybrid_headers(response.headers())?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            reqwest::StatusCode::TOO_MANY_REQUESTS => Err(super::HostedError::Call {
                code: api::heddle::api::common::CallFailureCode::ResourceExhausted,
                message: "history proof lookup throttled".into(),
                error: None,
            }),
            status if status.is_success() => {
                let body = super::bootstrap::bounded_response_body(
                    response,
                    api::witness_trust::MAX_PROOF_BYTES,
                    "history proof",
                )
                .await?;
                let decoded: api::heddle::api::v1alpha2::GetHostedWitnessHistoryProofResponse =
                    api::hybrid_codec::strict_decode(&body, api::witness_trust::MAX_PROOF_BYTES)?;
                let proof = decoded.proof.as_ref().ok_or(Reject::Proof)?;
                if proof.executor_id != request.executor_id
                    || proof.siblings.len() > api::witness_trust::MAX_SIBLINGS
                {
                    return Err(Reject::Proof.into());
                }
                let mut cache = self.proofs.lock().map_err(|_| Reject::StaleContext)?;
                if cache.len() >= 1024 {
                    cache.pop_first();
                }
                cache.insert(selector, decoded.clone());
                Ok(Some(decoded))
            }
            _ => Err(super::HostedError::BootstrapHttp(
                "history proof lookup unavailable".into(),
            )),
        }
    }
}

fn require_hybrid_headers(headers: &reqwest::header::HeaderMap) -> super::Result<()> {
    let exact = |name: &str, expected: &str| {
        let mut values = headers.get_all(name).iter();
        values
            .next()
            .is_some_and(|value| value.as_bytes() == expected.as_bytes())
            && values.next().is_none()
    };
    if !exact("Heddle-Protocol-Version", "2")
        || !exact(
            "Heddle-Mandatory-Features",
            "import-authority-host-witness-v1",
        )
    {
        return Err(api::hybrid_codec::Reject::Protocol.into());
    }
    Ok(())
}

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
    let candidate = if server.starts_with("https://") {
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
    repository: Option<&Path>,
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
    let repository_only = repository.is_some()
        && current.key_id == new_key_id
        && current.public_key_bytes()? == new_key;
    if current.public_key_bytes()? != expected && !repository_only {
        bail!(
            "descriptor trust replacement refused for {canonical_server}: \
             --expect-current-public-key does not match the current descriptor public key"
        );
    }
    // Select only a repository explicitly named by the trust ceremony. Other
    // enrolled repositories fail closed against the changed automatic pin until
    // this command is repeated with their path and the same expected old key.
    let selected_repository = repository.map(repo::Repository::open).transpose()?;
    let expected_root = if let Some(repository) = &selected_repository {
        use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
        let snapshot = HostedTrust::open(repository.heddle_dir(), canonical_server, SystemClock)?
            .snapshot()?;
        if snapshot.root.public_key != expected {
            bail!("repository descriptor root does not match --expect-current-public-key");
        }
        Some(snapshot.root)
    } else {
        None
    };
    let replacement = DescriptorTrustRecord {
        key_id: new_key_id.to_string(),
        public_key: hex::encode(new_key),
        first_verified_unix_millis: now_unix_millis()?,
    };
    store
        .servers
        .insert(canonical_server.to_string(), replacement.clone());
    save_store_to(&path, &store)?;
    if let (Some(repository), Some(expected_root)) = (selected_repository, expected_root) {
        repo::thread_replication::hosted_trust::replace_root(
            repository.heddle_dir(),
            &expected_root,
            &repo::thread_replication::hosted_trust::RootSelection {
                authority: canonical_server.into(),
                root_id: new_key_id.into(),
                public_key: new_key,
            },
        )?;
    }
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
        "descriptor root changed for {canonical_server}: pinned root key id `{}` \
         with descriptor root public key fingerprint {}; observed descriptor key id `{observed_key_id}`. \
         Automatic root rotation was refused; verify the new descriptor root public key out of band, then run \
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

    #[tokio::test]
    async fn witness_refresh_samples_time_after_lookup_for_both_carriers() {
        use api::hybrid_codec::Reject;
        use prost::Message as _;

        use crate::hosted_runtime::device_rpc::hybrid_security_tests::{Fixture, sign_set};

        let _guard = crate::test_process_env::exclusive().await;
        for native in [false, true] {
            let mut fixture = if native {
                Fixture::native()
            } else {
                Fixture::new()
            };
            let mut fresh = if let Some(bundle) = &fixture.native_bundle {
                bundle.witness_set.clone().expect("native set")
            } else {
                fixture.bundle.witness_set.clone().expect("import set")
            };
            let now = chrono::Utc::now().timestamp_millis();
            let body = fresh.body.as_mut().expect("set body");
            body.issued_at_unix_millis = now - 1000;
            body.valid_until_unix_millis = now + 240_000;
            sign_set(&mut fresh, 7);
            let body = fresh.body.as_ref().expect("set body");
            let authority = body.deployment_authority.clone();
            let root_id = body.descriptor_root_id.clone();
            let selected = api::witness_trust::SetExpectation {
                authority: &authority,
                root_id: &root_id,
                root_public_key: &fixture.root.public_key,
                root_epoch: 1,
                now_unix_millis: now - 10_000,
                clock_floor_unix_millis: 0,
                known_job_keys: &[],
            };
            assert!(
                matches!(
                    api::witness_trust::verify_set(&fresh, &selected, None),
                    Err(Reject::Expired)
                ),
                "the response was issued after the request's sampled time"
            );
            let mut lookup = HostedWitnessLookup::new(
                &authority,
                crate::hosted_runtime::hosted::BootstrapHttp::new(&config::ClientConfig::default()),
            )
            .expect("selected HTTPS origin");
            lookup.test_responses = Some(TestWitnessResponses {
                set: Some(fresh.clone()),
                proofs: vec![],
            });
            let verified = if let Some(bundle) = &mut fixture.native_bundle {
                refresh_native_proofs(&lookup, bundle, selected, None).await
            } else {
                refresh_import_proofs(&lookup, &mut fixture.bundle, selected, None).await
            }
            .expect("fresh response verifies at receiver time after lookup");
            assert_eq!(verified.body(), fresh.body.as_ref().expect("fresh body"));

            let mut expired = fresh.clone();
            let body = expired.body.as_mut().expect("set body");
            body.issued_at_unix_millis = now - 241_000;
            body.valid_until_unix_millis = now - 1000;
            sign_set(&mut expired, 7);
            api::witness_trust::verify_set(&expired, &selected, None)
                .expect("the earlier request time would accept an already expired response");
            lookup.test_responses = Some(TestWitnessResponses {
                set: Some(expired),
                proofs: vec![],
            });
            let before = if let Some(bundle) = &fixture.native_bundle {
                bundle.encode_to_vec()
            } else {
                fixture.bundle.encode_to_vec()
            };
            let result = if let Some(bundle) = &mut fixture.native_bundle {
                refresh_native_proofs(&lookup, bundle, selected, None).await
            } else {
                refresh_import_proofs(&lookup, &mut fixture.bundle, selected, None).await
            };
            assert!(matches!(
                result,
                Err(super::super::HostedError::Hybrid(Reject::Expired))
            ));
            let after = if let Some(bundle) = &fixture.native_bundle {
                bundle.encode_to_vec()
            } else {
                fixture.bundle.encode_to_vec()
            };
            assert_eq!(
                after, before,
                "expired metadata never changes retained originals"
            );
        }
    }

    #[test]
    fn proof_lookup_gate_requires_one_exact_version_and_feature_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Heddle-Protocol-Version", "2".parse().expect("version"));
        headers.insert(
            "Heddle-Mandatory-Features",
            "import-authority-host-witness-v1".parse().expect("feature"),
        );
        require_hybrid_headers(&headers).expect("supported protocol");
        headers.append("Heddle-Protocol-Version", "2".parse().expect("duplicate"));
        assert!(require_hybrid_headers(&headers).is_err());
        headers.insert("Heddle-Protocol-Version", "1".parse().expect("old peer"));
        assert!(require_hybrid_headers(&headers).is_err());
        headers.insert("Heddle-Protocol-Version", "2".parse().expect("version"));
        headers.insert(
            "Heddle-Mandatory-Features",
            "import-authority-host-witness-v1, unknown"
                .parse()
                .expect("feature"),
        );
        assert!(require_hybrid_headers(&headers).is_err());
        headers.insert(
            "Heddle-Mandatory-Features",
            "import-authority-host-witness-v1".parse().expect("feature"),
        );
        require_hybrid_headers(&headers).expect("exact control");
    }

    fn with_isolated_home<T>(test: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = config::credentials::lock_test_env();
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        for alias in [
            "API.Example",
            "https://api.example",
            "https://api.example:443",
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
                    &hex::encode(new),
                    None
                )
                .is_err()
            );
            assert_eq!(fs::read(descriptor_trust_path()).unwrap(), before);
            assert!(
                replace_descriptor_trust(server, &hex::encode(old), "", &hex::encode(new), None)
                    .is_err()
            );
            assert_eq!(fs::read(descriptor_trust_path()).unwrap(), before);

            let replacement = replace_descriptor_trust(
                server,
                &hex::encode(old),
                "new-id",
                &hex::encode(new),
                None,
            )
            .unwrap();
            assert_eq!(replacement.key_id, "new-id");
            assert_eq!(replacement.public_key, hex::encode(new));

            fs::write(descriptor_trust_path(), "not valid toml").unwrap();
            assert!(load_automatic_pin(server).is_err());
        });
    }

    #[test]
    fn same_pair_concurrent_first_contact_converges() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_home(|_| {
            insert_verified_pin("https://api.example", "automatic-id", &[0x77; 32]).unwrap();
            let automatic = trust_report("https://API.example:443", None).unwrap();
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
