// SPDX-License-Identifier: Apache-2.0
//! Disposable, bounded per-closure projection cache. Never a source Git mirror.
use crate::{
    Result,
    policy::{Config, Disclosure, Manifest, canonical, digest},
    transport::run_bounded,
};
use heddle_git_projection::gateway_view::{
    ViewLimits, export_public_native_view_with_authorized_threads,
};
use objects::object::StateId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
pub const MAX_BUNDLE: usize = 8 * 1024 * 1024;
const MAX_DISK: u64 = 96 * 1024 * 1024;
const MAX_CACHE: u64 = 128 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub manifest: Manifest,
    pub authorized_threads: Vec<String>,
    pub native_sha256: String,
}

pub fn project(directory: &Path) -> Result<()> {
    let bytes = fs::read(directory.join("job.json"))?;
    if bytes.len() > 64 * 1024 {
        return Err("projection job limit".into());
    }
    let job: Job = serde_json::from_slice(&bytes)?;
    job.manifest.validate()?;
    let mut bundle = Vec::new();
    File::open(directory.join("native.bundle"))?
        .take((MAX_BUNDLE + 1) as u64)
        .read_to_end(&mut bundle)?;
    if bundle.len() > MAX_BUNDLE || digest(&bundle) != job.native_sha256 {
        return Err("native digest mismatch".into());
    }
    let native = directory.join("native");
    fs::create_dir(&native)?;
    unpack(&bundle, &native)?;
    if native.join(".heddle/objectstore").exists()
        || native.join(".heddle/lazy-hydrator.toml").exists()
    {
        return Err("remote native source refused".into());
    }
    let repository = repo::Repository::open(&native)?;
    let destination = directory.join("view.git");
    if destination.exists() {
        return Err("projection sink must be fresh".into());
    }
    let sink = sley::Repository::init_bare(&destination)?;
    let names: Vec<&str> = job.authorized_threads.iter().map(String::as_str).collect();
    let oid = export_public_native_view_with_authorized_threads(
        &repository,
        &sink,
        StateId::parse(&job.manifest.state)?,
        &job.manifest.thread,
        &names,
        job.manifest.mode == "snapshot",
        ViewLimits::default(),
    )?;
    if oid.to_string() != job.manifest.git_oid {
        return Err("pinned Git OID mismatch".into());
    }
    drop(sink);
    drop(repository);
    for args in [
        vec!["update-ref", "refs/heads/main", &job.manifest.git_oid],
        vec!["symbolic-ref", "HEAD", "refs/heads/main"],
        vec!["config", "http.receivepack", "false"],
        vec!["config", "uploadpack.allowAnySHA1InWant", "false"],
        vec!["config", "uploadpack.allowReachableSHA1InWant", "false"],
        vec!["repack", "-ad"],
        vec!["fsck", "--strict", "--no-reflogs"],
    ] {
        let status = clean_command("git")
            .arg("--git-dir")
            .arg(&destination)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("Git projection validation failed".into());
        }
    }
    tree_size(&destination)?;
    // No native bytes or identity retained in the projection cache.
    fs::remove_dir_all(native)?;
    if directory.join("home").exists() {
        fs::remove_dir_all(directory.join("home"))?;
    }
    fs::remove_file(directory.join("native.bundle"))?;
    fs::remove_file(directory.join("job.json"))?;
    Ok(())
}

