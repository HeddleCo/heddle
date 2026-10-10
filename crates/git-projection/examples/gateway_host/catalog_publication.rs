// SPDX-License-Identifier: Apache-2.0
//! Metadata-only Artifacts Git v1 adapter. The native SourcePack never enters this repository.
//! There is no Artifacts binding write/CAS API: publication uses an exact expected-old
//! receive-pack command. Credentials and remote come only from trusted host configuration.
use crate::{
    Result,
    policy::{canonical, digest, hex},
    transport,
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::Command,
};

const MAX_METADATA: usize = 64 * 1024;
const MAX_CONTROL: usize = 1024 * 1024;
const CATALOG_REF: &str = "refs/heads/main";
const ABSENT: &str = "0000000000000000000000000000000000000000";

/// A deterministic, three-object catalog update: one manifest blob, one tree, one commit.
/// The commit parent is the exact previously published catalog pin, not a source Git commit.
pub struct CatalogCommit {
    pub pin: String,
    pub expected: String,
    pub manifest_sha256: String,
    pub metadata_bytes: usize,
    pack: Vec<u8>,
}
fn git(path: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(path).args(args);
    let mut output = tempfile::tempfile()?;
    if !transport::run_bounded(&mut command, input, output.try_clone()?)?.success() {
        return Err("catalog Git operation failed".into());
    }
    output.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    output
        .take((MAX_CONTROL + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONTROL {
        return Err("catalog output limit".into());
    }
    Ok(bytes)
}
fn oid(bytes: Vec<u8>) -> Result<String> {
    let text = std::str::from_utf8(&bytes)?.trim_end_matches('\n');
    if !hex(text, 40) {
        return Err("invalid catalog Git identity".into());
    }
    Ok(text.into())
}
fn metadata(value: &serde_json::Value, depth: usize) -> bool {
    if depth > 12 {
        return false;
    }
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => true,
        serde_json::Value::Number(n) => n.as_u64().is_some_and(|n| n <= 9_007_199_254_740_991),
        serde_json::Value::String(s) => s.len() <= 512 && !s.contains(['\0', '\r', '\n']),
        serde_json::Value::Array(a) => a.len() <= 128 && a.iter().all(|v| metadata(v, depth + 1)),
        serde_json::Value::Object(o) => {
            o.len() <= 32
                && o.iter().all(|(k, v)| {
                    k.len() <= 64
                        && !matches!(
                            k.as_str(),
                            "bytes" | "content" | "data" | "files" | "objects" | "pack"
                        )
                        && metadata(v, depth + 1)
                })
        }
    }
}
impl CatalogCommit {
    pub fn prepare(expected: &str, operation: &str, manifest: &[u8]) -> Result<Self> {
        if !hex(expected, 40) || !hex(operation, 64) || manifest.len() > MAX_METADATA {
            return Err("invalid bounded catalog publication".into());
        }
        let value: serde_json::Value = serde_json::from_slice(manifest)?;
        let root = value
            .as_object()
            .ok_or("catalog metadata object required")?;
        if canonical(&value)? != manifest
            || root.len() != 4
            || value["schema"] != 2
            || value["operation"] != operation
            || !value["intent"].is_object()
            || !value["native_receipt"].is_object()
            || !metadata(&value, 0)
        {
            return Err("canonical accepted-source metadata required".into());
        }
        let scratch = tempfile::tempdir()?;
        let path = scratch.path().join("metadata.git");
        sley::Repository::init_bare(&path)?;
        let blob = oid(git(&path, &["hash-object", "-w", "--stdin"], manifest)?)?;
        let tree = oid(git(
            &path,
            &["mktree"],
            format!("100644 blob {blob}\tmanifest.json\n").as_bytes(),
        )?)?;
        // Fixed metadata identity/time makes retries byte-identical, independent of short-lived
        // sessions, wall clocks and process restarts. The operation is already a domain hash.
        let parent = if expected == ABSENT {
            String::new()
        } else {
            format!("parent {expected}\n")
        };
        let raw = format!(
            "tree {tree}\n{parent}author Heddle Catalog <catalog@heddle.invalid> 1700000000 +0000\ncommitter Heddle Catalog <catalog@heddle.invalid> 1700000000 +0000\n\nheddle publication {operation}\n"
        );
        let pin = oid(git(
            &path,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            raw.as_bytes(),
        )?)?;
        let pack = git(
            &path,
            &["pack-objects", "--stdout"],
            format!("{pin}\n{tree}\n{blob}\n").as_bytes(),
        )?;
        if !pack.starts_with(b"PACK") || pack.len() > MAX_CONTROL {
            return Err("catalog pack limit".into());
        }
        Ok(Self {
            pin,
            expected: expected.into(),
            manifest_sha256: digest(manifest),
            metadata_bytes: manifest.len(),
            pack,
        })
    }
    /// Local controlled adapter: real Git objects and real atomic update-ref expected-old CAS.
    /// The target MUST be a dedicated metadata catalog, never the source repository.
    pub fn publish_local_fixture(&self, catalog: &Path) -> Result<()> {
        git(
            catalog,
            &["index-pack", "--stdin", "--fix-thin"],
            &self.pack,
        )?;
        let bytes = git(
            catalog,
            &["for-each-ref", "--format=%(objectname)", CATALOG_REF],
            &[],
        )?;
        let current = if bytes.is_empty() {
            ABSENT.to_string()
        } else {
            oid(bytes)?
        };
        if current == self.pin {
            return Ok(());
        }
        if current != self.expected {
            return Err("catalog compare-and-swap conflict".into());
        }
        git(
            catalog,
            &["update-ref", CATALOG_REF, &self.pin, &self.expected],
            &[],
        )?;
        if oid(git(catalog, &["rev-parse", "--verify", CATALOG_REF], &[])?)? != self.pin {
            return Err("catalog publication changed before verification".into());
        }
        File::open(catalog)?.sync_all()?;
        Ok(())
    }
}
fn packet(bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
    if bytes.len() + 4 > 65520 {
        return Err("catalog packet limit".into());
    }
    out.extend(format!("{:04x}", bytes.len() + 4).as_bytes());
    out.extend(bytes);
    Ok(())
}
fn packets(mut bytes: &[u8]) -> Result<Vec<&[u8]>> {
    let mut output = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err("truncated catalog response".into());
        }
        let size = usize::from_str_radix(std::str::from_utf8(&bytes[..4])?, 16)?;
        if size == 0 {
            bytes = &bytes[4..];
            continue;
        }
        if size < 4 || size > bytes.len() || output.len() >= 1024 {
            return Err("invalid catalog response".into());
        }
        output.push(&bytes[4..size]);
        bytes = &bytes[size..];
    }
    Ok(output)
}
fn bounded_response(response: reqwest::blocking::Response, content_type: &str) -> Result<Vec<u8>> {
    if response.status() != reqwest::StatusCode::OK
        || response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some(content_type)
        || response
            .headers()
            .contains_key(reqwest::header::CONTENT_ENCODING)
        || response
            .content_length()
            .is_some_and(|n| n > MAX_CONTROL as u64)
    {
        return Err("Artifacts Git response unavailable".into());
    }
    let mut bytes = Vec::new();
    response
        .take((MAX_CONTROL + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONTROL {
        return Err("Artifacts Git response limit".into());
    }
    Ok(bytes)
}
/// Caller-supplied existing repository credential. This adapter never creates or retains one.
pub struct ArtifactsGit<'a> {
    client: reqwest::blocking::Client,
    remote: reqwest::Url,
    credential: &'a str,
}
impl<'a> ArtifactsGit<'a> {
    pub fn new(remote: &str, credential: &'a str) -> Result<Self> {
        Self::configured(remote, credential, None)
    }
    /// Explicit synthetic integration seam; compiled out of production builds. Only HTTPS
    /// loopback with the supplied test CA is trusted, with normal certificate/hostname checks.
    #[cfg(feature = "gateway-fixture")]
    pub fn new_fixture(remote: &str, credential: &'a str, ca_pem: &[u8]) -> Result<Self> {
        if ca_pem.is_empty() || ca_pem.len() > 64 * 1024 {
            return Err("bounded fixture CA required".into());
        }
        Self::configured(remote, credential, Some(ca_pem))
    }
    fn configured(remote: &str, credential: &'a str, fixture_ca: Option<&[u8]>) -> Result<Self> {
        let url = reqwest::Url::parse(remote)?;
        let host = url.host_str().ok_or("missing Artifacts host")?;
        let account = host.strip_suffix(".artifacts.cloudflare.net");
        let parts: Vec<_> = url.path().split('/').collect();
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if url.scheme() != "https"
            || if fixture_ca.is_some() {
                host != "127.0.0.1"
            } else {
                account.is_none_or(|a| !safe(a)) || url.port().is_some()
            }
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || parts.len() != 4
            || parts[1] != "git"
            || !safe(parts[2])
            || !parts[3].strip_suffix(".git").is_some_and(safe)
            || remote != url.as_str()
            || credential.is_empty()
            || credential.len() > 512
            || !credential.bytes().all(|b| (b'!'..=b'~').contains(&b))
        {
            return Err("fixed Artifacts HTTPS remote and existing credential required".into());
        }
        let mut builder = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30));
        if let Some(ca) = fixture_ca {
            builder = builder.tls_certs_only([reqwest::Certificate::from_pem(ca)?]);
        }
        let client = builder.build()?;
        Ok(Self {
            client,
            remote: url,
            credential,
        })
    }
    /// Read the actual bounded remote metadata ref. This never publishes or guesses a pin.
    pub fn head(&self) -> Result<String> {
        let url = format!("{}/info/refs?service=git-receive-pack", self.remote);
        let bytes = bounded_response(
            self.client.get(url).bearer_auth(self.credential).send()?,
            "application/x-git-receive-pack-advertisement",
        )?;
        let lines = packets(&bytes)?;
        if lines.first().copied() != Some(b"# service=git-receive-pack\n") {
            return Err("invalid catalog discovery".into());
        }
        let mut found = None;
        for line in lines.iter().skip(1) {
            let line = line.split(|b| *b == 0).next().ok_or("invalid ref")?;
            let text = std::str::from_utf8(line)?.trim_end_matches('\n');
            if let Some((oid, name)) = text.split_once(' ')
                && name == CATALOG_REF
            {
                if !hex(oid, 40) || found.is_some() {
                    return Err("ambiguous catalog head".into());
                }
                found = Some(oid.to_owned());
            }
        }
        Ok(found.unwrap_or_else(|| ABSENT.to_string()))
    }
    /// Native acceptance and all R2 content must already be durable. The caller rechecks
    /// current authority before this call and again before sending any successful Git ACK.
    pub fn publish(&self, commit: &CatalogCommit) -> Result<()> {
        let head = self.head()?;
        if head == commit.pin {
            return Ok(());
        }
        if head != commit.expected {
            return Err("catalog compare-and-swap conflict".into());
        }
        let mut body = Vec::new();
        packet(
            format!(
                "{} {} {CATALOG_REF}\0report-status\n",
                commit.expected, commit.pin
            )
            .as_bytes(),
            &mut body,
        )?;
        body.extend(b"0000");
        body.extend(&commit.pack);
        let response = self
            .client
            .post(format!("{}/git-receive-pack", self.remote))
            .bearer_auth(self.credential)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-git-receive-pack-request",
            )
            .body(body)
            .send()?;
        let bytes = bounded_response(response, "application/x-git-receive-pack-result")?;
        let report = packets(&bytes)?;
        if report
            != vec![
                b"unpack ok\n".as_slice(),
                format!("ok {CATALOG_REF}\n").as_bytes(),
            ]
        {
            return Err("catalog publication not acknowledged; retry exact operation".into());
        }
        if self.head()? != commit.pin {
            return Err("catalog publication changed before verification".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        sley::Repository::init_bare(root.path()).unwrap();
        let blob =
            oid(git(root.path(), &["hash-object", "-w", "--stdin"], b"{}\n").unwrap()).unwrap();
        let tree = oid(git(
            root.path(),
            &["mktree"],
            format!("100644 blob {blob}\tmanifest.json\n").as_bytes(),
        )
        .unwrap())
        .unwrap();
        let first =
            oid(git(root.path(), &["commit-tree", &tree], b"initial catalog\n").unwrap()).unwrap();
        git(root.path(), &["update-ref", CATALOG_REF, &first], &[]).unwrap();
        (root, first)
    }
    fn manifest(operation: &str, extra: &str) -> Vec<u8> {
        canonical(&serde_json::json!({"schema":2,"operation":operation,"intent":{"label":extra},"native_receipt":{"receipt":"accepted"}})).unwrap()
    }
    #[test]
    fn real_git_catalog_cas_replay_and_metadata_only_objects() {
        let (catalog, old) = fixture();
        let operation = "a".repeat(64);
        let bytes = manifest(&operation, "first");
        let first = CatalogCommit::prepare(&old, &operation, &bytes).unwrap();
        let repeated = CatalogCommit::prepare(&old, &operation, &bytes).unwrap();
        assert_eq!(first.pin, repeated.pin);
        assert_eq!(first.pack, repeated.pack);
        assert_eq!(first.metadata_bytes, bytes.len());
        assert_eq!(first.manifest_sha256, digest(&bytes));
        first.publish_local_fixture(catalog.path()).unwrap();
        repeated.publish_local_fixture(catalog.path()).unwrap();
        assert_eq!(
            git(
                catalog.path(),
                &["show", &format!("{}:manifest.json", first.pin)],
                &[]
            )
            .unwrap(),
            bytes
        );
        let loser =
            CatalogCommit::prepare(&old, &"b".repeat(64), &manifest(&"b".repeat(64), "second"))
                .unwrap();
        assert!(loser.publish_local_fixture(catalog.path()).is_err());
        assert_eq!(
            oid(git(catalog.path(), &["rev-parse", CATALOG_REF], &[]).unwrap()).unwrap(),
            first.pin
        );
        assert_eq!(
            git(catalog.path(), &["ls-tree", "--name-only", &first.pin], &[]).unwrap(),
            b"manifest.json\n"
        );
    }
    #[test]
    fn noncanonical_source_content_oversize_and_unapproved_remote_refused() {
        let expected = "a".repeat(40);
        let op = "b".repeat(64);
        assert!(CatalogCommit::prepare(&expected, &op, &vec![b'x'; MAX_METADATA + 1]).is_err());
        let raw = canonical(&serde_json::json!({"schema":2,"operation":op,"intent":{"bytes":"secret source"},"native_receipt":{}})).unwrap();
        assert!(CatalogCommit::prepare(&expected, &op, &raw).is_err());
        for remote in [
            "https://evil.invalid/git/ns/repo.git",
            "http://abc.artifacts.cloudflare.net/git/ns/repo.git",
            "https://user@abc.artifacts.cloudflare.net/git/ns/repo.git",
            "https://abc.artifacts.cloudflare.net/git/ns/repo.git?x=1",
        ] {
            assert!(ArtifactsGit::new(remote, "PUBLIC_TEST_VECTOR").is_err());
        }
    }
    #[test]
    fn native_bootstrap_uses_expected_absent_cas_without_fake_parent() {
        let directory = tempfile::tempdir().unwrap();
        sley::Repository::init_bare(directory.path()).unwrap();
        let op = "c".repeat(64);
        let commit =
            CatalogCommit::prepare(ABSENT, &op, &manifest(&op, "native-bootstrap")).unwrap();
        commit.publish_local_fixture(directory.path()).unwrap();
        commit.publish_local_fixture(directory.path()).unwrap();
        let parents = git(
            directory.path(),
            &["rev-list", "--parents", "-n", "1", &commit.pin],
            &[],
        )
        .unwrap();
        assert_eq!(std::str::from_utf8(&parents).unwrap().trim(), commit.pin);
        let other = CatalogCommit::prepare(
            ABSENT,
            &"d".repeat(64),
            &manifest(&"d".repeat(64), "other-bootstrap"),
        )
        .unwrap();
        assert!(other.publish_local_fixture(directory.path()).is_err());
    }
    #[test]
    fn malformed_git_control_is_rejected() {
        for bytes in [b"0001".as_slice(), b"0003", b"zzzz", b"0008abc", b"0000bad"] {
            assert!(packets(bytes).is_err());
        }
    }
}
