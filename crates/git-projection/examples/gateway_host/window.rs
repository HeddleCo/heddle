// SPDX-License-Identifier: Apache-2.0
//! Explicit loopback-only, local synthetic Git window. No hosted authority is minted.
use crate::{
    Result,
    policy::{canonical, digest, hex, now},
    transport,
};
use heddle_git_projection::{
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{
        LocalPush, LocalWriter, PushReceipt, PushUpdate, WriteLimits, accept_local_fixture_push,
        parse_receive_pack,
    },
};
use objects::object::{StateId, ThreadName};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u64,
    scope: String,
    expires_at: u64,
    repository: String,
    thread: String,
    native: PathBuf,
    catalog: PathBuf,
    reader_sha256: String,
    writer_sha256: String,
    service_sha256: String,
    actor: String,
    policy_generation: String,
}
fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn load(path: &Path) -> Result<Config> {
    let mut bytes = Vec::new();
    File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    let c: Config = serde_json::from_slice(&bytes)?;
    let time = now()?;
    if bytes.len() > 65536
        || canonical(&c)? != bytes
        || c.schema != 1
        || c.scope != "quiescent-synthetic-local"
        || !(time < c.expires_at && c.expires_at <= time + 3600)
        || !safe_name(&c.repository)
        || c.thread.len() > 255
        || !c.thread.split('/').all(safe_name)
        || !c.native.is_absolute()
        || !c.catalog.is_absolute()
        || c.native == c.catalog
        || !hex(&c.reader_sha256, 64)
        || !hex(&c.writer_sha256, 64)
        || !hex(&c.service_sha256, 64)
        || c.reader_sha256 == c.writer_sha256
        || c.reader_sha256 == c.service_sha256
        || c.writer_sha256 == c.service_sha256
        || !safe_name(&c.actor)
        || !hex(&c.policy_generation, 64)
    {
        return Err("invalid or expired local window policy".into());
    }
    Ok(c)
}
fn token(header: Option<&str>, expected: &str) -> Result<()> {
    let value = header
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or("missing credential")?;
    if !(32..=256).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        || digest(value.as_bytes())
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |a, (b, c)| a | (b ^ c))
            != 0
    {
        return Err("denied".into());
    }
    Ok(())
}
fn authorize(
    path: &Path,
    expected: &Config,
    request: &transport::Request,
    write: bool,
) -> Result<()> {
    let c = load(path)?;
    if &c != expected {
        return Err("window policy changed".into());
    }
    token(request.header("authorization"), &c.reader_sha256)?;
    token(
        request.header("x-gateway-service-authorization"),
        &c.service_sha256,
    )?;
    if write {
        token(
            request.header("x-gateway-write-authorization"),
            &c.writer_sha256,
        )?;
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Version {
    schema: u64,
    mode: String,
    repository: String,
    thread: String,
    native_state: StateId,
    git_oid: String,
    receipt: Option<PushReceipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    old: String,
    new: String,
    expected_native: StateId,
    actor: String,
    policy_generation: String,
}
fn read_json<T: serde::de::DeserializeOwned + Serialize>(path: &Path) -> Result<T> {
    let mut data = Vec::new();
    File::open(path)?.take(65537).read_to_end(&mut data)?;
    let value: T = serde_json::from_slice(&data)?;
    if data.len() > 65536 || canonical(&value)? != data {
        return Err("noncanonical local catalog".into());
    }
    Ok(value)
}
fn durable(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("missing parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn head(repo: &repo::Repository, thread: &str) -> Result<StateId> {
    let projection = repo.native_thread(thread)?.projection()?;
    match projection.source_heads.as_slice() {
        [] => Ok(projection.genesis.base),
        [state] => Ok(*state),
        _ => Err("conflicted native heads".into()),
    }
}
fn git(path: &Path, args: &[&str], input: &[u8], pack: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(path).args(args);
    let output = tempfile::tempfile()?;
    let status = if pack {
        transport::run_bounded_pack(&mut command, input, output)?
    } else {
        transport::run_bounded(&mut command, input, output)?
    };
    if !status.success() {
        return Err("bounded Git operation failed".into());
    }
    Ok(())
}
fn project(c: &Config, repo: &repo::Repository, state: StateId, path: &Path) -> Result<String> {
    if path.exists() {
        return Err("Git projection must be empty".into());
    }
    let sink = sley::Repository::init_bare(path)?;
    let mapping = export_public_git_history(
        repo,
        &sink,
        &[HistoryTip {
            thread: &c.thread,
            state,
        }],
        &[&c.thread],
        ViewLimits::default(),
    )?;
    let oid = mapping
        .get_git(&state)
        .ok_or("missing projected tip")?
        .to_string();
    drop(sink);
    git(
        path,
        &["update-ref", &format!("refs/heads/{}", c.thread), &oid],
        &[],
        false,
    )?;
    git(
        path,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{}", c.thread)],
        &[],
        false,
    )?;
    git(path, &["config", "http.receivepack", "false"], &[], false)?;
    git(
        path,
        &["config", "uploadpack.allowAnySHA1InWant", "false"],
        &[],
        false,
    )?;
    git(
        path,
        &["config", "uploadpack.allowReachableSHA1InWant", "false"],
        &[],
        false,
    )?;
    git(path, &["fsck", "--strict", "--no-reflogs"], &[], false)?;
    Ok(oid)
}
fn current(c: &Config) -> Result<Version> {
    let version: Version = read_json(&c.catalog.join("current.json"))?;
    validate_version(c, &version)?;
    Ok(version)
}
fn validate_version(c: &Config, version: &Version) -> Result<()> {
    if version.schema != 1
        || version.mode != "full-history"
        || version.repository != c.repository
        || version.thread != c.thread
        || !hex(&version.git_oid, 40)
    {
        return Err("invalid local publication".into());
    }
    Ok(())
}
fn publish(c: &Config, repo: &repo::Repository, old: &Version, receipt: PushReceipt) -> Result<()> {
    if head(repo, &c.thread)? != receipt.native_state {
        return Err("accepted head has since changed".into());
    }
    let temp = tempfile::tempdir()?;
    let oid = project(
        c,
        repo,
        receipt.native_state,
        &temp.path().join("verify.git"),
    )?;
    if oid != receipt.new_git {
        return Err("accepted Git identity changed".into());
    }
    let name = ThreadName::new(&c.thread);
    let recorded = repo.refs().get_thread(&name)?;
    if recorded != Some(old.native_state) && recorded != Some(receipt.native_state) {
        return Err("native compatibility ref changed".into());
    }
    repo.set_thread_recorded(&name, &receipt.native_state)?;
    let present = current(c)?;
    if present.git_oid == receipt.new_git && present.native_state == receipt.native_state {
        return Ok(());
    }
    if &present != old {
        return Err("catalog compare-and-swap conflict".into());
    }
    // Explicit local fault-injection seam; never present in the hosted read path.
    if c.catalog.join("publication-paused").exists() {
        return Err("native committed; local publication pending".into());
    }
    let version = Version {
        schema: 1,
        mode: "full-history".into(),
        repository: c.repository.clone(),
        thread: c.thread.clone(),
        native_state: receipt.native_state,
        git_oid: oid,
        receipt: Some(receipt),
    };
    durable(
        &c.catalog
            .join("versions")
            .join(format!("{}.json", version.git_oid)),
        &canonical(&version)?,
    )?;
    durable(&c.catalog.join("current.json"), &canonical(&version)?)?;
    Ok(())
}
fn recover(
    c: &Config,
    path: &Path,
    request: &transport::Request,
    repo: &repo::Repository,
    old: &Version,
) -> Result<()> {
    if head(repo, &c.thread)? == old.native_state {
        return Ok(());
    }
    let p: Pending = read_json(&c.catalog.join("pending.json"))?;
    if p.old != old.git_oid
        || p.expected_native != old.native_state
        || p.actor != c.actor
        || p.policy_generation != c.policy_generation
    {
        return Err("pending publication scope mismatch".into());
    }
    let update = PushUpdate {
        thread: c.thread.clone(),
        old: p.old.parse()?,
        new: p.new.parse()?,
    };
    let temp = tempfile::tempdir()?;
    let empty = sley::Repository::init_bare(temp.path().join("replay.git"))?;
    let signer = repo.native_thread_signer(&repo.native_thread(&c.thread)?)?;
    let accepted = accept_local_fixture_push(
        repo,
        &empty,
        LocalPush {
            update: &update,
            expected_native: p.expected_native,
            policy_generation: &c.policy_generation,
        },
        LocalWriter {
            actor: &c.actor,
            signer: &signer,
        },
        WriteLimits::default(),
        || {
            authorize(path, c, request, true)
                .map_err(|e| heddle_git_projection::GitProjectionError::Git(e.to_string()))
        },
    )?;
    if !accepted.replayed {
        return Err("recovery requires durable prior receipt".into());
    }
    publish(c, repo, old, accepted.receipt)?;
    Ok(())
}
fn packet(body: &[u8], out: &mut Vec<u8>) {
    out.extend(format!("{:04x}", body.len() + 4).as_bytes());
    out.extend(body);
}
fn serve(request: &mut transport::Request, path: &Path, startup: &Config) -> Result<()> {
    let c = load(path)?;
    if c.repository != startup.repository
        || c.native != startup.native
        || c.catalog != startup.catalog
        || c.thread != startup.thread
        || c.actor != startup.actor
    {
        return Err("restart required for window identity change".into());
    }
    if request.pin != c.repository {
        return Err("ungranted repository".into());
    }
    let write =
        request.endpoint == "git-receive-pack" || request.query == "service=git-receive-pack";
    authorize(path, &c, request, write)?;
    let repo = repo::Repository::open(&c.native)?;
    local_source(&repo)?;
    // Shared with all supported local visibility mutation paths, plus native SQL CAS.
    let _lock = objects::lock::RepoLock::at(repo.heddle_dir().join("locks/repo.lock")).write()?;
    let mut version = current(&c)?;
    if write {
        recover(&c, path, request, &repo, &version)?;
        version = current(&c)?;
    }
    if head(&repo, &c.thread)? != version.native_state {
        return Err("native accepted; publication pending".into());
    }
    if write && request.method == "GET" {
        // Revalidate the complete current disclosure closure before advertising.
        let temp = tempfile::tempdir()?;
        if project(
            &c,
            &repo,
            version.native_state,
            &temp.path().join("public.git"),
        )? != version.git_oid
        {
            return Err("catalog identity mismatch".into());
        }
        let mut body = Vec::new();
        packet(b"# service=git-receive-pack\n", &mut body);
        body.extend(b"0000");
        packet(
            format!(
                "{} refs/heads/{}\0report-status ofs-delta object-format=sha1\n",
                version.git_oid, c.thread
            )
            .as_bytes(),
            &mut body,
        );
        body.extend(b"0000");
        authorize(path, &c, request, true)?;
        return transport::reply_git(
            request,
            "application/x-git-receive-pack-advertisement",
            &body,
        );
    }
    let temp = tempfile::tempdir()?;
    let git_path = temp.path().join("public.git");
    if !write {
        if project(&c, &repo, version.native_state, &git_path)? != version.git_oid {
            return Err("catalog identity mismatch".into());
        }
        let response = transport::prepare_git(request, &git_path)?;
        authorize(path, &c, request, false)?;
        if head(&repo, &c.thread)? != version.native_state {
            return Err("native head changed".into());
        }
        return response.send(request);
    }
    let receive = parse_receive_pack(&request.body, WriteLimits::default())?;
    if receive.update.thread != c.thread {
        return Err("ungranted branch".into());
    }
    // Keep prior immutable metadata so a repeated POST can recover after a lost ACK.
    let old: Version = read_json(
        &c.catalog
            .join("versions")
            .join(format!("{}.json", receive.update.old)),
    )?;
    validate_version(&c, &old)?;
    if old.repository != c.repository
        || old.thread != c.thread
        || old.git_oid != receive.update.old.to_string()
    {
        return Err("unknown previous publication".into());
    }
    if project(&c, &repo, old.native_state, &git_path)? != old.git_oid {
        return Err("old closure unavailable".into());
    }
    git(
        &git_path,
        &["index-pack", "--stdin", "--fix-thin", "--strict"],
        receive.pack,
        true,
    )?;
    git(&git_path, &["fsck", "--strict", "--no-reflogs"], &[], false)?;
    let quarantine = sley::Repository::open(&git_path)?;
    let pending = Pending {
        old: old.git_oid.clone(),
        new: receive.update.new.to_string(),
        expected_native: old.native_state,
        actor: c.actor.clone(),
        policy_generation: c.policy_generation.clone(),
    };
    // Do not overwrite a different committed-but-unpublished operation.
    if version.git_oid != old.git_oid && version.git_oid != pending.new {
        return Err("non-fast-forward publication".into());
    }
    durable(&c.catalog.join("pending.json"), &canonical(&pending)?)?;
    let signer = repo.native_thread_signer(&repo.native_thread(&c.thread)?)?;
    let accepted = accept_local_fixture_push(
        &repo,
        &quarantine,
        LocalPush {
            update: &receive.update,
            expected_native: old.native_state,
            policy_generation: &c.policy_generation,
        },
        LocalWriter {
            actor: &c.actor,
            signer: &signer,
        },
        WriteLimits::default(),
        || {
            authorize(path, &c, request, true)
                .map_err(|e| heddle_git_projection::GitProjectionError::Git(e.to_string()))
        },
    )?;
    publish(&c, &repo, &old, accepted.receipt)?;
    authorize(path, &c, request, true)?;
    let mut body = Vec::new();
    packet(b"unpack ok\n", &mut body);
    packet(
        format!("ok refs/heads/{}\n", c.thread).as_bytes(),
        &mut body,
    );
    body.extend(b"0000");
    transport::reply_git(request, "application/x-git-receive-pack-result", &body)
}
fn local_source(repo: &repo::Repository) -> Result<()> {
    if repo.heddle_dir().join("objectstore").exists()
        || repo.heddle_dir().join("lazy-hydrator.toml").exists()
    {
        return Err("local window refuses hosted native source".into());
    }
    Ok(())
}
pub fn run(path: &Path, bind: &str) -> Result<()> {
    let address: SocketAddr = bind.parse()?;
    if !address.ip().is_loopback() {
        return Err("local Git window requires loopback".into());
    }
    let c = load(path)?;
    let repo = repo::Repository::open(&c.native)?;
    local_source(&repo)?;
    let lock = objects::lock::RepoLock::at(repo.heddle_dir().join("locks/repo.lock")).write()?;
    fs::create_dir_all(c.catalog.join("versions"))?;
    File::open(&c.catalog)?.sync_all()?;
    File::open(c.catalog.parent().ok_or("missing catalog parent")?)?.sync_all()?;
    if !c.catalog.join("current.json").exists() {
        let state = head(&repo, &c.thread)?;
        let temp = tempfile::tempdir()?;
        let oid = project(&c, &repo, state, &temp.path().join("initial.git"))?;
        let version = Version {
            schema: 1,
            mode: "full-history".into(),
            repository: c.repository.clone(),
            thread: c.thread.clone(),
            native_state: state,
            git_oid: oid,
            receipt: None,
        };
        durable(
            &c.catalog
                .join("versions")
                .join(format!("{}.json", version.git_oid)),
            &canonical(&version)?,
        )?;
        durable(&c.catalog.join("current.json"), &canonical(&version)?)?;
    } else {
        current(&c)?;
    }
    drop(lock);
    drop(repo);
    let listener = TcpListener::bind(address)?;
    let host = listener.local_addr()?.to_string();
    println!("Local synthetic Git window listening at {host}");
    for stream in listener.incoming() {
        let stream = stream?;
        let mut fallback = stream.try_clone()?;
        match transport::read_window_request(stream, &host, &c.repository) {
            Ok(mut request) => {
                if let Err(error) = serve(&mut request, path, &c) {
                    eprintln!("local window request denied: {error}");
                    let _=request.reply(403,b"Window unavailable or access denied; native acceptance may require publication recovery\n");
                }
            }
            Err(_) => {
                let _ = transport::reply_stream(&mut fallback, 400, b"Invalid request\n");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(root: &Path) -> Config {
        Config {
            schema: 1,
            scope: "quiescent-synthetic-local".into(),
            expires_at: now().unwrap() + 600,
            repository: "toy".into(),
            thread: "main".into(),
            native: root.join("native"),
            catalog: root.join("catalog"),
            reader_sha256: digest(b"rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr"),
            writer_sha256: digest(b"wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww"),
            service_sha256: digest(b"ssssssssssssssssssssssssssssssss"),
            actor: "fixture-writer".into(),
            policy_generation: "a".repeat(64),
        }
    }
    #[test]
    fn local_policy_is_canonical_bounded_and_separates_write_authority() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("policy.json");
        let mut c = policy(root.path());
        fs::write(&file, canonical(&c).unwrap()).unwrap();
        assert!(load(&file).is_ok());
        c.writer_sha256 = c.reader_sha256.clone();
        fs::write(&file, canonical(&c).unwrap()).unwrap();
        assert!(load(&file).is_err());
        c = policy(root.path());
        c.scope = "hosted".into();
        fs::write(&file, canonical(&c).unwrap()).unwrap();
        assert!(load(&file).is_err());
        c = policy(root.path());
        c.expires_at = now().unwrap();
        fs::write(&file, canonical(&c).unwrap()).unwrap();
        assert!(load(&file).is_err());
        c = policy(root.path());
        c.repository = "-bad".into();
        fs::write(&file, canonical(&c).unwrap()).unwrap();
        assert!(load(&file).is_err());
        let c = policy(root.path());
        let mut bytes = canonical(&c).unwrap();
        bytes.insert(1, b' ');
        fs::write(&file, bytes).unwrap();
        assert!(load(&file).is_err());
        assert!(
            token(
                Some("Bearer rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr"),
                &c.reader_sha256
            )
            .is_ok()
        );
        assert!(
            token(
                Some("Bearer rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr"),
                &c.writer_sha256
            )
            .is_err()
        );
    }
    #[test]
    fn catalog_metadata_rejects_duplicates_unknown_fields_and_partial_writes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pending.json");
        let value = serde_json::json!({"one":1});
        durable(&path, &canonical(&value).unwrap()).unwrap();
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap(), value);
        fs::write(&path, b"{\"one\":1,\"one\":1}\n").unwrap();
        assert!(read_json::<serde_json::Value>(&path).is_err());
        fs::write(&path, b"{\"one\":").unwrap();
        assert!(read_json::<serde_json::Value>(&path).is_err());
    }
}