pub fn clean_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    command
}
fn unpack(data: &[u8], root: &Path) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for entry in tar::Archive::new(data).entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let text = path.to_str().ok_or("non-UTF8 bundle path")?;
        let parts: Vec<_> = path.components().collect();
        if !entry.header().entry_type().is_file()
            || parts.len() < 2
            || parts.first() != Some(&Component::Normal(".heddle".as_ref()))
            || parts.iter().any(|p| !matches!(p, Component::Normal(_)))
            || text.contains('\\')
            || text.split('/').any(|p| {
                p.is_empty()
                    || p == "."
                    || [
                        "identity.toml",
                        "git-projection",
                        "state",
                        "materialized-roots",
                    ]
                    .contains(&p)
            })
            || !seen.insert(text.to_lowercase())
            || seen.len() > 10000
        {
            return Err("unsafe native bundle entry".into());
        }
        let size = entry.size();
        total = total.checked_add(size).ok_or("bundle size overflow")?;
        if total > MAX_BUNDLE as u64 {
            return Err("expanded bundle limit".into());
        }
        let target = root.join(&path);
        fs::create_dir_all(target.parent().ok_or("missing parent")?)?;
        let mut out = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(target)?;
        if std::io::copy(&mut entry.by_ref().take(size + 1), &mut out)? != size {
            return Err("bundle length mismatch".into());
        }
    }
    if seen.is_empty() {
        return Err("empty native bundle".into());
    }
    Ok(())
}
fn tree_size(root: &Path) -> Result<u64> {
    fn walk(root: &Path, count: &mut usize, total: &mut u64) -> Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            *count += 1;
            let meta = entry.path().symlink_metadata()?;
            if *count > 20000 || (!meta.is_dir() && !meta.is_file()) {
                return Err("unsafe projection cache layout".into());
            }
            if meta.is_dir() {
                walk(&entry.path(), count, total)?;
            } else {
                *total = total
                    .checked_add(meta.len())
                    .ok_or("projection size overflow")?;
            }
            if *total > MAX_DISK {
                return Err("projection disk limit".into());
            }
        }
        Ok(())
    }
    let mut total = 0;
    walk(root, &mut 0, &mut total)?;
    Ok(total)
}
fn tree_digest(root: &Path) -> Result<String> {
    fn walk(root: &Path, relative: &Path, hasher: &mut Sha256) -> Result<()> {
        let mut entries =
            fs::read_dir(root.join(relative))?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = relative.join(entry.file_name());
            let meta = entry.path().symlink_metadata()?;
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            if meta.is_dir() {
                hasher.update(b"dir");
                walk(root, &path, hasher)?;
            } else if meta.is_file() {
                hasher.update(meta.len().to_be_bytes());
                let mut file = File::open(entry.path())?;
                let mut bytes = [0u8; 65536];
                loop {
                    let n = file.read(&mut bytes)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&bytes[..n]);
                }
            } else {
                return Err("unsafe projection layout".into());
            }
        }
        Ok(())
    }
    let mut hasher = Sha256::new();
    walk(root, Path::new(""), &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        } else if meta.is_file() {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        } else {
            return Err("unsafe cached projection".into());
        }
    }
    Ok(())
}
struct Entry {
    key: String,
    root: TempDir,
    bytes: u64,
    created: Instant,
    fingerprint: String,
}
pub struct Cache {
    entries: VecDeque<Entry>,
    enabled: bool,
    pub hits: u64,
    pub misses: u64,
}
impl Cache {
    pub fn new(enabled: bool) -> Self {
        Self {
            entries: VecDeque::new(),
            enabled,
            hits: 0,
            misses: 0,
        }
    }
    pub fn key(c: &Config, m: &Manifest, d: &Disclosure) -> Result<String> {
        // Include both authority generation and complete immutable source/view/Thread scope.
        let mut threads = c.threads(m)?.to_vec();
        threads.sort();
        Ok(digest(&canonical(
            &serde_json::json!({"projection_version":1,"native":c.bundle(m)?,"manifest":m,"threads":threads,"generation":d.generation,"audience":d.audience,"reader":d.reader_sha256,"authority":d.authority}),
        )?))
    }
    pub fn materialize(
        &mut self,
        c: &Config,
        m: &Manifest,
        d: &Disclosure,
        target: &Path,
        read: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<()> {
        let key = Self::key(c, m, d)?;
        self.entries
            .retain(|e| e.created.elapsed() < Duration::from_secs(60));
        if self.enabled
            && let Some(index) = self.entries.iter().position(|e| e.key == key)
        {
            let entry = self
                .entries
                .remove(index)
                .ok_or("cache entry disappeared")?;
            tree_size(&entry.root.path().join("view.git"))?;
            if tree_digest(&entry.root.path().join("view.git"))? != entry.fingerprint {
                return Err("cached projection integrity mismatch".into());
            }
            copy_tree(&entry.root.path().join("view.git"), target)?;
            self.entries.push_back(entry);
            self.hits += 1;
            return Ok(());
        }
        self.misses += 1;
        let bytes = read()?;
        if bytes.len() > MAX_BUNDLE || digest(&bytes) != c.bundle(m)? {
            return Err("native bundle digest mismatch".into());
        }
        let temp = tempfile::Builder::new()
            .prefix("heddle-projection-")
            .tempdir()?;
        let job = Job {
            manifest: m.clone(),
            authorized_threads: c.threads(m)?.to_vec(),
            native_sha256: c.bundle(m)?.into(),
        };
        fs::write(temp.path().join("job.json"), canonical(&job)?)?;
        fs::write(temp.path().join("native.bundle"), bytes)?;
        let mut command = clean_command(std::env::current_exe()?);
        command
            .arg("--project")
            .arg(temp.path())
            .env("HEDDLE_HOME", temp.path().join("home"))
            .env("HEDDLE_PRINCIPAL_NAME", "Synthetic Demo")
            .env("HEDDLE_PRINCIPAL_EMAIL", "demo@example.invalid")
            .stderr(Stdio::null());
        let output = tempfile::tempfile()?;
        if !run_bounded(&mut command, b"", output)?.success() {
            return Err("native projection refused".into());
        }
        let size = tree_size(&temp.path().join("view.git"))?;
        // Capture successful projection before insertion. Failed builds never poison a hit.
        copy_tree(&temp.path().join("view.git"), target)?;
        if self.enabled {
            while self.entries.len() >= 4
                || self.entries.iter().map(|e| e.bytes).sum::<u64>() + size > MAX_CACHE
            {
                self.entries.pop_front();
            }
            let fingerprint = tree_digest(&temp.path().join("view.git"))?;
            self.entries.push_back(Entry {
                key,
                root: temp,
                bytes: size,
                created: Instant::now(),
                fingerprint,
            });
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_symlink_and_identity_denied() {
        for name in [".heddle/identity.toml", ".heddle/git-projection/data"] {
            let mut bytes = Vec::new();
            {
                let mut tar = tar::Builder::new(&mut bytes);
                let mut header = tar::Header::new_gnu();
                header.set_size(1);
                header.set_mode(0o600);
                header.set_cksum();
                tar.append_data(&mut header, name, &b"x"[..]).expect("tar");
                tar.finish().expect("finish");
            }
            let temp = tempfile::tempdir().expect("temp");
            assert!(unpack(&bytes, temp.path()).is_err(), "{name}");
        }
    }
    fn cache_fixture() -> (Config, Manifest, Disclosure, Cache) {
        let m = Manifest {
            schema: 1,
            repository: "repo".into(),
            source: "source".into(),
            thread: "main".into(),
            state: format!("hs-{}", "0".repeat(52)),
            git_oid: "1".repeat(40),
            mode: "snapshot".into(),
            policy_epoch: 1,
        };
        let c = Config {
            schema: 1,
            expires_at: crate::policy::now().expect("time") + 600,
            published_pins: vec!["2".repeat(40)],
            reader_sha256: "3".repeat(64),
            service_sha256: "4".repeat(64),
            views: vec![m.clone()],
            descriptors: [("source".into(), "5".repeat(64))].into(),
            authorized_threads: [("source".into(), vec!["main".into()])].into(),
        };
        let d = Disclosure {
            schema: 1,
            authority: "quiescent-synthetic".into(),
            allow: true,
            audience: "public".into(),
            reader_sha256: c.reader_sha256.clone(),
            pin: c.published_pins[0].clone(),
            manifest_sha256: digest(&canonical(&m).expect("manifest")),
            native_sha256: c.descriptors["source"].clone(),
            threads_sha256: digest(&canonical(&vec!["main"]).expect("threads")),
            generation: "6".repeat(64),
            expires_at: crate::policy::now().expect("time") + 25,
        };
        let temp = tempfile::tempdir().expect("cache");
        fs::create_dir(temp.path().join("view.git")).expect("dir");
        fs::write(temp.path().join("view.git/HEAD"), b"verified fixture").expect("file");
        let fingerprint = tree_digest(&temp.path().join("view.git")).expect("digest");
        let mut cache = Cache::new(true);
        cache.entries.push_back(Entry {
            key: Cache::key(&c, &m, &d).expect("key"),
            root: temp,
            bytes: 16,
            created: Instant::now(),
            fingerprint,
        });
        (c, m, d, cache)
    }
    #[test]
    fn cached_bytes_are_checked_and_request_copies_cannot_mutate_cache() {
        let (c, m, d, mut cache) = cache_fixture();
        let temp = tempfile::tempdir().expect("requests");
        let first = temp.path().join("one");
        cache
            .materialize(&c, &m, &d, &first, || Err("unexpected read".into()))
            .expect("hit");
        fs::write(first.join("HEAD"), b"request mutation").expect("mutate request");
        let second = temp.path().join("two");
        cache
            .materialize(&c, &m, &d, &second, || Err("unexpected read".into()))
            .expect("hit");
        assert_eq!(
            fs::read(second.join("HEAD")).expect("read"),
            b"verified fixture"
        );
        fs::write(
            cache.entries[0].root.path().join("view.git/HEAD"),
            b"corruption",
        )
        .expect("corrupt cache");
        let target = temp.path().join("three");
        assert!(
            cache
                .materialize(&c, &m, &d, &target, || Err("unexpected read".into()))
                .is_err()
        );
        assert!(!target.exists());
        assert!(cache.entries.is_empty());
    }
    #[test]
    fn expired_or_disabled_cache_requires_native_revalidation() {
        for disabled in [false, true] {
            let (c, m, d, mut cache) = cache_fixture();
            if disabled {
                cache.enabled = false;
            } else {
                cache.entries[0].created = Instant::now() - Duration::from_secs(61);
            }
            let mut reads = 0;
            let temp = tempfile::tempdir().expect("request");
            assert!(
                cache
                    .materialize(&c, &m, &d, &temp.path().join("view"), || {
                        reads += 1;
                        Err("native unavailable".into())
                    })
                    .is_err()
            );
            assert_eq!(reads, 1);
            assert_eq!(cache.hits, 0);
        }
    }
}
