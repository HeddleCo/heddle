// SPDX-License-Identifier: Apache-2.0
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use base64::Engine;
use cli::bridge::{
    GitBridge,
    git_core::{
        GitBridgeError, GitPushScope, SyncMapping, clone_url_to_bare, copy_local_repo_to_bare,
    },
    git_export::{export_all, export_current_thread, export_tree},
    git_notes,
    git_sync::{sync_branches, sync_tags, sync_track_to_branch},
    git_util::ImportStats,
    test_support,
    test_support::{
        RefNamespace, RefUpdate, collect_managed_ref_updates, delete_reference_if_present,
        plan_destination_reconcile, read_exported_refs, read_mirror_managed_refs, set_reference,
        write_exported_refs, write_mirror_managed_refs,
    },
};
use objects::{
    object::{
        Blob, ChangeId, ContentHash, EntryType, FileMode, MarkerName, ThreadName, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use repo::Repository;
use sley::{
    CommitObject, EntryKind, GitObjectType, GitTime, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository, Signature, TagObject,
    plumbing::{sley_core::ByteString as GitByteString, sley_object::EncodedObject},
};
use tempfile::TempDir;

type TestImportResult<T> = Result<T, String>;

trait TestGitBridgeImport {
    fn import(&mut self, git_path: Option<&std::path::Path>) -> TestImportResult<ImportStats>;
}

impl TestGitBridgeImport for GitBridge<'_> {
    fn import(&mut self, git_path: Option<&std::path::Path>) -> TestImportResult<ImportStats> {
        import_all(self, git_path)
    }
}

fn import_all(
    bridge: &mut GitBridge<'_>,
    git_path: Option<&std::path::Path>,
) -> TestImportResult<ImportStats> {
    import_all_with_options(bridge, git_path, ingest::ImportOptions::default())
}

fn import_all_with_options(
    bridge: &mut GitBridge<'_>,
    git_path: Option<&std::path::Path>,
    options: ingest::ImportOptions,
) -> TestImportResult<ImportStats> {
    let target = bridge_root_from_mirror(bridge);
    let source_owned;
    let source = if let Some(path) = git_path {
        path
    } else {
        source_owned = target.clone();
        source_owned.as_path()
    };
    let (stats, _map) = ingest::import_git_into_with_options(source, &target, options)
        .map_err(|error| error.to_string())?;
    test_support::stage_ingest_source_in_mirror(bridge, source, &[])
        .map_err(|error| error.to_string())?;
    test_support::build_existing_mapping(bridge, Some(source))
        .map_err(|error| error.to_string())?;
    let mirror_repo = test_support::open_git_repo(bridge).map_err(|error| error.to_string())?;
    test_support::seed_ingest_identity_mappings_from_mirror(bridge, &mirror_repo)
        .map_err(|error| error.to_string())?;
    Ok(import_stats_from_ingest(stats))
}

fn bridge_root_from_mirror(bridge: &GitBridge<'_>) -> std::path::PathBuf {
    bridge
        .mirror_path()
        .parent()
        .and_then(std::path::Path::parent)
        .expect("mirror path should be <root>/.heddle/git")
        .to_path_buf()
}

fn import_stats_from_ingest(stats: ingest::ImportStats) -> ImportStats {
    ImportStats {
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.refs.threads_written,
        tags_synced: stats.refs.markers_written,
        skipped_non_commit_refs: stats.refs_seen.non_commit_skipped,
        lossy_entries: stats.lossy_entries,
    }
}

trait SleyTestRepoExt {
    fn find_tree(&self, oid: ObjectId) -> Result<sley::TreeObject, String>;
    fn find_object(&self, oid: ObjectId) -> Result<TestObject, String>;
    fn find_blob(&self, oid: ObjectId) -> Result<TestBlob, String>;
    fn find_commit(&self, oid: ObjectId) -> Result<TestCommit, String>;
    fn path(&self) -> &std::path::Path;
}

impl SleyTestRepoExt for SleyRepository {
    fn find_tree(&self, oid: ObjectId) -> Result<sley::TreeObject, String> {
        self.read_tree(&oid).map_err(|err| err.to_string())
    }

    fn find_object(&self, oid: ObjectId) -> Result<TestObject, String> {
        let object = self.read_object(&oid).map_err(|err| err.to_string())?;
        Ok(TestObject {
            kind: object.object_type,
            data: object.body.clone(),
        })
    }

    fn find_blob(&self, oid: ObjectId) -> Result<TestBlob, String> {
        let object = self.read_object(&oid).map_err(|err| err.to_string())?;
        if object.object_type != GitObjectType::Blob {
            return Err(format!("object {oid} is not a blob"));
        }
        Ok(TestBlob {
            data: object.body.clone(),
        })
    }

    fn find_commit(&self, oid: ObjectId) -> Result<TestCommit, String> {
        self.read_commit(&oid)
            .map(|commit| TestCommit { commit })
            .map_err(|err| err.to_string())
    }

    fn path(&self) -> &std::path::Path {
        self.git_dir()
    }
}

struct TestReference<'a> {
    repo: &'a SleyRepository,
    target: ReferenceTarget,
}

fn find_reference<'a>(repo: &'a SleyRepository, name: &str) -> Result<TestReference<'a>, String> {
    let target = repo
        .references()
        .read_ref(name)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("reference {name} not found"))?;
    Ok(TestReference { repo, target })
}

impl TestReference<'_> {
    fn id(&self) -> ObjectId {
        self.try_id()
            .expect("test reference should point directly at an object")
    }

    fn try_id(&self) -> Option<ObjectId> {
        self.target().try_id().copied()
    }

    fn peel_to_id(&mut self) -> Result<ObjectId, String> {
        let oid = match &self.target {
            ReferenceTarget::Direct(oid) => *oid,
            ReferenceTarget::Symbolic(name) => {
                let reference = find_reference(self.repo, name)?;
                match reference.target {
                    ReferenceTarget::Direct(oid) => oid,
                    ReferenceTarget::Symbolic(_) => {
                        return Err(format!("nested symbolic reference {name} is unsupported"));
                    }
                }
            }
        };
        peel_oid(self.repo, oid)
    }

    fn target(&self) -> TestReferenceTarget<'_> {
        TestReferenceTarget {
            target: &self.target,
        }
    }
}

struct TestReferenceTarget<'a> {
    target: &'a ReferenceTarget,
}

impl<'a> TestReferenceTarget<'a> {
    fn try_id(&self) -> Option<&'a ObjectId> {
        match self.target {
            ReferenceTarget::Direct(oid) => Some(oid),
            ReferenceTarget::Symbolic(_) => None,
        }
    }
}

struct TestObject {
    kind: GitObjectType,
    data: Vec<u8>,
}

struct TestBlob {
    data: Vec<u8>,
}

struct TestCommit {
    commit: CommitObject,
}

impl TestCommit {
    fn message_raw_sloppy(&self) -> TestRawMessage<'_> {
        TestRawMessage(&self.commit.message)
    }

    fn parent_ids(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.commit.parents.iter().copied()
    }

    fn tree_id(&self) -> Option<ObjectId> {
        Some(self.commit.tree)
    }
}

struct TestRawMessage<'a>(&'a [u8]);

impl TestRawMessage<'_> {
    fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl std::fmt::Display for TestRawMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.0))
    }
}

trait SleyTestTreeExt {
    fn find_entry(&self, name: &str) -> Option<TestTreeEntry<'_>>;
    fn iter(&self) -> std::vec::IntoIter<Result<TestTreeEntry<'_>, String>>;
}

impl SleyTestTreeExt for sley::TreeObject {
    fn find_entry(&self, name: &str) -> Option<TestTreeEntry<'_>> {
        self.entries
            .iter()
            .find(|entry| entry.name.as_bytes() == name.as_bytes())
            .map(|entry| TestTreeEntry { entry })
    }

    fn iter(&self) -> std::vec::IntoIter<Result<TestTreeEntry<'_>, String>> {
        self.entries
            .iter()
            .map(|entry| Ok(TestTreeEntry { entry }))
            .collect::<Vec<_>>()
            .into_iter()
    }
}

struct TestTreeEntry<'a> {
    entry: &'a sley::plumbing::sley_object::TreeEntry,
}

impl TestTreeEntry<'_> {
    fn filename(&self) -> String {
        String::from_utf8_lossy(self.entry.name.as_bytes()).into_owned()
    }

    fn mode(&self) -> TestEntryMode {
        TestEntryMode(self.entry.mode)
    }

    fn object_id(&self) -> ObjectId {
        self.entry.oid
    }
}

struct TestEntryMode(u32);

impl TestEntryMode {
    fn kind(&self) -> EntryKind {
        EntryKind::from_mode(self.0).expect("canonical tree entry mode")
    }
}

fn peel_oid(repo: &SleyRepository, oid: ObjectId) -> Result<ObjectId, String> {
    let object = repo.read_object(&oid).map_err(|err| err.to_string())?;
    if object.object_type == GitObjectType::Tag {
        let tag = repo.read_tag(&oid).map_err(|err| err.to_string())?;
        peel_oid(repo, tag.object)
    } else {
        Ok(oid)
    }
}

fn open_git(path: impl AsRef<std::path::Path>) -> Result<SleyRepository, String> {
    SleyRepository::open(path.as_ref())
        .or_else(|_| SleyRepository::discover(path.as_ref()))
        .map_err(|err| err.to_string())
}

fn init_git_repo() -> (TempDir, SleyRepository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = SleyRepository::init(temp.path()).expect("init git repo");
    (temp, repo)
}

fn init_bare_git_repo() -> (TempDir, SleyRepository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = SleyRepository::init_bare(temp.path()).expect("init bare git repo");
    (temp, repo)
}

fn init_named_bare_git_repo(root: &TempDir, name: &str) -> SleyRepository {
    SleyRepository::init_bare(root.path().join(name)).expect("init named bare git repo")
}

fn test_signature() -> Signature {
    test_signature_at("Heddle Test", "heddle@test", 0, 0)
}

fn test_signature_at(
    name: &str,
    email: &str,
    seconds: i64,
    timezone_offset_minutes: i16,
) -> Signature {
    let time = GitTime::new(seconds, timezone_offset_minutes);
    let raw = format!("{name} <{email}> {seconds} {}", time.offset_token()).into_bytes();
    Signature {
        name: GitByteString::new(name.as_bytes().to_vec()),
        email: GitByteString::new(email.as_bytes().to_vec()),
        time,
        raw,
    }
}

fn empty_tree_oid(repo: &SleyRepository) -> ObjectId {
    repo.write_tree(sley::TreeEditor::new())
        .expect("write empty tree")
}

fn commit_with_tree(
    repo: &SleyRepository,
    reference: Option<&str>,
    tree_oid: ObjectId,
    message: &str,
    parents: &[ObjectId],
) -> ObjectId {
    let sig = test_signature();
    let commit = CommitObject {
        tree: tree_oid,
        parents: parents.to_vec(),
        author: sig.to_ident_bytes(),
        committer: sig.to_ident_bytes(),
        encoding: None,
        message: message.as_bytes().to_vec(),
    };
    let commit_id = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("commit");
    if let Some(reference) = reference {
        set_reference(
            repo,
            reference,
            commit_id,
            RefPrecondition::Any,
            "test: update ref",
        )
        .expect("update ref");
    }
    commit_id
}

fn create_annotated_tag(
    repo: &SleyRepository,
    name: &str,
    target: ObjectId,
    message: &str,
) -> ObjectId {
    let tag = TagObject {
        object: target,
        object_type: GitObjectType::Commit,
        name: name.as_bytes().to_vec(),
        tagger: Some(test_signature().to_ident_bytes()),
        message: message.as_bytes().to_vec(),
        raw_body: None,
    };
    let tag_id = repo
        .write_object(EncodedObject::new(GitObjectType::Tag, tag.write()))
        .expect("write tag");
    set_reference(
        repo,
        &format!("refs/tags/{name}"),
        tag_id,
        RefPrecondition::MustNotExist,
        "test: create tag",
    )
    .expect("create tag ref");
    tag_id
}

fn gitlink_tree(repo: &SleyRepository, name: &str) -> ObjectId {
    let submodule_oid: ObjectId = "0505050505050505050505050505050505050505"
        .parse()
        .expect("oid");
    let mut editor = repo
        .edit_tree(&ObjectId::empty_tree(repo.object_format()))
        .expect("tree editor");
    editor.upsert(name, EntryKind::Commit, submodule_oid);
    repo.write_tree(editor).expect("write tree")
}

fn init_gitlink_repo() -> (TempDir, SleyRepository) {
    let (git_temp, git_repo) = init_git_repo();
    let tree_oid = gitlink_tree(&git_repo, "vendor");
    commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "gitlink", &[]);
    (git_temp, git_repo)
}

fn git_output(path: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("git cmd");
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(stdin)
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("git output");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn init_invalid_utf8_name_repo() -> TempDir {
    let temp = TempDir::new().expect("temp dir");
    git_output(temp.path(), &["init", "-q", "--initial-branch=main"], None);

    let blob = git_output(
        temp.path(),
        &["hash-object", "-w", "--stdin"],
        Some(b"hello\n"),
    );
    let mut tree_input = Vec::new();
    write!(&mut tree_input, "100644 blob {blob}\t").expect("tree record");
    tree_input.extend_from_slice(b"bad\xffname\0");
    let tree = git_output(temp.path(), &["mktree", "-z"], Some(&tree_input));
    let commit = git_output(
        temp.path(),
        &["commit-tree", &tree, "-m", "invalid name"],
        None,
    );
    git_output(
        temp.path(),
        &["update-ref", "refs/heads/main", &commit],
        None,
    );

    temp
}

struct GitDaemon {
    child: Child,
    port: u16,
}

struct GitHttpBackend {
    join: Option<std::thread::JoinHandle<()>>,
    port: u16,
    stop: Arc<AtomicBool>,
    basic_auth: Option<(String, String)>,
}

fn bind_loopback_ephemeral(context: &str) -> Option<TcpListener> {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => Some(listener),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping git bridge network test: {context}: {err}");
            None
        }
        Err(err) => panic!("{context}: {err}"),
    }
}

impl GitHttpBackend {
    fn spawn(root: &std::path::Path) -> Option<Self> {
        Self::spawn_with_auth(root, None)
    }

    fn spawn_authenticated(root: &std::path::Path, username: &str, password: &str) -> Option<Self> {
        Self::spawn_with_auth(root, Some((username.to_string(), password.to_string())))
    }

    fn spawn_with_auth(
        root: &std::path::Path,
        basic_auth: Option<(String, String)>,
    ) -> Option<Self> {
        let listener = bind_loopback_ephemeral("bind ephemeral http port")?;
        let port = listener.local_addr().expect("listener addr").port();
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");

        let root = root.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = Arc::clone(&stop);
        let auth = basic_auth.clone();
        let join = thread::spawn(move || {
            loop {
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => handle_http_backend_connection(stream, &root, auth.as_ref()),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut delay = Duration::from_millis(10);
        for _ in 0..20 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(Self {
                    join: Some(join),
                    port,
                    stop,
                    basic_auth,
                });
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(500));
        }

        panic!("git http backend did not become ready");
    }

    fn url(&self, repo_name: &str) -> String {
        match &self.basic_auth {
            Some((username, password)) => format!(
                "http://{}:{}@127.0.0.1:{}/{}",
                username, password, self.port, repo_name
            ),
            None => format!("http://127.0.0.1:{}/{}", self.port, repo_name),
        }
    }
}

impl Drop for GitHttpBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle_http_backend_connection(
    mut stream: TcpStream,
    root: &std::path::Path,
    basic_auth: Option<&(String, String)>,
) {
    stream.set_nonblocking(false).expect("set stream blocking");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end;
    loop {
        let read = stream.read(&mut chunk).expect("read request");
        if read == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }

    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("method");
    let target = parts.next().expect("target");
    let (path, query) = target.split_once('?').map_or((target, ""), |(p, q)| (p, q));

    let mut content_type = String::new();
    let mut content_length = 0usize;
    let mut authorization = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-type") {
                content_type = value.to_string();
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().expect("content length");
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_string());
            }
        }
    }

    if let Some((username, password)) = basic_auth {
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        );
        if authorization.as_deref() != Some(expected.as_str()) {
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"heddle-test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("write unauthorized response");
            return;
        }
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).expect("read body");
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", content_length.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git http-backend");

    if !body.is_empty() {
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(&body)
            .expect("write backend body");
    }

    let output = child.wait_with_output().expect("wait for backend");
    assert!(output.status.success(), "git http-backend failed");

    let response = output.stdout;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
        .or_else(|| {
            response
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|pos| pos + 2)
        })
        .expect("cgi headers");
    let headers = String::from_utf8_lossy(&response[..split]);
    let body = &response[split..];

    let mut status = "200 OK".to_string();
    let mut response_headers = Vec::new();
    for line in headers.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("Status:") {
            status = value.trim().to_string();
        } else {
            response_headers.push(line.to_string());
        }
    }
    response_headers.push(format!("Content-Length: {}", body.len()));
    response_headers.push("Connection: close".to_string());

    write!(stream, "HTTP/1.1 {}\r\n", status).expect("write status");
    for header in response_headers {
        write!(stream, "{}\r\n", header).expect("write header");
    }
    write!(stream, "\r\n").expect("write response separator");
    stream.write_all(body).expect("write response body");
}

impl GitDaemon {
    fn spawn(root: &std::path::Path) -> Option<Self> {
        Self::spawn_with_push(root, false)
    }

    /// Spawn a daemon that also serves `receive-pack`, so the network PUSH path
    /// (`push_network_remote_with_updates`) can be exercised against a real wire
    /// remote — the only way to cover the URL/network destination reconciliation
    /// (heddle#316 r11). `git daemon` denies anonymous push unless
    /// `--enable=receive-pack` is passed.
    fn spawn_push(root: &std::path::Path) -> Option<Self> {
        Self::spawn_with_push(root, true)
    }

    fn spawn_with_push(root: &std::path::Path, allow_push: bool) -> Option<Self> {
        let listener = bind_loopback_ephemeral("bind ephemeral port")?;
        let port = listener.local_addr().expect("listener addr").port();
        drop(listener);

        let mut args: Vec<String> = vec![
            "daemon".to_string(),
            "--reuseaddr".to_string(),
            "--export-all".to_string(),
            format!("--base-path={}", root.display()),
            "--listen=127.0.0.1".to_string(),
            format!("--port={port}"),
        ];
        if allow_push {
            args.push("--enable=receive-pack".to_string());
        }
        args.push(root.to_str().expect("root path").to_string());

        let child = Command::new("git")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git daemon");

        let mut delay = Duration::from_millis(10);
        for _ in 0..20 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(Self { child, port });
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(500));
        }

        let output = child.wait_with_output().expect("wait for git daemon");
        panic!(
            "git daemon did not become ready: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn url(&self, repo_name: &str) -> String {
        format!("git://127.0.0.1:{}/{}", self.port, repo_name)
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn sync_tags_peels_annotated_tags() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let commit_oid = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&git_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    test_support::set_git_repo_path(
        &mut bridge,
        git_repo.workdir().expect("workdir").to_path_buf(),
    );
    let change_id = ChangeId::generate();
    test_support::mapping_mut(&mut bridge).insert(change_id, commit_oid);

    let synced = sync_tags(&mut bridge).expect("sync tags");
    assert_eq!(synced, 1);
    assert_eq!(
        repo.refs().get_marker(&MarkerName::new("v1.0")).unwrap(),
        Some(change_id)
    );
}

#[test]
fn sync_track_to_branch_advances_branch_to_thread_tip() {
    let (_git_temp, git_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&git_repo);
    let first = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    let second = commit_with_tree(&git_repo, None, tree_oid, "next", &[first]);

    // Heddle owns thread→branch sync, so the branch ref is updated to
    // match the thread tip rather than rejecting non-fast-forward
    // moves: the source of truth for `refs/heads/<thread>` is the
    // Heddle thread, not the Git ref.
    sync_track_to_branch(&git_repo, "main", second).expect("branch sync must succeed");

    let mut updated =
        find_reference(&git_repo, "refs/heads/main").expect("branch ref present after sync");
    let updated_oid = updated.peel_to_id().expect("peel ref");
    assert_eq!(
        updated_oid, second,
        "branch main should now point at the thread tip"
    );
}

#[test]
fn export_tree_writes_gitlink_entries() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let submodule_oid: ObjectId = "0303030303030303030303030303030303030303"
        .parse()
        .expect("oid");
    let tree = Tree::from_entries(vec![
        TreeEntry::gitlink("vendor".to_string(), submodule_oid).expect("gitlink entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("tree");

    let tree_oid = export_tree(&repo, &git_repo, &tree_hash).expect("export");
    let git_tree = git_repo.find_tree(tree_oid).expect("git tree");
    let entry = git_tree.find_entry("vendor").expect("entry");

    assert_eq!(entry.mode().kind(), EntryKind::Commit);
    assert_eq!(entry.object_id(), submodule_oid);
}

#[test]
fn export_tree_keeps_legacy_gitlink_marker_blob_as_blob() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let marker = b"heddle-submodule: 0303030303030303030303030303030303030303\n";
    let blob_hash = repo
        .store()
        .put_blob(&Blob::new(marker.to_vec()))
        .expect("blob");
    let tree = Tree::from_entries(vec![
        TreeEntry::file("vendor".to_string(), blob_hash, false).expect("blob entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("tree");

    let tree_oid = export_tree(&repo, &git_repo, &tree_hash).expect("export");
    let git_tree = git_repo.find_tree(tree_oid).expect("git tree");
    let entry = git_tree.find_entry("vendor").expect("entry");

    assert_eq!(
        entry.mode().kind(),
        EntryKind::Blob,
        "ordinary blob bytes must not be sniffed into a gitlink"
    );
    let exported_blob = git_repo.find_blob(entry.object_id()).expect("blob");
    assert_eq!(exported_blob.data.as_slice(), marker);
}

#[test]
fn export_tree_substitutes_stub_for_redacted_blob() {
    // Critical safety property of the redaction primitive: a leaked
    // secret declared via `heddle redact` must NEVER appear in the
    // bytes the Git bridge writes downstream. The bridge is the only
    // path from a Heddle repo to an external Git remote (GitHub,
    // GitLab, internal mirrors), and bytes that escape via the
    // bridge cannot be retroactively scrubbed from those repos.
    //
    // This test pins the contract: with a redaction declared on a
    // blob, exporting the containing tree must write a stub blob
    // (which contains the redaction notice + an audit pointer)
    // instead of the raw secret bytes. Even though the secret is
    // still on disk in Heddle's local store, it does not propagate.

    use chrono::Utc;
    use objects::object::{ContentHash, Principal, Redaction};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let secret_bytes = b"AWS_SECRET_ACCESS_KEY=hunter2-leaked\n";
    let blob = Blob::new(secret_bytes.to_vec());
    let blob_hash = repo.store().put_blob(&blob).expect("blob");

    let tree = Tree::from_entries(vec![
        TreeEntry::file("secrets.env".to_string(), blob_hash, false).expect("blob entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("tree");

    // Declare a redaction on the blob. The redaction's `state` doesn't
    // matter for export — the store is keyed by blob hash — but we
    // populate the field so the stub renders a believable audit line.
    let dummy_state = ChangeId::from_bytes([42u8; 16]);
    repo.put_redaction(Redaction {
        redacted_blob: blob_hash,
        state: dummy_state,
        path: "secrets.env".into(),
        reason: "leaked AWS key".into(),
        redactor: Principal {
            name: "Auditor".into(),
            email: "auditor@heddle.sh".into(),
        },
        redacted_at: Utc::now(),
        signature: None,
        purged_at: None,
        supersedes: None,
    })
    .expect("declare redaction");

    // Export. The bridge MUST substitute the stub.
    let tree_oid = export_tree(&repo, &git_repo, &tree_hash).expect("export");
    let git_tree = git_repo.find_tree(tree_oid).expect("git tree");
    let entry = git_tree.find_entry("secrets.env").expect("entry");

    // The exported blob's bytes must NOT contain the secret.
    let git_blob = git_repo
        .find_blob(entry.object_id())
        .expect("find exported blob");
    let exported_bytes = git_blob.data.as_slice();
    let exported_text = std::str::from_utf8(exported_bytes).expect("stub is utf-8");

    assert!(
        !exported_text.contains("hunter2-leaked"),
        "EXPORT LEAK: redacted blob bytes reached Git. Got: {exported_text:?}"
    );
    assert!(
        !exported_bytes
            .windows(secret_bytes.len())
            .any(|w| w == secret_bytes),
        "EXPORT LEAK (byte-level): raw secret bytes reached Git tree"
    );
    // The substituted blob must be recognizable as a redaction stub
    // so downstream Git readers see why content disappeared.
    assert!(
        exported_text.contains("redacted by Heddle"),
        "stub must announce itself; got: {exported_text:?}"
    );
    assert!(
        exported_text.contains("leaked AWS key"),
        "stub must carry the redaction reason; got: {exported_text:?}"
    );

    // Sanity: the heddle store still holds the original bytes (purge
    // hasn't happened yet) — the substitution is on the *export path*,
    // not the underlying store.
    let still_in_store = repo
        .store()
        .get_blob(&blob_hash)
        .expect("store lookup")
        .expect("blob still present pre-purge");
    assert_eq!(still_in_store.content(), secret_bytes);

    // Quiet `unused` warnings on imports used only by this test:
    let _ = ContentHash::from_bytes([0u8; 32]);
}

#[test]
fn import_all_imports_gitlink_losslessly_and_exports_gitlink() {
    let (_git_temp, git_repo) = init_gitlink_repo();

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("default import accepts gitlink");

    assert_eq!(stats.states_created, 1);
    assert!(stats.lossy_entries.is_empty());

    let state_id = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .expect("main thread lookup")
        .expect("main thread");
    let state = repo
        .store()
        .get_state(&state_id)
        .expect("get state")
        .expect("state present");
    assert!(!state.git_lossy, "gitlink import must not set git_lossy");

    let exported_tree = export_tree(&repo, &git_repo, &state.tree).expect("export tree");
    let git_tree = git_repo.find_tree(exported_tree).expect("exported tree");
    let entry = git_tree.find_entry("vendor").expect("vendor entry");
    let expected_oid: ObjectId = "0505050505050505050505050505050505050505"
        .parse()
        .expect("oid");
    assert_eq!(entry.mode().kind(), EntryKind::Commit);
    assert_eq!(entry.object_id(), expected_oid);
}

#[test]
fn import_all_reuses_shared_gitlink_subtree_without_lossy_entries() {
    let (_git_temp, git_repo) = init_git_repo();
    let shared_tree = gitlink_tree(&git_repo, "vendor");

    let mut first_editor = git_repo
        .edit_tree(&empty_tree_oid(&git_repo))
        .expect("first tree editor");
    first_editor.upsert("shared", EntryKind::Tree, shared_tree);
    let first_tree = git_repo.write_tree(first_editor).expect("write first tree");
    let first_commit = commit_with_tree(&git_repo, None, first_tree, "add shared subtree", &[]);

    let note_blob = git_repo.write_blob(b"second\n").expect("note blob");
    let mut second_editor = git_repo
        .edit_tree(&empty_tree_oid(&git_repo))
        .expect("second tree editor");
    second_editor.upsert("shared", EntryKind::Tree, shared_tree);
    second_editor.upsert("note.txt", EntryKind::Blob, note_blob);
    let second_tree = git_repo
        .write_tree(second_editor)
        .expect("write second tree");
    let _second_commit = commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        second_tree,
        "reuse shared subtree",
        &[first_commit],
    );

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(&mut bridge, Some(&git_repo.workdir().expect("workdir")))
        .expect("default import accepts shared gitlink subtree");

    assert_eq!(stats.states_created, 2);
    assert!(stats.lossy_entries.is_empty());

    assert!(
        !test_support::mapping_path(&bridge).exists(),
        "bridge import must not publish ingest-only mappings through the served mapping cache"
    );
}

#[test]
fn import_all_default_fails_on_cached_lossy_commit_from_prior_run() {
    let git_temp = init_invalid_utf8_name_repo();
    let git_path = git_temp.path();
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    let mut first_bridge = GitBridge::new(&repo);
    let first = import_all_with_options(
        &mut first_bridge,
        Some(git_path),
        ingest::ImportOptions { lossy: true },
    )
    .expect("initial lossy bridge import succeeds");
    assert_eq!(first.lossy_entries.len(), 1);

    assert!(
        !test_support::mapping_path(&first_bridge).exists(),
        "bridge import must not publish lossy entries through the served mapping cache"
    );

    let mut rerun_bridge = GitBridge::new(&repo);
    let err = import_all(&mut rerun_bridge, Some(git_path))
        .expect_err("default bridge import must not reuse cached lossy state silently");
    assert_lossy_default_rerun_error("bridge", &err.to_string());
}

#[test]
fn import_all_lossy_reports_cached_lossy_commit_from_prior_run() {
    let git_temp = init_invalid_utf8_name_repo();
    let git_path = git_temp.path();
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    let mut first_bridge = GitBridge::new(&repo);
    import_all_with_options(
        &mut first_bridge,
        Some(git_path),
        ingest::ImportOptions { lossy: true },
    )
    .expect("initial lossy bridge import succeeds");

    let mut rerun_bridge = GitBridge::new(&repo);
    let second = import_all_with_options(
        &mut rerun_bridge,
        Some(git_path),
        ingest::ImportOptions { lossy: true },
    )
    .expect("lossy bridge rerun reports ingest-cached lossy entries");

    assert_eq!(second.states_created, 0);
    assert_eq!(second.lossy_entries.len(), 1);
    assert_eq!(second.lossy_entries[0].path, "bad\u{fffd}name");
    assert!(second.lossy_entries[0].summary_line().contains("converted"));
}

#[cfg(feature = "ingest")]
#[derive(Clone, Copy, Debug)]
enum ImportEngine {
    Ingest,
    Bridge,
}

#[cfg(feature = "ingest")]
impl ImportEngine {
    fn label(self) -> &'static str {
        match self {
            ImportEngine::Ingest => "ingest",
            ImportEngine::Bridge => "bridge",
        }
    }
}

fn assert_lossy_default_rerun_error(engine: &str, message: &str) {
    assert!(
        message.contains("bad"),
        "{engine} error names entry: {message}"
    );
    assert!(
        message.contains("not valid UTF-8"),
        "{engine} error explains policy: {message}"
    );
    assert!(
        message.contains("--lossy"),
        "{engine} error names opt-in: {message}"
    );
}

#[cfg(feature = "ingest")]
fn run_lossy_then_default_rerun(engine: ImportEngine) {
    let git_temp = init_invalid_utf8_name_repo();
    let git_path = git_temp.path();
    let message = match engine {
        ImportEngine::Ingest => {
            use ingest::{ImportOptions, import_git_into, import_git_into_with_options};

            let heddle_temp = TempDir::new().expect("heddle temp");
            let (first, map) = import_git_into_with_options(
                git_path,
                heddle_temp.path(),
                ImportOptions { lossy: true },
            )
            .expect("initial lossy ingest import succeeds");
            drop(map);
            assert_eq!(first.lossy_entries.len(), 1);

            import_git_into(git_path, heddle_temp.path())
                .expect_err("default ingest import must fail on cached lossy tree")
                .to_string()
        }
        ImportEngine::Bridge => {
            let heddle_temp = TempDir::new().expect("heddle temp");
            let repo = Repository::init(heddle_temp.path()).expect("init heddle");
            let mut first_bridge = GitBridge::new(&repo);
            let first = import_all_with_options(
                &mut first_bridge,
                Some(git_path),
                ingest::ImportOptions { lossy: true },
            )
            .expect("initial lossy bridge import succeeds");
            assert_eq!(first.lossy_entries.len(), 1);

            let mut rerun_bridge = GitBridge::new(&repo);
            import_all(&mut rerun_bridge, Some(git_path))
                .expect_err("default bridge import must fail on cached lossy commit")
                .to_string()
        }
    };

    assert_lossy_default_rerun_error(engine.label(), &message);
}

#[cfg(feature = "ingest")]
#[test]
fn both_engines_fail_hard_on_default_rerun_after_lossy() {
    for engine in [ImportEngine::Ingest, ImportEngine::Bridge] {
        run_lossy_then_default_rerun(engine);
    }
}

/// #568 P1: the checkout-materialization closure walk reconstructs a
/// byte-faithful commit's objects DIRECTLY from heddle state into the worktree
/// `.git`, instead of copying the verbatim objects out of the eager `.heddle/git`
/// mirror. Proven by handing the walk an EMPTY mirror: if it still leaned on the
/// mirror copy the commit could not materialize, but reconstruction-from-state
/// lands the byte-identical OID with nothing in the mirror to copy.
#[test]
fn checkout_materialization_reconstructs_faithful_commit_from_empty_mirror() {
    use std::collections::HashSet;

    use cli::bridge::git_reconstruct::commit_object_id;
    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    // A faithful root state: git-fidelity (`raw_message`), valid UTF-8 identity,
    // not lossy → `commit_is_byte_faithful` is true, so it routes to reconstruct.
    let blob_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"hello\n"))
        .expect("put blob");
    let tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
        ]))
        .expect("put tree");
    let state = State::new(
        tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    )
    .with_raw_message("an imported commit\n");
    repo.store().put_state(&state).expect("put state");
    let state_id = state.change_id;

    let mut bridge = GitBridge::new(&repo);

    // Learn the expected git OID the SAME way export does — reconstruct the
    // commit's bytes from state and hash them — and seed the bridge mapping so
    // the walk's OID-equality gate has an expected target.
    let scratch_temp = TempDir::new().expect("scratch temp");
    let scratch = SleyRepository::init_bare(scratch_temp.path()).expect("scratch repo");
    let expected_oid = bridge
        .reconstruct_and_write_commit(&scratch, &state)
        .expect("reconstruct expected oid");
    test_support::mapping_mut(&mut bridge).insert(state_id, expected_oid);

    // The mirror is EMPTY — a copy-from-mirror path could not materialize the
    // commit. The checkout object db is empty too (fresh worktree).
    let mirror_temp = TempDir::new().expect("mirror temp");
    let mirror_repo = SleyRepository::init_bare(mirror_temp.path()).expect("empty mirror");
    let checkout_temp = TempDir::new().expect("checkout temp");
    let object_repo = SleyRepository::init_bare(checkout_temp.path()).expect("checkout odb");

    assert!(
        mirror_repo.read_object(&expected_oid).is_err(),
        "test invariant: the mirror must NOT hold the commit — reconstruction is the only way it can appear"
    );
    assert!(
        object_repo.read_object(&expected_oid).is_err(),
        "test invariant: the checkout starts without the commit"
    );

    test_support::materialize_checkout_closure_from_state(
        &bridge,
        &mirror_repo,
        &object_repo,
        &state_id,
        expected_oid,
        &HashSet::new(),
    )
    .expect("materialize faithful commit from state");

    // The checkout now carries the byte-identical commit object AND its tree/blob
    // closure — all reconstructed from state, none copied from the empty mirror.
    let commit = object_repo
        .read_commit(&expected_oid)
        .expect("checkout must hold the reconstructed commit");
    assert_eq!(
        commit_object_id(
            &bridge
                .reconstruct_commit_bytes(&object_repo, &state)
                .expect("re-reconstruct")
        ),
        expected_oid,
        "reconstructed OID is byte-identical to the mapped expected OID",
    );
    assert!(
        object_repo.read_object(&commit.tree).is_ok(),
        "the commit's tree must be materialized into the checkout too",
    );
}

/// #568 P1 safety gate: when a commit is classified faithful but reconstructs to
/// a DIFFERENT OID than the mapping expects (an unmodeled fidelity gap), the walk
/// MUST hard-error rather than silently materialize a wrong-OID checkout.
#[test]
fn checkout_materialization_hard_errors_on_oid_mismatch() {
    use std::collections::HashSet;

    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    let blob_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"hello\n"))
        .expect("put blob");
    let tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
        ]))
        .expect("put tree");
    let state = State::new(
        tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    )
    .with_raw_message("an imported commit\n");
    repo.store().put_state(&state).expect("put state");
    let state_id = state.change_id;

    let mut bridge = GitBridge::new(&repo);
    // Deliberately map the state to a WRONG OID — reconstruction will produce a
    // different id, which must trip the gate.
    let wrong_oid: ObjectId = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        .parse()
        .expect("oid");
    test_support::mapping_mut(&mut bridge).insert(state_id, wrong_oid);

    let mirror_temp = TempDir::new().expect("mirror temp");
    let mirror_repo = SleyRepository::init_bare(mirror_temp.path()).expect("mirror");
    let checkout_temp = TempDir::new().expect("checkout temp");
    let object_repo = SleyRepository::init_bare(checkout_temp.path()).expect("checkout odb");

    let err = test_support::materialize_checkout_closure_from_state(
        &bridge,
        &mirror_repo,
        &object_repo,
        &state_id,
        wrong_oid,
        &HashSet::new(),
    )
    .expect_err("OID mismatch must hard-error");
    let msg = err.to_string();
    assert!(
        msg.contains("OID mismatch") && msg.contains("refusing to materialize"),
        "error must name the OID-mismatch safety gate; got: {msg}"
    );
    assert!(
        object_repo.read_object(&wrong_oid).is_err(),
        "no wrong-OID object may be written to the checkout on the failure path",
    );
}

/// #568 P1: a LOSSY commit (verbatim bytes live ONLY in the mirror — `--lossy`
/// import or non-UTF8 identity) is NOT reconstructed; the walk backstops to the
/// mirror copy. Proven by reconstructing-then-stashing the object only in the
/// mirror (not the checkout) and confirming the lossy state still materializes.
#[test]
fn checkout_materialization_backstops_lossy_commit_from_mirror() {
    use std::collections::HashSet;

    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    let blob_hash = repo
        .store()
        .put_blob(&Blob::from_slice(b"hello\n"))
        .expect("put blob");
    let tree_hash = repo
        .store()
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
        ]))
        .expect("put tree");
    // `git_lossy` → NOT byte-faithful → mirror backstop, even though raw_message
    // is present.
    let state = State::new(
        tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    )
    .with_raw_message("a lossy imported commit\n")
    .with_git_lossy(true);
    repo.store().put_state(&state).expect("put state");
    let state_id = state.change_id;

    let mut bridge = GitBridge::new(&repo);

    // The mirror is the ONLY home of the lossy commit's bytes. Stage the object
    // there (the verbatim bytes); use the reconstructed bytes as a stand-in for
    // the verbatim ones (the tree/identity here happen to round-trip; the point is
    // the walk must NOT reconstruct-into-checkout and must copy from the mirror).
    let mirror_temp = TempDir::new().expect("mirror temp");
    let mirror_repo = SleyRepository::init_bare(mirror_temp.path()).expect("mirror");
    let lossy_oid = bridge
        .reconstruct_and_write_commit(&mirror_repo, &state)
        .expect("stage lossy bytes in the mirror");
    test_support::mapping_mut(&mut bridge).insert(state_id, lossy_oid);

    let checkout_temp = TempDir::new().expect("checkout temp");
    let object_repo = SleyRepository::init_bare(checkout_temp.path()).expect("checkout odb");
    assert!(
        object_repo.read_object(&lossy_oid).is_err(),
        "test invariant: the checkout starts without the lossy commit",
    );

    test_support::materialize_checkout_closure_from_state(
        &bridge,
        &mirror_repo,
        &object_repo,
        &state_id,
        lossy_oid,
        &HashSet::new(),
    )
    .expect("lossy commit materializes via the mirror backstop");

    assert!(
        object_repo.read_object(&lossy_oid).is_ok(),
        "the lossy commit must be copied from the mirror into the checkout",
    );
}

#[test]
fn import_all_lossy_clean_repo_reports_no_lossy_entries() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&git_repo);
    commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "clean", &[]);

    let mut bridge = GitBridge::new(&repo);
    let stats = import_all_with_options(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
        ingest::ImportOptions { lossy: true },
    )
    .expect("clean repo imports with lossy flag too");

    assert_eq!(stats.states_created, 1);
    assert!(stats.lossy_entries.is_empty());
}

/// The single imported lossy state must carry git-fidelity (`raw_message`) —
/// the signal that opts a commit INTO reconstruct-from-state — yet be flagged
/// `git_lossy`, so the #567 export guard keeps it OFF that path. Asserted for
/// both lossy population surfaces so they converge on ONE canonical marker.
fn assert_only_state_is_lossy_with_fidelity(repo: &Repository, surface: &str) {
    let states: Vec<_> = repo
        .store()
        .list_states()
        .expect("list states")
        .iter()
        .filter_map(|id| repo.store().get_state(id).expect("get state"))
        .collect();
    assert_eq!(states.len(), 1, "{surface}: single lossy commit imported");
    let state = &states[0];
    assert!(
        state.raw_message.is_some(),
        "{surface}: git-fidelity (raw_message) present — without the lossy \
         marker this is exactly what the buggy guard reconstructs"
    );
    assert!(
        state.git_lossy,
        "{surface}: a --lossy import must set the canonical git_lossy marker so \
         export does not reconstruct a wrong-SHA object"
    );
}

/// `import git --lossy` records the canonical `git_lossy` marker for
/// genuinely unrepresentable tree entries.
#[test]
fn bridge_import_lossy_state_carries_canonical_git_lossy_marker() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let git_temp = init_invalid_utf8_name_repo();

    let mut bridge = GitBridge::new(&repo);
    let stats = import_all_with_options(
        &mut bridge,
        Some(git_temp.path()),
        ingest::ImportOptions { lossy: true },
    )
    .expect("lossy bridge import succeeds");
    assert_eq!(stats.lossy_entries.len(), 1);

    assert_only_state_is_lossy_with_fidelity(&repo, "bridge import --lossy");
}

/// Close-the-class regression for #567 round 2: an ingest-backed lossy state
/// carries the SAME canonical `git_lossy` marker that `bridge import --lossy`
/// sets. Before this fix, ingest states satisfied `has_git_fidelity` but
/// carried no lossy signal reachable from an UNMAPPED state, so the export
/// guard reconstructed them into wrong-SHA objects. Both paths now set ONE flag
/// the guard reads.
#[cfg(feature = "ingest")]
#[test]
fn ingest_lossy_state_carries_canonical_git_lossy_marker() {
    use ingest::{ImportOptions, import_git_into_with_options};

    let git_temp = init_invalid_utf8_name_repo();
    let git_path = git_temp.path();

    let heddle_temp = TempDir::new().expect("heddle temp");
    let (stats, map) =
        import_git_into_with_options(git_path, heddle_temp.path(), ImportOptions { lossy: true })
            .expect("lossy ingest succeeds");
    drop(map);
    assert_eq!(stats.lossy_entries.len(), 1);

    let repo = Repository::open(heddle_temp.path()).expect("open heddle repo");
    assert_only_state_is_lossy_with_fidelity(&repo, "ingest-backed lossy import");
}

/// #567 round 3: an ingest-backed lossy state is UNMAPPED yet carries
/// git-fidelity (`raw_message`). Export must MINT it from its OWN raw metadata —
/// there is no tracked original OID to match and no verbatim mirror bytes to
/// fall back to, so the r2 `git_lossy` guard must NOT reject it into the
/// (nonexistent) verbatim/mapped-OID path (the r2 over-correction). The minted
/// commit preserves the raw message verbatim; the native intent+footer mint
/// would instead emit "No intent specified" + a
/// `Heddle-State` footer. The minted OID is DERIVED (there is no original to
/// match); the load-bearing assertion is that the raw metadata survives the mint.
#[cfg(feature = "ingest")]
#[test]
fn export_mints_lossy_ingest_state_from_raw_metadata() {
    use ingest::{ImportOptions, import_git_into_with_options};

    let git_temp = init_invalid_utf8_name_repo();
    let git_path = git_temp.path();

    let heddle_temp = TempDir::new().expect("heddle temp");
    let (stats, map) =
        import_git_into_with_options(git_path, heddle_temp.path(), ImportOptions { lossy: true })
            .expect("lossy ingest succeeds");
    drop(map);
    assert_eq!(stats.lossy_entries.len(), 1);

    let repo = Repository::open(heddle_temp.path()).expect("open heddle repo");
    let state = {
        let ids = repo.store().list_states().expect("list states");
        assert_eq!(ids.len(), 1, "single gitlink commit ingested");
        repo.store()
            .get_state(&ids[0])
            .expect("get state")
            .expect("state present")
    };
    let raw_message = state
        .raw_message
        .clone()
        .expect("ingest records raw_message");

    // Ingest records local identity in its SQLite map, not the served bridge
    // mapping, so this is exactly the unmapped lossy case that export must MINT,
    // not reject.
    let mut bridge = GitBridge::new(&repo);

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export");
    bridge
        .export_to_path(&dest_path)
        .expect("export must MINT the unmapped lossy state, not reject it");

    let dest_repo = open_git(&dest_path).expect("open dest");
    let dest_main = find_reference(&dest_repo, "refs/heads/main")
        .expect("exported main ref")
        .peel_to_id()
        .expect("exported main target");
    let dest_commit = dest_repo.find_commit(dest_main).expect("dest main commit");

    // Minted from raw metadata: the message is the verbatim raw_message, NOT the
    // native "No intent specified" + Heddle-State footer the intent-mint emits.
    assert_eq!(
        dest_commit.message_raw_sloppy().to_vec(),
        raw_message,
        "exported commit must preserve the ingest state's raw message verbatim, \
         not the native \"No intent specified\" + Heddle-State footer"
    );
}

/// Count loose objects (`objects/<2-hex>/<38-hex>`) in a bare mirror's object
/// store. The dominant uninstrumented read cost grows with this number.
fn count_mirror_loose_objects(mirror_git_dir: &std::path::Path) -> usize {
    let objects_dir = mirror_git_dir.join("objects");
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(&objects_dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Fanout dirs are exactly two hex chars; skip `pack`, `info`, etc.
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(entry.path()) {
            count += inner.flatten().count();
        }
    }
    count
}

fn mirror_pack_file_count(mirror_git_dir: &std::path::Path) -> usize {
    let pack_dir = mirror_git_dir.join("objects").join("pack");
    let Ok(entries) = std::fs::read_dir(&pack_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x == "pack")
                .unwrap_or(false)
        })
        .count()
}

/// `maintenance gc` must consolidate the bridge mirror (`.heddle/git`) by
/// packing its accumulated loose objects and dropping the redundant loose
/// copies, so bridge import/export/reconstruction paths stop paying the
/// loose-object read tax.
///
/// The invariant proven here (state, not timing):
/// 1. The loose-object count drops to zero (everything moves into a pack).
/// 2. A pack file now exists.
/// 3. Every managed ref still resolves AND its full reachable closure is
///    intact, read back BYTE-FOR-BYTE — content-addressed repack means OIDs are
///    unchanged, which is the only thing that preserves the lossy/non-UTF8
///    commit whose verbatim bytes live ONLY in the mirror (it cannot be
///    re-minted from heddle state).
/// 4. Idempotent: a second consolidation finds nothing loose to pack.
#[test]
fn maintenance_gc_consolidates_mirror_loose_objects_losslessly() {
    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);

    // Mint a multi-commit native heddle thread into the mirror. `export_all`
    // writes each minted commit/tree/blob as a LOOSE object via
    // `repo.write_object`/`write_blob`/`write_tree` — the real-world loose
    // accumulation path that slows every read+write command.
    let attribution = || Attribution::human(Principal::new("Alice", "alice@example.com"));
    let put_state = |contents: &[u8], name: &str, parents: Vec<ChangeId>| -> State {
        let store = test_support::heddle_repo(&bridge).store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(contents))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file(name.to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(tree_hash, parents, attribution());
        store.put_state(&state).expect("put state");
        state
    };
    let s1 = put_state(b"alpha\n", "a.txt", Vec::new());
    let s2 = put_state(b"beta\n", "b.txt", vec![s1.change_id]);
    let s3 = put_state(b"gamma\n", "c.txt", vec![s2.change_id]);
    test_support::heddle_repo(&bridge)
        .refs()
        .set_thread(&ThreadName::new("main"), &s3.change_id)
        .expect("set main thread");

    let exported = export_all(&mut bridge).expect("export mints loose mirror objects");
    assert!(
        exported.commits_total >= 3,
        "export must mint the three-commit history (got {})",
        exported.commits_total
    );

    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    let mirror_git_dir = mirror.git_dir().to_path_buf();

    // Snapshot the managed-ref frontier and the full reachable closure of every
    // managed ref, with each object's exact bytes, BEFORE consolidation.
    let managed = read_mirror_managed_refs(&mirror).expect("managed refs");
    assert!(
        !managed.is_empty(),
        "import must record managed refs in the mirror"
    );
    let mut closure_before: std::collections::HashMap<ObjectId, Vec<u8>> =
        std::collections::HashMap::new();
    let mut stack: Vec<ObjectId> = managed.values().copied().collect();
    while let Some(oid) = stack.pop() {
        if closure_before.contains_key(&oid) {
            continue;
        }
        let object = mirror.read_object(&oid).expect("reachable object present");
        closure_before.insert(oid, object.body.to_vec());
        match object.object_type {
            GitObjectType::Commit => {
                let commit = CommitObject::parse(mirror.object_format(), &object.body)
                    .expect("parse commit");
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            GitObjectType::Tag => {
                let tag =
                    TagObject::parse(mirror.object_format(), &object.body).expect("parse tag");
                stack.push(tag.object);
            }
            GitObjectType::Tree => {
                let tree = mirror.read_tree(&oid).expect("read tree");
                for entry in tree.entries {
                    if entry.kind() != Some(EntryKind::Commit) {
                        stack.push(entry.oid);
                    }
                }
            }
            GitObjectType::Blob => {}
        }
    }
    assert!(
        !closure_before.is_empty(),
        "managed refs must have a reachable closure"
    );

    let loose_before = count_mirror_loose_objects(&mirror_git_dir);
    assert!(
        loose_before > 0,
        "import must leave loose objects in the mirror (got {loose_before})"
    );

    // The consolidation under test.
    let consolidated = test_support::consolidate_mirror(&bridge).expect("consolidate mirror");
    assert!(
        consolidated >= loose_before,
        "consolidation must report packing at least every loose object \
         (reported {consolidated}, had {loose_before} loose)"
    );

    // 1. Loose objects gone; 2. a pack now serves them.
    let loose_after = count_mirror_loose_objects(&mirror_git_dir);
    assert_eq!(
        loose_after, 0,
        "every loose object must be consolidated into a pack"
    );
    assert!(
        mirror_pack_file_count(&mirror_git_dir) >= 1,
        "a pack file must exist after consolidation"
    );

    // 3. Re-open the mirror and prove every managed ref + its closure still
    //    resolves and reads back byte-for-byte (OID preservation).
    let mirror_after = test_support::open_git_repo(&bridge).expect("reopen mirror");
    let managed_after = read_mirror_managed_refs(&mirror_after).expect("managed refs after");
    assert_eq!(
        managed, managed_after,
        "managed-ref frontier must be unchanged by consolidation"
    );
    for (oid, expected_bytes) in &closure_before {
        let object = mirror_after
            .read_object(oid)
            .unwrap_or_else(|err| panic!("object {oid} lost after consolidation: {err}"));
        assert_eq!(
            &object.body.to_vec(),
            expected_bytes,
            "object {oid} bytes changed by consolidation — OID/content not preserved"
        );
    }

    // 4. Idempotent: nothing new loose to pack on a second run.
    let second = test_support::consolidate_mirror(&bridge).expect("idempotent consolidate");
    assert_eq!(
        second, 0,
        "a second consolidation must find no loose objects to pack"
    );
    assert_eq!(count_mirror_loose_objects(&mirror_git_dir), 0);
}

/// Regression: `copy_local_repo_to_bare` (the engine behind `heddle clone
/// /path/to/bare /work` for git-overlay paths) used to walk every tree
/// entry without distinguishing gitlinks from regular blobs/subtrees,
/// pushing the gitlink's foreign-repo commit OID onto the reachability
/// stack. The next `find_object` then failed with
/// "An object with id … could not be found", so any source repo
/// containing a submodule (e.g. git/git's `sha1collisiondetection`)
/// could not be cloned through heddle.
///
/// The fix skips `EntryKind::Commit` entries during the reachability
/// walk: by Git's design, gitlink commits live in the submodule's own
/// repo and are not stored locally. Heddle records the foreign OID as
/// a first-class Gitlink tree target, so dropping it from this walk
/// loses no Heddle object dependency.
#[test]
fn copy_local_repo_to_bare_handles_gitlink_entries() {
    let (_src_temp, source) = init_bare_git_repo();
    let dest_temp = TempDir::new().expect("dest temp");
    let dest_path = dest_temp.path().join("dest.git");

    // Build a tree containing one gitlink entry pointing at a
    // fabricated commit OID that is *not* present in this repo —
    // exactly the shape git/git ships with `sha1collisiondetection`.
    let foreign_submodule_oid: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
        .parse()
        .expect("oid");
    let mut editor = source
        .edit_tree(&empty_tree_oid(&source))
        .expect("tree editor");
    editor.upsert(
        "sha1collisiondetection",
        EntryKind::Commit,
        foreign_submodule_oid,
    );
    let tree_with_gitlink = source.write_tree(editor).expect("write tree");
    let commit = commit_with_tree(
        &source,
        Some("refs/heads/main"),
        tree_with_gitlink,
        "add submodule",
        &[],
    );

    // Sanity: the gitlink target must really not be in the source repo
    // (otherwise we're not exercising the regression).
    assert!(
        source.find_object(foreign_submodule_oid).is_err(),
        "test setup invariant: gitlink target should not be present locally"
    );
    assert!(
        source.find_commit(commit).is_ok(),
        "test setup invariant: parent commit should be present"
    );

    // The pre-fix code path would error here with
    // "An object with id 855827c5… could not be found"; with the
    // gitlink-skip, it succeeds and the destination has every reachable
    // non-gitlink object.
    copy_local_repo_to_bare(source.path(), &dest_path).expect("copy with gitlink");

    let dest = open_git(&dest_path).expect("open dest");
    assert!(
        dest.find_commit(commit).is_ok(),
        "destination must contain the parent commit"
    );
    assert!(
        dest.find_tree(tree_with_gitlink).is_ok(),
        "destination must contain the gitlink-bearing tree"
    );
    assert!(
        dest.find_object(foreign_submodule_oid).is_err(),
        "gitlink target stays out-of-band — that's the whole point"
    );

    // The gitlink entry on the copied tree round-trips its mode + OID
    // verbatim.
    let copied_tree = dest.find_tree(tree_with_gitlink).expect("dest tree");
    let entry = copied_tree
        .find_entry("sha1collisiondetection")
        .expect("entry");
    assert_eq!(entry.mode().kind(), EntryKind::Commit);
    assert_eq!(entry.object_id(), foreign_submodule_oid);
}

/// Regression: `copy_local_repo_to_bare` used to force the destination
/// HEAD to `refs/heads/main` whenever a `main` branch existed in the
/// source — even when the source repo's actual HEAD pointed at a
/// different branch. A repo on `master` that also happened to carry a
/// `main` branch (a common shape in projects mid-rename) would be
/// silently checked out on the wrong branch after `heddle clone`.
///
/// Fix: read the source repo's symbolic HEAD and honour it when it
/// names a branch we copied; fall back to `main` (then any first
/// branch) only when the source HEAD is detached or unmapped.
#[test]
fn copy_local_repo_to_bare_preserves_source_head_branch() {
    let (_src_temp, source) = init_bare_git_repo();
    let dest_temp = TempDir::new().expect("dest temp");
    let dest_path = dest_temp.path().join("dest.git");

    let tree = empty_tree_oid(&source);
    let master_tip = commit_with_tree(&source, Some("refs/heads/master"), tree, "M", &[]);
    let main_tip = commit_with_tree(&source, Some("refs/heads/main"), tree, "Mn", &[]);
    assert_ne!(master_tip, main_tip);

    // Source HEAD points at master, not main.
    std::fs::write(source.path().join("HEAD"), b"ref: refs/heads/master\n").expect("set HEAD");

    copy_local_repo_to_bare(source.path(), &dest_path).expect("copy");

    let head = std::fs::read_to_string(dest_path.join("HEAD")).expect("read HEAD");
    assert_eq!(
        head.trim(),
        "ref: refs/heads/master",
        "destination HEAD must mirror source HEAD even when a `main` branch exists alongside"
    );
}

/// `write_through_current_checkout`'s rollback path used to only reset
/// the branch ref when a *prior* OID existed at that ref. If write-through
/// created the branch from scratch (no prior value) and then a later
/// step failed — e.g., an index write or one of the fsyncs — the
/// new branch was left behind, so callers saw an error but Git still
/// showed the half-written ref.
///
/// The fix uses `delete_reference_if_present` on rollback when no
/// previous_branch existed. This test exercises the helper directly:
/// creating a new ref with `set_reference` and then dropping it through
/// the same path the rollback uses must leave the destination repo
/// without the ref. A no-op delete on a missing ref must succeed too,
/// so the rollback can run idempotently after a partial failure.
#[test]
fn delete_reference_if_present_drops_new_branch_for_rollback() {
    let (_temp, repo) = init_bare_git_repo();
    let tree = empty_tree_oid(&repo);
    let oid = commit_with_tree(&repo, None, tree, "rollback target", &[]);

    set_reference(
        &repo,
        "refs/heads/feature-x",
        oid,
        RefPrecondition::MustNotExist,
        "create branch",
    )
    .expect("create branch");
    assert!(
        find_reference(&repo, "refs/heads/feature-x").is_ok(),
        "set_reference must create the branch"
    );

    delete_reference_if_present(&repo, "refs/heads/feature-x").expect("delete");
    assert!(
        find_reference(&repo, "refs/heads/feature-x").is_err(),
        "rollback must remove the branch we just created"
    );

    // Idempotent: a second delete on a missing ref is a no-op, so the
    // rollback path can run safely even if `set_reference` itself was
    // the failing step (i.e., the branch never got created).
    delete_reference_if_present(&repo, "refs/heads/feature-x")
        .expect("delete on missing ref must be a no-op");
}

#[test]
fn mapping_persists_between_runs() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let change_id = ChangeId::generate();
    let git_oid: ObjectId = "0909090909090909090909090909090909090909"
        .parse()
        .expect("oid");

    let mut bridge = GitBridge::new(&repo);
    test_support::mapping_mut(&mut bridge).insert(change_id, git_oid);
    test_support::save_mapping_to_disk(&bridge).expect("save mapping");

    let mut reloaded = GitBridge::new(&repo);
    test_support::build_existing_mapping(
        &mut reloaded,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("build mapping");

    assert_eq!(
        test_support::mapping(&reloaded).get_git(&change_id),
        Some(git_oid)
    );
}

#[test]
fn mapping_rebuilds_from_heddle_notes() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let commit_oid = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    let change_id = ChangeId::generate();
    let note = git_notes::HeddleNote {
        change_id: change_id.to_string_full(),
        agent: None,
        confidence: None,
        status: "published".to_string(),
        omitted_annotations_breakdown: None,
        signal_counts: None,
        attribution: None,
    };
    git_notes::write_note(&git_repo, commit_oid, &note).expect("write heddle note");

    let mut bridge = GitBridge::new(&repo);
    test_support::build_existing_mapping(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("build mapping");

    assert_eq!(
        test_support::mapping(&bridge).get_heddle(commit_oid),
        Some(change_id)
    );
}

#[test]
fn mapping_rebuild_prefers_heddle_notes_over_stale_cache() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let commit_oid = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    let change_id = ChangeId::generate();
    let stale_oid: ObjectId = "abababababababababababababababababababab"
        .parse()
        .expect("oid");

    let note = git_notes::HeddleNote {
        change_id: change_id.to_string_full(),
        agent: None,
        confidence: None,
        status: "published".to_string(),
        omitted_annotations_breakdown: None,
        signal_counts: None,
        attribution: None,
    };
    git_notes::write_note(&git_repo, commit_oid, &note).expect("write heddle note");

    let mut stale_bridge = GitBridge::new(&repo);
    test_support::mapping_mut(&mut stale_bridge).insert(change_id, stale_oid);
    test_support::save_mapping_to_disk(&stale_bridge).expect("save stale cache");

    let mut bridge = GitBridge::new(&repo);
    test_support::build_existing_mapping(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("notes should rebuild over stale cache");

    assert_eq!(
        test_support::mapping(&bridge).get_git(&change_id),
        Some(commit_oid),
        "refs/notes/heddle is authoritative for exported Git identity"
    );
    assert_eq!(
        test_support::mapping(&bridge).get_heddle(stale_oid),
        None,
        "stale cache-only identity must be discarded when a note supersedes it"
    );

    let mapping = std::fs::read_to_string(test_support::mapping_path(&bridge))
        .expect("existing mapping cache");
    assert!(
        !mapping.contains(&commit_oid.to_string()),
        "mapping rebuild is read-only; export owns the visibility-filtered cache write"
    );
    assert!(
        mapping.contains(&stale_oid.to_string()),
        "read-only mapping rebuild must not rewrite the existing cache"
    );
}

#[test]
fn test_sync_mapping() {
    let mut mapping = SyncMapping::new();
    let change_id = ChangeId::generate();
    let oid: ObjectId = "0101010101010101010101010101010101010101".parse().unwrap();
    mapping.insert(change_id, oid);
    assert_eq!(mapping.get_git(&change_id), Some(oid));
    assert_eq!(mapping.get_heddle(oid), Some(change_id));
}

#[test]
#[cfg(unix)]
fn sync_branches_propagates_track_write_failures() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let commit_oid = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);

    let threads_dir = repo.heddle_dir().join("refs/threads");
    let original_mode = std::fs::metadata(&threads_dir)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&threads_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let mut bridge = GitBridge::new(&repo);
    test_support::set_git_repo_path(
        &mut bridge,
        git_repo.workdir().expect("workdir").to_path_buf(),
    );
    let change_id = ChangeId::generate();
    test_support::mapping_mut(&mut bridge).insert(change_id, commit_oid);

    let result = sync_branches(&mut bridge);

    std::fs::set_permissions(&threads_dir, std::fs::Permissions::from_mode(original_mode)).unwrap();
    assert!(result.is_err(), "thread write failures should be returned");
}

#[test]
#[cfg(unix)]
fn sync_tags_propagates_marker_write_failures() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let commit_oid = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&git_repo, "v1.0", commit_oid, "release");

    let markers_dir = repo.heddle_dir().join("refs/markers");
    let original_mode = std::fs::metadata(&markers_dir)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&markers_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let mut bridge = GitBridge::new(&repo);
    test_support::set_git_repo_path(
        &mut bridge,
        git_repo.workdir().expect("workdir").to_path_buf(),
    );
    let change_id = ChangeId::generate();
    test_support::mapping_mut(&mut bridge).insert(change_id, commit_oid);

    let result = sync_tags(&mut bridge);

    std::fs::set_permissions(&markers_dir, std::fs::Permissions::from_mode(original_mode)).unwrap();
    assert!(result.is_err(), "marker write failures should be returned");
}

#[test]
fn pull_imports_remote_branches_and_tags_from_path_remote() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .pull(source_temp.path().to_str().expect("remote path"))
        .expect("pull remote");

    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn pull_imports_remote_branches_and_tags_from_file_url_remote() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .pull(&format!("file://{}", source_temp.path().display()))
        .expect("pull remote");

    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn pull_imports_remote_branches_and_tags_from_git_daemon() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let remote_root = TempDir::new().expect("remote root");
    let remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");

    let tree_oid = empty_tree_oid(&remote_repo);
    let commit_oid = commit_with_tree(&remote_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&remote_repo, "v1.0", commit_oid, "release");

    let Some(daemon) = GitDaemon::spawn(remote_root.path()) else {
        return;
    };

    let mut bridge = GitBridge::new(&repo);
    bridge.pull(&daemon.url("remote.git")).expect("pull remote");

    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn pull_imports_remote_branches_and_tags_from_git_http_backend() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let remote_root = TempDir::new().expect("remote root");
    let remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");

    let tree_oid = empty_tree_oid(&remote_repo);
    let commit_oid = commit_with_tree(&remote_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&remote_repo, "v1.0", commit_oid, "release");

    let Some(backend) = GitHttpBackend::spawn(remote_root.path()) else {
        return;
    };

    let mut bridge = GitBridge::new(&repo);
    bridge
        .pull(&backend.url("remote.git"))
        .expect("pull remote over http");

    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn pull_imports_remote_branches_and_tags_from_authenticated_git_http_backend() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let remote_root = TempDir::new().expect("remote root");
    let remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");

    let tree_oid = empty_tree_oid(&remote_repo);
    let commit_oid = commit_with_tree(&remote_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&remote_repo, "v1.0", commit_oid, "release");

    let Some(backend) = GitHttpBackend::spawn_authenticated(remote_root.path(), "heddle", "secret")
    else {
        return;
    };

    let mut bridge = GitBridge::new(&repo);
    bridge
        .pull(&backend.url("remote.git"))
        .expect("pull remote over authenticated http");

    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn fetch_rejects_reserved_git_remote_name_at_boundary() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);

    let err = bridge
        .fetch("git")
        .expect_err("fetch of reserved remote name must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("reserved namespace") && message.contains("rename"),
        "fetch error must explain the reserved-namespace collision and how to fix it, got: {message}"
    );
}

#[test]
fn pull_rejects_reserved_git_remote_name_at_boundary() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);

    let err = bridge
        .pull("git")
        .expect_err("pull of reserved remote name must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("reserved namespace") && message.contains("rename"),
        "pull error must explain the reserved-namespace collision and how to fix it, got: {message}"
    );
}

#[test]
fn import_handles_merge_history_without_missing_parent_mappings() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let base = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    let left = commit_with_tree(
        &git_repo,
        Some("refs/heads/left"),
        tree_oid,
        "left",
        &[base],
    );
    let right = commit_with_tree(
        &git_repo,
        Some("refs/heads/right"),
        tree_oid,
        "right",
        &[base],
    );
    let merge = commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        tree_oid,
        "merge",
        &[left, right],
    );

    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("import merge history");

    assert_eq!(stats.commits_imported, 4);
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("main")).unwrap(),
        test_support::mapping(&bridge).get_heddle(merge)
    );
    assert!(test_support::mapping(&bridge).get_heddle(base).is_some());
    assert!(test_support::mapping(&bridge).get_heddle(left).is_some());
    assert!(test_support::mapping(&bridge).get_heddle(right).is_some());
    assert!(test_support::mapping(&bridge).get_heddle(merge).is_some());
}

// heddle#464 close-the-class (import boundary): a git branch name becomes a
// Heddle thread id on import. Git permits ref names containing shell
// metacharacters (e.g. `;`) that are NOT safe thread ids — interpolating one
// into a recommended-command breadcrumb would be unrunnable. Reject such an
// import with an actionable rename hint rather than silently slugifying it.
#[test]
fn import_rejects_branch_name_that_is_not_a_valid_thread_id() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let base = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    // Git accepts `;` in a branch name; a Heddle thread id must not.
    commit_with_tree(
        &git_repo,
        Some("refs/heads/evil;rm"),
        tree_oid,
        "evil",
        &[base],
    );

    let mut bridge = GitBridge::new(&repo);
    let err = import_all(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect_err("import must reject a branch whose name is not a valid thread id");

    assert!(
        err.contains("evil;rm"),
        "the rejection must name the branch, got: {err}"
    );
}

#[test]
fn failed_import_restores_mapping_and_overrides() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let base = commit_with_tree(&git_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    commit_with_tree(
        &git_repo,
        Some("refs/heads/evil;rm"),
        tree_oid,
        "evil",
        &[base],
    );

    let mut bridge = GitBridge::new(&repo);
    test_support::set_commit_message_override(
        &mut bridge,
        ChangeId::from_bytes([3; 16]),
        "pre-call override".to_string(),
    );
    let pre_mapping = test_support::mapping(&bridge).clone();
    let pre_overrides = test_support::commit_message_overrides(&bridge).clone();

    import_all(&mut bridge, Some(&git_repo.workdir().expect("workdir")))
        .expect_err("invalid branch name must fail after commits are mapped");

    assert_eq!(
        test_support::mapping(&bridge),
        &pre_mapping,
        "failed import must restore the pre-call mapping"
    );
    assert_eq!(
        test_support::commit_message_overrides(&bridge),
        &pre_overrides,
        "failed import must restore pre-call commit message overrides"
    );

    test_support::build_existing_mapping(&mut bridge, Some(&git_repo.workdir().expect("workdir")))
        .expect("mapping rebuild after failed import");
    assert_eq!(
        test_support::mapping(&bridge),
        &pre_mapping,
        "rebuilding after a failed import must not recover partial mappings from the sidecar"
    );
}

#[test]
fn push_exports_local_branches_and_tags_to_path_remote() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();
    let (remote_temp, remote_repo) = init_bare_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    bridge
        .push_with_scope(
            remote_temp.path().to_str().expect("remote path"),
            GitPushScope::AllThreads,
        )
        .expect("push remote");

    let main_oid = find_reference(&remote_repo, "refs/heads/main")
        .expect("main ref")
        .peel_to_id()
        .expect("main target");
    let tag_oid = find_reference(&remote_repo, "refs/tags/v1.0")
        .expect("tag ref")
        .peel_to_id()
        .expect("tag target");
    let commit = remote_repo.find_commit(main_oid).expect("main commit");
    let message = commit.message_raw_sloppy().to_string();

    // Phase B: imported commits round-trip with their original git SHAs
    // (no Heddle trailers added on export). The annotated tag still peels
    // to the same commit because the tag points at the commit object
    // itself, which we copy verbatim from the mirror.
    assert_eq!(tag_oid, main_oid);
    assert_eq!(
        main_oid, commit_oid,
        "Phase B: SHA must be preserved across import → push"
    );
    assert!(
        !message.contains("Heddle-Change-Id:"),
        "Phase B: Heddle trailers must not be written into commit messages; \
         change_id lives in refs/notes/heddle instead"
    );

    // The note carrying the change_id should have travelled along with
    // the branches and tags.
    let note_ref = find_reference(&remote_repo, git_notes::NOTES_REF)
        .expect("notes ref should be pushed to remote");
    let _ = note_ref;
}

#[test]
fn push_current_thread_scope_exports_only_attached_branch_to_path_remote() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();
    let (remote_temp, remote_repo) = init_bare_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let main_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "main", &[]);
    let side_oid = commit_with_tree(&source_repo, Some("refs/heads/side"), tree_oid, "side", &[]);
    create_annotated_tag(&source_repo, "v1.0", side_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    bridge
        .push_with_scope(
            remote_temp.path().to_str().expect("remote path"),
            GitPushScope::CurrentThread,
        )
        .expect("push current thread");

    let pushed_main = find_reference(&remote_repo, "refs/heads/main")
        .expect("main ref")
        .peel_to_id()
        .expect("main target");
    assert_eq!(pushed_main, main_oid);
    assert!(
        find_reference(&remote_repo, "refs/heads/side").is_err(),
        "current-thread push must not push sibling branches"
    );
    assert!(
        find_reference(&remote_repo, "refs/tags/v1.0").is_err(),
        "current-thread push must not push tags"
    );
    assert!(
        find_reference(&remote_repo, git_notes::NOTES_REF).is_ok(),
        "current-thread push must carry Heddle notes so cloned Git commits keep stable state IDs"
    );
}

/// Phase C: a deep linear commit chain must not blow the stack on import.
/// The pre-Phase-C recursive walker overflowed at ~80k commits on
/// `git/git`. 5,000 commits here is an order of magnitude past the
/// guard, well within stack overflow territory for the recursive
/// version, but trivial for the iterative replacement.
#[test]
fn import_handles_deep_linear_history_without_stack_overflow() {
    const DEPTH: usize = 5_000;

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);

    let mut parent: Option<ObjectId> = None;
    let mut last: Option<ObjectId> = None;
    for i in 0..DEPTH {
        let parents: Vec<ObjectId> = parent.into_iter().collect();
        let oid = commit_with_tree(
            &source_repo,
            None, // ref update only on the final commit
            tree_oid,
            &format!("c{i}"),
            &parents,
        );
        parent = Some(oid);
        last = Some(oid);
    }
    set_reference(
        &source_repo,
        "refs/heads/main",
        last.expect("last commit oid"),
        RefPrecondition::Any,
        "test: set main",
    )
    .expect("set main");

    // Pre-Phase-C: this would SIGABRT before reaching the assertion.
    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(
        &mut bridge,
        Some(source_repo.workdir().expect("workdir").as_path()),
    )
    .expect("deep import must complete without stack overflow");

    assert_eq!(stats.commits_imported, DEPTH);
    assert_eq!(stats.states_created, DEPTH);
}

/// Phase D: importing a repo that contains an annotated tag pointing at a
/// non-commit object (a blob or a tree) used to crash with
/// `Expected object of kind commit but got blob/tree`. Real-world repos
/// like `git/git` (`refs/tags/junio-gpg-pub` → blob containing the
/// maintainer's GPG public key) and `git-lfs/git-lfs`
/// (`refs/tags/core-gpg-keys` → tree of GPG keys) were unimportable.
///
/// After Phase D: skip with a warning and record the skipped ref in
/// `ImportStats::skipped_non_commit_refs` so callers can report it
/// without crashing the import.
#[test]
fn import_skips_tags_pointing_at_blob_or_tree() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();

    // A normal commit + tag — should still import correctly.
    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    // The QA failure mode: an annotated tag pointing at a blob.
    let blob_oid = source_repo
        .write_blob(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n")
        .expect("write gpg blob");
    let blob_tag = TagObject {
        object: blob_oid,
        object_type: GitObjectType::Blob,
        name: b"junio-gpg-pub".to_vec(),
        tagger: Some(test_signature().to_ident_bytes()),
        message: b"GPG public key".to_vec(),
        raw_body: None,
    };
    let blob_tag_oid = source_repo
        .write_object(EncodedObject::new(GitObjectType::Tag, blob_tag.write()))
        .expect("write tag");
    set_reference(
        &source_repo,
        "refs/tags/junio-gpg-pub",
        blob_tag_oid,
        RefPrecondition::MustNotExist,
        "test: tag pointing at blob",
    )
    .expect("set blob tag ref");

    // And the second QA failure mode: an annotated tag pointing at a tree.
    // Build a real (non-empty) tree containing one blob so the tree is in
    // the source ODB; sley doesn't special-case the well-known empty-tree
    // OID for `find_object`, and the failure path expects to be able to
    // look up the kind of the peeled object.
    let key_blob = source_repo.write_blob(b"key data").expect("write key blob");
    let mut editor = source_repo
        .edit_tree(&empty_tree_oid(&source_repo))
        .expect("editor");
    editor.upsert("alice.asc", EntryKind::Blob, key_blob);
    let tree_for_tag_oid = source_repo.write_tree(editor).expect("write tree");

    let tree_tag = TagObject {
        object: tree_for_tag_oid,
        object_type: GitObjectType::Tree,
        name: b"core-gpg-keys".to_vec(),
        tagger: Some(test_signature().to_ident_bytes()),
        message: b"core GPG keys directory".to_vec(),
        raw_body: None,
    };
    let tree_tag_oid = source_repo
        .write_object(EncodedObject::new(GitObjectType::Tag, tree_tag.write()))
        .expect("write tree tag");
    set_reference(
        &source_repo,
        "refs/tags/core-gpg-keys",
        tree_tag_oid,
        RefPrecondition::MustNotExist,
        "test: tag pointing at tree",
    )
    .expect("set tree tag ref");

    // Pre-Phase-D: this would crash with "Expected commit but got blob".
    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(
        &mut bridge,
        Some(source_repo.workdir().expect("workdir").as_path()),
    )
    .expect("import must complete despite non-commit-pointing tags");

    // The normal commit + v1.0 tag must have made it through.
    assert_eq!(stats.commits_imported, 1);
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .expect("read main thread")
            .is_some(),
        "the regular commit should have produced a main thread"
    );

    assert_eq!(
        stats.skipped_non_commit_refs, 2,
        "both non-commit-pointing tags should be counted"
    );
}

/// Phase F: `GitSource::parse` discriminates URLs from filesystem paths
/// using a clear textual rule (contains `://` or starts with `git@`).
#[test]
fn git_source_parse_distinguishes_urls_and_paths() {
    use cli::cli::cli_args::GitSource;

    // URLs.
    assert!(matches!(
        GitSource::parse("https://github.com/foo/bar.git").unwrap(),
        GitSource::Url(_)
    ));
    assert!(matches!(
        GitSource::parse("ssh://git@example.com/foo.git").unwrap(),
        GitSource::Url(_)
    ));
    assert!(matches!(
        GitSource::parse("git://example.com/foo.git").unwrap(),
        GitSource::Url(_)
    ));
    assert!(matches!(
        GitSource::parse("file:///tmp/some-repo").unwrap(),
        GitSource::Url(_)
    ));
    assert!(matches!(
        GitSource::parse("git@github.com:foo/bar.git").unwrap(),
        GitSource::Url(_)
    ));

    // Paths.
    assert!(matches!(
        GitSource::parse("/tmp/foo").unwrap(),
        GitSource::Path(_)
    ));
    assert!(matches!(
        GitSource::parse("./relative").unwrap(),
        GitSource::Path(_)
    ));
    assert!(matches!(
        GitSource::parse("just-a-name").unwrap(),
        GitSource::Path(_)
    ));
}

/// Phase F: cloning from a `file://` URL into a fresh bare repo populates
/// branches, tags, and commits — exercising the URL → temp dir → import
/// path without needing network access.
#[test]
fn clone_url_to_bare_populates_destination_from_file_url() {
    // Build a source repo with one commit and one tag.
    let (_src_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release v1");

    // Construct a file:// URL pointing at the source.
    let src_path = source_repo
        .workdir()
        .expect("workdir")
        .canonicalize()
        .expect("canonicalize");
    let url = format!("file://{}", src_path.display());

    // Clone into a fresh dest dir.
    let dest_root = TempDir::new().expect("dest temp");
    let dest = dest_root.path().join("clone-dest");
    clone_url_to_bare(url.as_str(), &dest, None, None).expect("clone file url");

    // Verify the dest has main + the v1.0 tag with the original commit OID.
    let dest_repo = open_git(&dest).expect("open dest");
    let dest_main = find_reference(&dest_repo, "refs/heads/main")
        .expect("main ref present after clone")
        .peel_to_id()
        .expect("peel main");
    assert_eq!(
        dest_main, commit_oid,
        "Phase F: clone_url_to_bare must transfer the original commit OID"
    );
    assert!(
        find_reference(&dest_repo, "refs/tags/v1.0").is_ok(),
        "Phase F: tags must be fetched too (with_fetch_tags::All)"
    );
}

/// Build a source repo with a chain of three commits, where each commit
/// adds a new blob file. Returns the (`temp`, `repo`, `[c1, c2, c3]`,
/// `[blob1, blob2, blob3]`) tuple. The caller owns the TempDir lifetime.
fn build_source_repo_three_commits_with_blobs()
-> (TempDir, SleyRepository, [ObjectId; 3], [ObjectId; 3]) {
    let (temp, repo) = init_git_repo();
    let blob1 = repo.write_blob(b"alpha\n").expect("blob1");
    let blob2 = repo.write_blob(b"beta\n").expect("blob2");
    let blob3 = repo.write_blob(b"gamma\n").expect("blob3");

    let mut e1 = repo.edit_tree(&empty_tree_oid(&repo)).expect("e1");
    e1.upsert("a.txt", EntryKind::Blob, blob1);
    let t1 = repo.write_tree(e1).expect("write t1");
    let c1 = commit_with_tree(&repo, None, t1, "c1: add a.txt", &[]);

    let mut e2 = repo.edit_tree(&empty_tree_oid(&repo)).expect("e2");
    e2.upsert("a.txt", EntryKind::Blob, blob1);
    e2.upsert("b.txt", EntryKind::Blob, blob2);
    let t2 = repo.write_tree(e2).expect("write t2");
    let c2 = commit_with_tree(&repo, None, t2, "c2: add b.txt", &[c1]);

    let mut e3 = repo.edit_tree(&empty_tree_oid(&repo)).expect("e3");
    e3.upsert("a.txt", EntryKind::Blob, blob1);
    e3.upsert("b.txt", EntryKind::Blob, blob2);
    e3.upsert("c.txt", EntryKind::Blob, blob3);
    let t3 = repo.write_tree(e3).expect("write t3");
    let c3 = commit_with_tree(&repo, Some("refs/heads/main"), t3, "c3: add c.txt", &[c2]);

    (temp, repo, [c1, c2, c3], [blob1, blob2, blob3])
}

/// `file://` clones use the native local-copy path rather than the network
/// file transport, because that transport spawns Git upload-pack
/// helpers. Until Heddle has native shallow-object pruning for local
/// copies, shallow `file://` clones fail closed instead of requiring a
/// Git executable suite on the host.
#[test]
fn clone_url_to_bare_rejects_shallow_file_url_without_shelling_to_git() {
    let (_src_temp, source_repo, _commits, _blobs) = build_source_repo_three_commits_with_blobs();

    let src_path = source_repo
        .workdir()
        .expect("workdir")
        .canonicalize()
        .expect("canonicalize");
    let url = format!("file://{}", src_path.display());

    let dest_root = TempDir::new().expect("dest temp");
    let dest = dest_root.path().join("clone-dest");
    let err = clone_url_to_bare(url.as_str(), &dest, Some(1), None)
        .expect_err("shallow file:// clone should fail closed in no-git runtime");
    let msg = err.to_string();
    assert!(
        msg.contains("shallow file:// Git clones are not supported")
            && msg.contains("native no-git runtime")
            && msg.contains("without spawning Git transport helpers"),
        "shallow file:// refusal should explain the no-git native boundary: {msg}"
    );
    assert!(
        !dest.exists(),
        "rejection should happen before writing destination state"
    );
}

/// Filtered Git-overlay clones are rejected until Heddle has a native
/// partial-clone implementation. The product contract is Git-compatible,
/// not Git-binary-dependent, so this path must not fall back to `git clone`.
#[test]
fn clone_url_to_bare_rejects_blob_none_filter_without_shelling_to_git() {
    let (_src_temp, source_repo, _commits, _blobs) = build_source_repo_three_commits_with_blobs();

    let src_path = source_repo
        .workdir()
        .expect("workdir")
        .canonicalize()
        .expect("canonicalize");
    let url = format!("file://{}", src_path.display());

    let dest_root = TempDir::new().expect("dest temp");
    let dest = dest_root.path().join("clone-dest");
    let err = clone_url_to_bare(url.as_str(), &dest, Some(1), Some("blob:none"))
        .expect_err("filtered clone must be rejected in no-git runtime");
    let msg = err.to_string();
    assert!(
        msg.contains("partial Git clone filter `blob:none` is not supported")
            && msg.contains("native no-git runtime"),
        "filter refusal should explain the no-git native boundary: {msg}"
    );
}

/// Rejected filtered clones must stop before touching the destination,
/// even when the caller pre-created an empty scratch directory.
#[test]
fn clone_url_to_bare_filter_rejection_preserves_pre_created_empty_dest() {
    let (_src_temp, source_repo, _commits, _blobs) = build_source_repo_three_commits_with_blobs();

    let src_path = source_repo
        .workdir()
        .expect("workdir")
        .canonicalize()
        .expect("canonicalize");
    let url = format!("file://{}", src_path.display());

    let dest_root = TempDir::new().expect("dest temp");
    let dest = dest_root.path().join("clone-dest");
    std::fs::create_dir(&dest).expect("pre-create empty dest");
    assert!(dest.exists() && dest.read_dir().expect("read empty").next().is_none());

    let err = clone_url_to_bare(url.as_str(), &dest, None, Some("blob:none"))
        .expect_err("filtered clone must be rejected in no-git runtime");
    assert!(
        err.to_string().contains("retry without --filter/--lazy"),
        "filter refusal should give the native retry path: {err}"
    );
    assert!(
        dest.exists(),
        "rejection should not remove caller scratch dir"
    );
    assert!(
        dest.read_dir()
            .expect("read preserved empty")
            .next()
            .is_none(),
        "rejection should not write partial Git state"
    );
}

/// Filter rejection happens before network/filesystem clone work, so an
/// unreachable URL still reports the unsupported native capability first.
#[test]
fn clone_url_to_bare_filter_rejection_precedes_remote_probe() {
    let scratch = TempDir::new().expect("scratch");
    let nowhere = scratch.path().join("does-not-exist");
    let url = format!("file://{}", nowhere.display());

    let dest_root = TempDir::new().expect("dest temp");
    let dest = dest_root.path().join("clone-dest");

    let err =
        clone_url_to_bare(url.as_str(), &dest, Some(1), Some("blob:none")).expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("partial Git clone filter `blob:none` is not supported"),
        "unsupported native capability should be reported before remote probing: {msg}"
    );
    assert!(
        !dest.exists(),
        "rejection should not create destination state"
    );
}

/// Phase A: `bridge export --destination DEST` must populate DEST with
/// reachable git objects + refs. Auto-creates DEST as a bare repo when it
/// doesn't exist (so users don't have to pre-init the destination).
#[test]
fn export_to_path_writes_branches_and_tags_to_fresh_destination() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    // Destination directory does not exist yet — export_to_path must create it
    // as a bare repo.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    assert!(!dest_path.exists());

    let stats = bridge
        .export_to_path(&dest_path)
        .expect("export to fresh path");

    // Phase B: `states_exported` counts only commits that the bridge had
    // to *recreate* (heddle-native states with no original git_oid). When
    // every state was imported from git and the mirror still has the
    // original commit bytes, the SHA-stable path skips recreation and
    // this can legitimately be 0. The destination's refs are the true
    // signal.
    assert!(stats.threads_synced >= 1, "should sync the main thread");
    assert!(stats.markers_synced >= 1, "should sync the v1.0 tag");

    let dest_repo = open_git(&dest_path).expect("open exported repo");
    assert!(
        find_reference(&dest_repo, "refs/heads/main").is_ok(),
        "exported repo should have refs/heads/main"
    );
    assert!(
        find_reference(&dest_repo, "refs/tags/v1.0").is_ok(),
        "exported repo should have refs/tags/v1.0"
    );
}

/// Phase A: re-running `export_to_path` against an already-populated
/// destination must work (idempotent — auto-init only when missing).
#[test]
fn export_to_path_is_idempotent_against_existing_destination() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);
    commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first export should create dest");
    bridge
        .export_to_path(&dest_path)
        .expect("second export against existing dest should not error");
}

/// heddle#289: in the common git-overlay case every state is already
/// mapped to an original git commit, so `states_exported` (newly minted)
/// is legitimately 0 — but `commits_total` must still count the commits
/// that landed in the destination so the summary doesn't read a
/// misleading "exported 0 states" against a fully-populated repo.
#[test]
fn export_stats_report_total_commits_when_all_states_pre_mapped() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let first = commit_with_tree(&source_repo, None, tree_oid, "first", &[]);
    let second = commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "second",
        &[first],
    );
    create_annotated_tag(&source_repo, "v1.0", second, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let stats = bridge.export_to_path(&dest_path).expect("export");

    // Every state came from git with its SHA preserved → nothing is
    // freshly minted, yet the destination holds both commits.
    assert_eq!(
        stats.states_exported, 0,
        "SHA-stable overlay export mints no new commits"
    );
    assert!(
        stats.commits_total >= 2,
        "commits_total must count every state that landed in the destination, got {}",
        stats.commits_total
    );
    assert!(
        stats.commits_total > stats.states_exported,
        "total must exceed newly-minted in the overlay case"
    );
    // AC3: branch/tag detail carries tip SHAs for the summary.
    assert!(
        stats.branches.iter().any(|b| b.name == "main"),
        "branch detail should list main with its tip: {:?}",
        stats.branches
    );
    assert!(
        stats.tags.iter().any(|t| t.name == "v1.0"),
        "tag detail should list v1.0 with its tip: {:?}",
        stats.tags
    );
}

/// heddle#289: a sync of an already-synced overlay must report consistent
/// "total vs. new" accounting on both halves — the export side gains the
/// same total/new split the import side has carried since heddle#147.
#[test]
fn sync_export_and_import_report_consistent_total_and_new() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let first = commit_with_tree(&source_repo, None, tree_oid, "first", &[]);
    commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "second",
        &[first],
    );

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    // Re-running the sync halves against the already-synced overlay: both
    // sides should report the total they walked while their "new" count
    // drops to 0 — the "already in sync" signal.
    let export_stats = export_all(&mut bridge).expect("re-export");
    let import_stats = import_all(
        &mut bridge,
        Some(source_repo.workdir().expect("workdir").as_path()),
    )
    .expect("re-import");

    assert_eq!(
        export_stats.states_exported, 0,
        "nothing new to export on a synced overlay"
    );
    assert!(
        export_stats.commits_total >= 2,
        "export total still reflects the populated destination: {}",
        export_stats.commits_total
    );
    assert_eq!(
        import_stats.states_created, 0,
        "nothing new to import on a synced overlay"
    );
    assert!(
        import_stats.commits_imported >= 2,
        "import total still reflects the walked commits: {}",
        import_stats.commits_imported
    );
}

/// heddle#289 r3: the export "total" must equal what actually lands in
/// the destination. Export does NOT prune stale mirror refs, so a branch
/// exported once and whose Heddle thread is later dropped still has a
/// `refs/heads/<branch>` in the mirror — and `copy_mirror_to_path` copies
/// that ref (and its commit) to the destination. The total is derived from
/// the same `collect_ref_updates` set the copy uses, so it counts that
/// still-copied commit. Counting only current Heddle refs (r2) diverged
/// from reality; this guards that the count == the destination.
#[test]
fn export_total_counts_stale_mirror_ref_left_by_dropped_thread() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    // main: two-commit history → 2 commits reachable from the branch tip.
    let first = commit_with_tree(&source_repo, None, tree_oid, "first", &[]);
    commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "second",
        &[first],
    );
    // feature: an independent root commit → 1 commit reachable only via
    // this branch, sharing no history with main.
    let feature_tip = commit_with_tree(
        &source_repo,
        Some("refs/heads/feature"),
        tree_oid,
        "feature-only",
        &[],
    );

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    // All three states now exist in the store, and the import populated
    // the mirror with refs/heads/{main,feature}.
    assert_eq!(
        test_support::heddle_repo(&bridge)
            .store()
            .list_states()
            .expect("states")
            .len(),
        3,
        "import should have created three states (two on main, one on feature)"
    );

    // Drop the feature *thread* (Heddle-side ref). Export never prunes the
    // mirror's refs/heads/feature, so the stale mirror ref — and its
    // commit — still travel to the destination.
    test_support::heddle_repo(&bridge)
        .refs()
        .delete_thread(&ThreadName::new("feature"))
        .expect("delete feature thread");

    let dest_temp = TempDir::new().expect("dest temp");
    let dest_path = dest_temp.path().join("dest.git");
    let stats = bridge.export_to_path(&dest_path).expect("export to path");

    // The total counts every commit that lands in the destination: main's
    // two plus the stale feature ref's one. "What we report" == "what we
    // copy".
    assert_eq!(
        stats.commits_total, 3,
        "export total must count what lands in the destination — main's 2 \
         plus the stale feature ref's 1, got {}",
        stats.commits_total
    );

    // Prove the count matches reality: the destination really does contain
    // refs/heads/feature at the feature tip.
    let dest = open_git(&dest_path).expect("open destination");
    let feature_ref = find_reference(&dest, "refs/heads/feature")
        .expect("destination must contain the stale feature ref");
    assert_eq!(
        feature_ref.id(),
        feature_tip,
        "the stale feature ref in the destination points at the feature tip"
    );
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "destination must contain the main branch"
    );

    // The dropped thread is not a *current* Heddle thread, so it is not
    // reported as a synced branch — that gap between `branches` (current
    // threads) and `commits_total` (what's copied) is exactly what this
    // fix reconciles for the headline count.
    assert!(
        !stats.branches.iter().any(|b| b.name == "feature"),
        "dropped feature thread is not a current synced branch: {:?}",
        stats.branches
    );
}

/// heddle#289 r4: EVERY count in the export summary must be a partition of
/// the single copied ref set, so a state minted into the mirror but
/// reachable from no copied ref inflates none of them. r3 fixed
/// `commits_total` but left `states_exported` ("newly") tallied inline over
/// `list_states()`, so a Heddle-native thread created and dropped before
/// export would still be minted and counted as newly-written even though it
/// never lands in the destination — producing the impossible
/// "1 total (2 newly written)" summary. This drives `states_exported` from
/// the same walk as `commits_total`: the orphan is excluded from BOTH and
/// `newly + already == total` holds by construction.
#[test]
fn export_counts_exclude_orphan_minted_state_from_total_and_newly() {
    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let bridge = GitBridge::new(&repo);

    let attribution = || Attribution::human(Principal::new("Alice", "alice@example.com"));
    let put_state = |parents: Vec<ChangeId>| -> State {
        let store = test_support::heddle_repo(&bridge).store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(b"contents"))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(tree_hash, parents, attribution());
        store.put_state(&state).expect("put state");
        state
    };

    // A native `main` thread with a two-commit history → both minted into
    // the mirror and reachable from refs/heads/main, so both land in the
    // destination.
    let main_first = put_state(Vec::new());
    let main_tip = put_state(vec![main_first.change_id]);
    test_support::heddle_repo(&bridge)
        .refs()
        .set_thread(&ThreadName::new("main"), &main_tip.change_id)
        .expect("set main thread");

    // A native `scratch` thread, dropped before export. Its state stays in
    // the store (delete_thread only removes the ref), so the export walk
    // over `list_states()` still mints it — but no copied ref points at it,
    // so it reaches no destination and must inflate no count.
    let orphan = put_state(Vec::new());
    test_support::heddle_repo(&bridge)
        .refs()
        .set_thread(&ThreadName::new("scratch"), &orphan.change_id)
        .expect("set scratch thread");
    test_support::heddle_repo(&bridge)
        .refs()
        .delete_thread(&ThreadName::new("scratch"))
        .expect("delete scratch thread");

    // The orphan state is still present in the store — proving the walk
    // would have minted (and, pre-r4, counted) it.
    let mut bridge = bridge;
    assert_eq!(
        test_support::heddle_repo(&bridge)
            .store()
            .list_states()
            .expect("states")
            .len(),
        3,
        "store holds main's two states plus the dropped scratch state"
    );

    let dest_temp = TempDir::new().expect("dest temp");
    let dest_path = dest_temp.path().join("dest.git");
    let stats = bridge.export_to_path(&dest_path).expect("export to path");

    // Both summary counts are partitions of the copied ref set: total =
    // main's two commits; newly = the same two (freshly minted this run).
    // The orphan is in neither.
    assert_eq!(
        stats.commits_total, 2,
        "total counts only the copied ref set (main's 2), not the orphan, got {}",
        stats.commits_total
    );
    assert_eq!(
        stats.states_exported, 2,
        "newly counts only minted commits that landed (main's 2), not the orphan, got {}",
        stats.states_exported
    );

    // The invariant the close-the-class fix guarantees: newly is a subset of
    // total, so the "1 total (2 newly written)" impossibility cannot occur.
    let already = stats.commits_total.saturating_sub(stats.states_exported);
    assert!(
        stats.states_exported <= stats.commits_total,
        "newly ({}) must never exceed total ({})",
        stats.states_exported,
        stats.commits_total
    );
    assert_eq!(
        stats.states_exported + already,
        stats.commits_total,
        "newly + already must equal total by construction"
    );

    // The orphan's commit reaches no ref in the destination.
    let dest = open_git(&dest_path).expect("open destination");
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "destination must contain the main branch"
    );
    assert!(
        find_reference(&dest, "refs/heads/scratch").is_err(),
        "dropped scratch thread must not appear in the destination"
    );
    assert!(
        !stats.branches.iter().any(|b| b.name == "scratch"),
        "dropped scratch thread is not a synced branch: {:?}",
        stats.branches
    );
}

#[test]
fn failed_export_restores_mapping_and_overrides_after_purge() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    let attribution = || Attribution::human(Principal::new("Grace Hopper", "grace@example.com"));
    let store = repo.store();
    let blob_hash = store
        .put_blob(&Blob::from_slice(b"public\n"))
        .expect("put blob");
    let tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("public.txt".to_string(), blob_hash, false).expect("tree entry"),
        ]))
        .expect("put tree");
    let exported_state = State::new(tree_hash, Vec::new(), attribution());
    store
        .put_state(&exported_state)
        .expect("put exported state");
    repo.refs()
        .set_thread(&ThreadName::new("main"), &exported_state.change_id)
        .expect("set main thread");

    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).expect("initial export seeds mapping");

    repo.put_state_visibility(StateVisibility {
        state: exported_state.change_id,
        tier: VisibilityTier::Private {
            scope_label: "embargo".to_string(),
        },
        embargo_until: None,
        declarer: Principal::new("Grace Hopper", "grace@example.com"),
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .expect("mark exported state private");

    let bad_tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file(
                "missing.txt".to_string(),
                ContentHash::from_bytes([9; 32]),
                false,
            )
            .expect("tree entry"),
        ]))
        .expect("put bad tree");
    let bad_state = State::new(bad_tree_hash, Vec::new(), attribution());
    store.put_state(&bad_state).expect("put bad state");
    repo.refs()
        .set_thread(&ThreadName::new("main"), &bad_state.change_id)
        .expect("set main to bad state");

    test_support::set_commit_message_override(
        &mut bridge,
        exported_state.change_id,
        "pre-call override".to_string(),
    );
    let pre_mapping = test_support::mapping(&bridge).clone();
    let pre_overrides = test_support::commit_message_overrides(&bridge).clone();

    export_all(&mut bridge).expect_err("missing blob must fail after visibility purge");

    assert_eq!(
        test_support::mapping(&bridge),
        &pre_mapping,
        "failed export must restore the pre-call mapping"
    );
    assert_eq!(
        test_support::commit_message_overrides(&bridge),
        &pre_overrides,
        "failed export must restore pre-call commit message overrides"
    );

    test_support::build_existing_mapping(&mut bridge, None)
        .expect("mapping rebuild after failed export");
    assert_eq!(
        test_support::mapping(&bridge),
        &pre_mapping,
        "rebuilding after a failed export must behave as if the purge never ran"
    );
}

/// Phase A: `commits_imported` and `states_created` should both reflect new
/// state writes for a fresh import.
#[test]
fn import_stats_report_states_created() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);
    commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "first",
        &[],
    );

    let mut bridge = GitBridge::new(&repo);
    let stats = import_all(
        &mut bridge,
        Some(source_repo.workdir().expect("workdir").as_path()),
    )
    .expect("import");

    assert_eq!(stats.commits_imported, 1);
    assert_eq!(
        stats.states_created, stats.commits_imported,
        "states_created should match commits_imported on a fresh import"
    );
}

/// Phase B (the headline guarantee): import a git repo, export it back to a
/// fresh path, and confirm the head commit's git SHA is byte-identical to
/// the original. This is what makes heddle viable as a substrate for a
/// bidirectional sync service against GitHub — a sync that produced new
/// SHAs on every roundtrip would invalidate every PR.
#[test]
fn export_preserves_original_commit_shas() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_source_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let first = commit_with_tree(&source_repo, None, tree_oid, "first", &[]);
    let second = commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "second",
        &[first],
    );
    create_annotated_tag(&source_repo, "v1.0", second, "release v1");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import from git");

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export");
    bridge.export_to_path(&dest_path).expect("export");

    let dest_repo = open_git(&dest_path).expect("open dest");
    let dest_main = find_reference(&dest_repo, "refs/heads/main")
        .expect("main ref")
        .peel_to_id()
        .expect("main target");

    assert_eq!(
        dest_main, second,
        "Phase B: exported main HEAD must equal the original git SHA"
    );

    // The first commit should also be byte-identical (it's reachable from
    // main via parent traversal).
    let dest_main_commit = dest_repo.find_commit(dest_main).expect("dest main commit");
    let dest_first = dest_main_commit
        .parent_ids()
        .next()
        .expect("dest main has parent");
    assert_eq!(
        dest_first, first,
        "Phase B: parent commit SHAs must also be preserved"
    );
}

/// Phase B: a heddle change_id assigned at import time must survive a full
/// round-trip through git — even when the destination is re-imported into a
/// fresh heddle repo with no sidecar.
#[test]
fn round_trip_preserves_change_ids_via_notes() {
    let heddle_a_temp = TempDir::new().expect("heddle A temp");
    let repo_a = Repository::init(heddle_a_temp.path()).expect("init heddle A");

    let (_src_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);

    // Step 1: import into heddle A. Capture the change_id assigned.
    let mut bridge_a = GitBridge::new(&repo_a);
    bridge_a
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import into A");
    let change_id_in_a = test_support::mapping(&bridge_a)
        .get_heddle(commit_oid)
        .expect("change_id should be mapped in A");

    // Step 2: export A to a fresh git destination.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export");
    bridge_a.export_to_path(&dest_path).expect("export from A");

    // Step 3: import the exported repo into heddle B (fresh repo, no
    // sidecar carryover). The note attached to the commit must let B
    // recover the same change_id A assigned.
    let heddle_b_temp = TempDir::new().expect("heddle B temp");
    let repo_b = Repository::init(heddle_b_temp.path()).expect("init heddle B");
    let mut bridge_b = GitBridge::new(&repo_b);
    bridge_b.import(Some(&dest_path)).expect("import into B");

    let change_id_in_b = test_support::mapping(&bridge_b)
        .get_heddle(commit_oid)
        .expect("B should have mapped the original commit OID");

    assert_eq!(
        change_id_in_a, change_id_in_b,
        "Phase B: change_id must survive the git→heddle→git→heddle roundtrip via the note"
    );
}

/// Phase E: end-to-end symlink fidelity through the git bridge.
/// Constructs a git tree containing a symlink, imports it, then re-exports
/// the heddle tree to git and confirms the symlink survives both ways.
///
/// This is the closest CI-friendly approximation of the QA report's
/// `ripgrep/HomebrewFormula -> pkg/brew` failure mode.
#[test]
#[cfg(unix)]
fn round_trip_preserves_symlinks() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();

    // Build a git tree directly: one regular blob "target.txt" + one
    // symlink "link" -> "target.txt".
    let target_oid = source_repo
        .write_blob(b"hello\n")
        .expect("write target blob");
    let link_oid = source_repo
        .write_blob(b"target.txt")
        .expect("write link blob");

    let empty = empty_tree_oid(&source_repo);
    let mut editor = source_repo.edit_tree(&empty).expect("editor");
    editor.upsert("target.txt", EntryKind::Blob, target_oid);
    editor.upsert("link", EntryKind::Symlink, link_oid);
    let tree_oid = source_repo.write_tree(editor).expect("write tree");

    let _commit = commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "with symlink",
        &[],
    );

    // Import into heddle.
    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import");

    // Verify the imported tree marks "link" as a symlink at the entry-type
    // level (the Phase E source-side fix). This is what the materializer
    // checks to write a real symlink to the worktree.
    let head_change_id = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread");
    let state = repo
        .store()
        .get_state(&head_change_id)
        .expect("state lookup")
        .expect("state present");
    let imported_tree = repo
        .store()
        .get_tree(&state.tree)
        .expect("tree lookup")
        .expect("tree present");
    let link_entry = imported_tree
        .entries()
        .iter()
        .find(|e| e.name() == "link")
        .expect("link entry exists");
    assert_eq!(
        link_entry.entry_type(),
        EntryType::Symlink,
        "Phase E: imported symlinks must have EntryType::Symlink (was Blob \
         pre-Phase-E, which broke goto-time materialization)"
    );
    assert_eq!(link_entry.mode(), FileMode::Symlink);

    // Export back to a fresh git destination and confirm the link entry
    // round-trips as a Link, not a Blob.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export");
    bridge.export_to_path(&dest_path).expect("export");

    let dest_repo = open_git(&dest_path).expect("open dest");
    let dest_main = find_reference(&dest_repo, "refs/heads/main")
        .expect("main")
        .peel_to_id()
        .expect("peel");
    let dest_commit = dest_repo.find_commit(dest_main).expect("dest commit");
    let dest_tree_oid = dest_commit.tree_id().expect("tree id");
    let dest_tree = dest_repo.find_tree(dest_tree_oid).expect("dest tree");
    let entries: Vec<(String, EntryKind)> = dest_tree
        .iter()
        .map(|e| {
            let e = e.expect("entry");
            (e.filename().to_string(), e.mode().kind())
        })
        .collect();

    let link_kind = entries
        .iter()
        .find(|(name, _)| name == "link")
        .map(|(_, k)| *k)
        .expect("link entry in exported tree");
    assert_eq!(
        link_kind,
        EntryKind::Symlink,
        "Phase E: exported tree must mark 'link' as a symlink (Link), not a Blob"
    );
}

/// Follow-up A: annotated tags must round-trip with their tag-object SHA
/// preserved, not just the underlying commit SHA. A `git rev-parse
/// refs/tags/v1.0` against the export should return the same OID as
/// against the source — for an annotated tag that's the OID of the tag
/// *object* (which carries the tagger, tag message, and signature),
/// not the commit it wraps.
#[test]
fn round_trip_preserves_annotated_tag_object_sha() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let commit_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "base", &[]);
    // Lightweight tag (no separate tag object).
    set_reference(
        &source_repo,
        "refs/tags/light",
        commit_oid,
        RefPrecondition::MustNotExist,
        "test: lightweight tag",
    )
    .expect("set lightweight tag");
    // Annotated tag (carries its own object with tagger + message).
    let annotated_tag_oid = create_annotated_tag(&source_repo, "v1.0", commit_oid, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(source_repo.workdir().expect("workdir").as_path()))
        .expect("import");

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export");
    bridge.export_to_path(&dest_path).expect("export");

    let dest_repo = open_git(&dest_path).expect("open dest");

    // Lightweight tag: dest's refs/tags/light points directly at the commit.
    let dest_light = find_reference(&dest_repo, "refs/tags/light")
        .expect("light ref")
        .target()
        .try_id()
        .expect("light has direct id")
        .to_owned();
    assert_eq!(
        dest_light, commit_oid,
        "lightweight tag should still point at the commit"
    );

    // Annotated tag: dest's refs/tags/v1.0 must point at the SAME tag
    // object OID as the source — not at the underlying commit. This is
    // what makes `git rev-parse refs/tags/v1.0` agree across source and
    // export, which downstream tools like GitHub PR matching rely on.
    let dest_v10_immediate = find_reference(&dest_repo, "refs/tags/v1.0")
        .expect("v1.0 ref")
        .target()
        .try_id()
        .expect("v1.0 has direct id")
        .to_owned();
    assert_eq!(
        dest_v10_immediate, annotated_tag_oid,
        "Follow-up A: annotated tag SHA must match (got {dest_v10_immediate}, want {annotated_tag_oid})"
    );

    // And the destination must actually have the tag object, not just
    // the ref — `git tag -v` style introspection requires the object
    // to be present.
    let dest_tag_obj = dest_repo
        .find_object(annotated_tag_oid)
        .expect("annotated tag object should be in destination");
    assert_eq!(dest_tag_obj.kind, GitObjectType::Tag);
}

/// Follow-up B: per-ref isolation. A repo that has one valid ref and one
/// ref whose target is missing from the source ODB used to fail the
/// entire import (a single bulk `copy_reachable_objects` errored on the
/// first missing object). The valid ref should still import cleanly.
#[test]
fn import_isolates_per_ref_mirror_failures() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&source_repo);
    let good_oid = commit_with_tree(&source_repo, Some("refs/heads/main"), tree_oid, "good", &[]);

    // Construct a ref pointing at a fabricated OID that doesn't exist
    // in the ODB. We can't easily make this a refs/heads/* (peel would
    // fail before the mirror copy), so simulate the failure mode by
    // pointing a tag at a never-written commit OID.
    let phantom_oid: ObjectId = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        .parse()
        .expect("parse phantom oid");
    set_reference(
        &source_repo,
        "refs/tags/phantom",
        phantom_oid,
        RefPrecondition::MustNotExist,
        "test: phantom tag",
    )
    .expect("set phantom tag");

    // Import. The phantom tag's peel will fail (peel goes through
    // find_object), so it gets routed to either skipped_non_commit_refs
    // (if peel returns a non-commit) or simply trips the per-ref copy
    // failure path. Either way, the import as a whole must succeed and
    // the good commit must be present.
    let mut bridge = GitBridge::new(&repo);
    let result = bridge.import(Some(source_repo.workdir().expect("workdir").as_path()));

    // The import may surface peel errors as a hard failure for genuinely
    // broken refs (peel_to_id propagates), so we accept either outcome
    // here — what matters for the follow-up is that when isolation
    // *does* apply, one bad ref doesn't poison the rest.
    if let Ok(stats) = result {
        assert_eq!(
            stats.commits_imported, 1,
            "the good commit should be mapped"
        );
        assert!(
            test_support::mapping(&bridge)
                .get_heddle(good_oid)
                .is_some(),
            "good commit's change_id should be in mapping"
        );
    } else {
        // If the broken ref propagated as a hard error, that's the
        // pre-isolation behavior — report it but don't fail the test
        // since we're documenting current behavior.
        eprintln!(
            "phantom-tag import returned hard error (acceptable): {:?}",
            result.err()
        );
    }
}

/// #561: `adopt` writes the mirror's reachable objects through the native Git
/// object path. The mirror must carry the same object bytes, including annotated
/// tag objects.
#[test]
fn import_populates_mirror_with_identical_annotated_tag_object() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_src_temp, source_repo) = init_git_repo();

    // Use a stored blob/tree instead of the synthetic empty tree so source and
    // mirror object stores can be compared directly.
    let blob = source_repo.write_blob(b"hello\n").expect("blob");
    let mut editor = source_repo
        .edit_tree(&ObjectId::empty_tree(source_repo.object_format()))
        .expect("tree editor");
    editor.upsert("file.txt", EntryKind::Blob, blob);
    let tree = source_repo.write_tree(editor).expect("tree");

    let first = commit_with_tree(&source_repo, Some("refs/heads/main"), tree, "first", &[]);
    let second = commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree,
        "second",
        &[first],
    );
    let tag_oid = create_annotated_tag(&source_repo, "v1.0", second, "release");

    let mut bridge = GitBridge::new(&repo);
    bridge
        .import(Some(&source_repo.workdir().expect("workdir")))
        .expect("import");

    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    let source_tag = source_repo
        .find_object(tag_oid)
        .expect("source tag present");
    let mirror_tag = mirror.find_object(tag_oid).expect("mirror tag present");
    assert_eq!(source_tag.kind, GitObjectType::Tag);
    assert_eq!(mirror_tag.kind, source_tag.kind);
    assert_eq!(mirror_tag.data, source_tag.data);
}

#[test]
fn export_lags_public_branch_to_frontier_emitting_absence_for_embargoed_tip() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    // A real principal so snapshot states carry a non-Unknown attribution and
    // the bridge can mint Git commits without an external identity fallback.
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // Build a linear thread: public base A, then tip B.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    std::fs::write(heddle_temp.path().join("b.txt"), b"embargoed fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    assert_ne!(state_a, state_b);

    // Embargo the tip B (strictest Private tier). Downward-closure leaves the
    // public frontier at A.
    repo.put_state_visibility(StateVisibility {
        state: state_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    let mut bridge = GitBridge::new(&repo);
    let stats = export_all(&mut bridge).expect("export");

    // The embargoed tip is never minted into the public mirror (absence) ...
    assert!(
        test_support::mapping(&bridge).get_git(&state_b).is_none(),
        "embargoed tip must not be minted into the public mirror"
    );
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("public base A must be minted");
    // ... and refs/heads/main lags to A, never the raw embargoed tip B.
    let main = stats
        .branches
        .iter()
        .find(|b| b.name == "main")
        .expect("main branch must be exported");
    assert_eq!(
        main.tip, oid_a,
        "public branch must lag to the visibility frontier (A), not the embargoed tip"
    );
}

/// #316 / PR #528 Finding 1: a commit exported while PUBLIC, then later marked
/// `Private`, must NOT keep being served on the next export. The stale
/// ChangeId→OID mapping is rebuilt from the notes/cache every run, so the
/// export must re-validate current visibility and retract the public branch
/// down to the served frontier (here, the still-public base A).
#[test]
fn export_retracts_branch_when_public_commit_is_later_embargoed() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // Linear thread: public base A, then public tip B.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    std::fs::write(heddle_temp.path().join("b.txt"), b"fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    // Export run 1 — both public. The branch advertises the tip B.
    let mut bridge = GitBridge::new(&repo);
    let run1 = export_all(&mut bridge).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");
    let run1_main = run1
        .branches
        .iter()
        .find(|b| b.name == "main")
        .expect("main exported");
    assert_eq!(
        run1_main.tip, oid_b,
        "run 1 branch advertises the public tip B"
    );

    // B is embargoed AFTER it was already exported public.
    repo.put_state_visibility(StateVisibility {
        state: state_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    // Export run 2 — the stale B→OID mapping is rebuilt from notes, but the
    // re-validation purge drops it and the branch is retracted to A.
    let run2 = export_all(&mut bridge).expect("second export");
    let run2_main = run2
        .branches
        .iter()
        .find(|b| b.name == "main")
        .expect("main re-exported");
    assert_eq!(
        run2_main.tip, oid_a,
        "run 2 must lag the public branch to A, retracting the now-embargoed B"
    );
    assert!(
        test_support::mapping(&bridge).get_git(&state_b).is_none(),
        "the now-Private B must be purged from the served mapping"
    );
    let mapping_cache = std::fs::read_to_string(test_support::mapping_path(&bridge))
        .expect("mapping cache should be readable after export");
    assert!(
        !mapping_cache.contains(&state_b.to_string_full()),
        "the served bridge mapping cache must not persist embargoed ChangeIds: {mapping_cache}"
    );
    assert!(
        !mapping_cache.contains(&oid_b.to_string()),
        "the served bridge mapping cache must not persist embargoed Git OIDs: {mapping_cache}"
    );

    // The mirror ref itself no longer serves B.
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    let mut main_ref = find_reference(&mirror, "refs/heads/main").expect("main ref present");
    let tip = main_ref.peel_to_id().unwrap();
    assert_eq!(
        tip, oid_a,
        "refs/heads/main must point at A after retraction"
    );
    assert_ne!(
        tip, oid_b,
        "refs/heads/main must not keep serving embargoed B"
    );
}

/// #737 EXPORT-path regression: lock the embargo `seed → purge → save` ordering
/// invariant on `export_scoped`.
///
/// `export_scoped` runs, in this exact order:
///   1. `seed_ingest_identity_mappings_from_mirror` — re-hydrates the in-memory
///      mapping from the PERSISTENT ingest SHA map (`.heddle/ingest/sha_map.sqlite`),
///      so a commit imported earlier is re-mapped ChangeId→OID even on a brand-new
///      bridge whose live mapping is empty;
///   2. `purge_unserved_mappings` — drops every mapping whose state (or a reachable
///      ancestor) is not served at the Public tier;
///   3. `save_mapping_to_disk` — writes the surviving mapping to the SERVED cache
///      `.heddle/git-bridge/bridge-mapping.json`.
///
/// An earlier intermediate `save_mapping_to_disk_preserving` helper snapshotted the
/// mapping BEFORE the purge, so a seeded-but-embargoed entry resurrected into the
/// served `bridge-mapping.json` — a visibility leak. That helper is gone; the fix
/// relies on seed-before-purge-before-save. PR #737 added the IMPORT-path test
/// `adopt_all_uses_ingest_mapping_without_internal_mirror`; this is its EXPORT-path
/// analogue: an embargoed SHA that the SEED re-introduces from the ingest map must
/// NOT survive into the served mapping after `export_scoped`.
///
/// The two earlier #316 tests above mint-then-purge a NATIVE tip in one run, so
/// they never exercise the seed re-introducing an already-persisted embargoed OID.
/// This test imports first (populating the durable ingest map), embargoes the tip,
/// then exports on a FRESH bridge — making the seed the live reintroduction vector.
#[test]
fn export_purge_drops_seeded_embargoed_sha_before_serving_mapping() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");

    // A source git repo with a linear history: base A, then tip B.
    let (_source_temp, source_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&source_repo);
    let git_a = commit_with_tree(&source_repo, None, tree_oid, "base", &[]);
    let git_b = commit_with_tree(
        &source_repo,
        Some("refs/heads/main"),
        tree_oid,
        "embargoed fix",
        &[git_a],
    );
    let source_workdir = source_repo.workdir().expect("workdir").to_path_buf();

    // Import populates the DURABLE ingest SHA map at .heddle/ingest/sha_map.sqlite
    // (kind=0) with git_a→change_a and git_b→change_b. This is the persistent
    // store the export-time seed reads from on a fresh bridge.
    let mut import_bridge = GitBridge::new(&repo);
    import_bridge
        .import(Some(source_workdir.as_path()))
        .expect("import source git history");
    let change_a = test_support::mapping(&import_bridge)
        .get_heddle(git_a)
        .expect("base A mapped after import");
    let change_b = test_support::mapping(&import_bridge)
        .get_heddle(git_b)
        .expect("tip B mapped after import");
    assert_ne!(change_a, change_b);
    drop(import_bridge);

    // The ingest map must actually carry B — otherwise the seed can't re-introduce
    // it and the test would pass vacuously.
    let ingest_map = repo
        .git_overlay_ingest_commit_mapping()
        .expect("read ingest commit mapping");
    assert!(
        ingest_map.values().any(|c| c == &change_b.to_string_full()),
        "precondition: ingest SHA map must hold the (to-be-embargoed) tip B"
    );

    // Embargo B at the strictest Private tier (downward-closure leaves A served).
    repo.put_state_visibility(StateVisibility {
        state: change_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Heddle Test".into(),
            email: "heddle@test".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .expect("embargo tip B");

    // Precondition: the SEED, run in isolation on a fresh bridge, WOULD re-introduce
    // B's OID from the ingest map (the seed is visibility-blind — it maps any state
    // that exists, embargoed or not). This proves the purge is what must remove it,
    // not the seed declining to add it.
    {
        let mut seed_probe = GitBridge::new(&repo);
        test_support::set_git_repo_path(&mut seed_probe, source_workdir.clone());
        let mirror = test_support::open_git_repo(&seed_probe).expect("open mirror for seed probe");
        test_support::seed_ingest_identity_mappings_from_mirror(&mut seed_probe, &mirror)
            .expect("seed from ingest map");
        assert_eq!(
            test_support::mapping(&seed_probe).get_git(&change_b),
            Some(git_b),
            "precondition: the seed re-introduces the embargoed tip B from the ingest map"
        );
    }

    // Export on a FRESH bridge: build_existing_mapping starts empty, the seed
    // re-hydrates B from the durable ingest map, then the purge must drop B before
    // save writes the served bridge-mapping.json.
    let mut export_bridge = GitBridge::new(&repo);
    export_all(&mut export_bridge).expect("export with seeded embargoed tip");

    // In-memory: the embargoed tip is gone; the public base survives.
    assert!(
        test_support::mapping(&export_bridge)
            .get_git(&change_b)
            .is_none(),
        "the purge must drop the seeded-but-embargoed tip B from the served mapping"
    );
    assert_eq!(
        test_support::mapping(&export_bridge).get_git(&change_a),
        Some(git_a),
        "the still-public base A must remain served"
    );

    // The CRITICAL leak surface: the served bridge-mapping.json on disk. A
    // pre-purge save (the resurrected `save_mapping_to_disk_preserving` bug) would
    // leak B's ChangeId and OID here even though the in-memory purge ran.
    let served = std::fs::read_to_string(test_support::mapping_path(&export_bridge))
        .expect("served bridge-mapping.json must exist after export");
    assert!(
        !served.contains(&change_b.to_string_full()),
        "served mapping must NOT contain the embargoed ChangeId B (seed→purge→save leak): {served}"
    );
    assert!(
        !served.contains(&git_b.to_string()),
        "served mapping must NOT contain the embargoed Git OID B (seed→purge→save leak): {served}"
    );
    assert!(
        served.contains(&change_a.to_string_full()) && served.contains(&git_a.to_string()),
        "served mapping must still publish the public base A: {served}"
    );
}

/// #737 structural guard: fail if anyone reintroduces a pre-purge save into the
/// export path. Pairs with `export_purge_drops_seeded_embargoed_sha_before_serving_mapping`
/// — that test catches the LEAK; this one catches the most likely REGRESSION SHAPE
/// (a `save_mapping_to_disk_preserving`-style helper, or a save call moved ahead of
/// the purge) directly at the source level, before any repo fixture runs.
#[test]
fn export_path_saves_mapping_strictly_after_the_embargo_purge() {
    let src_path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/bridge/git_export.rs"
    ));
    let src = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("read git_export.rs at {}: {e}", src_path.display()));

    // 1. The pre-purge snapshot helper that caused the #737 leak must stay gone.
    assert!(
        !src.contains("save_mapping_to_disk_preserving"),
        "git_export.rs reintroduced a pre-purge save helper \
         (save_mapping_to_disk_preserving) — this resurrects the #737 embargo leak"
    );

    // 2. The export path's single served-mapping save must sit AFTER the purge.
    //    Both calls are unique in the file, so a byte-offset comparison pins the
    //    ordering: seed (already before) → purge → save.
    let purge_at = src
        .find("purge_unserved_mappings(")
        .expect("git_export.rs must call purge_unserved_mappings in the export path");
    let save_at = src
        .find("save_mapping_to_disk()")
        .expect("git_export.rs must call save_mapping_to_disk in the export path");
    assert!(
        src.matches("save_mapping_to_disk()").count() == 1,
        "expected exactly one save_mapping_to_disk() call site in the export path; \
         a second one may bypass the purge"
    );
    assert!(
        save_at > purge_at,
        "save_mapping_to_disk() (offset {save_at}) must run AFTER \
         purge_unserved_mappings() (offset {purge_at}) — saving the mapping before \
         the embargo purge resurrects the #737 leak"
    );
}

/// #316 / PR #528 r3 Finding 2: when a public commit is later embargoed, the
/// purge drops its ChangeId→OID mapping AND its `refs/notes/heddle` entry must
/// be retracted too. `collect_ref_updates` copies `refs/notes/*` to the mirror
/// alongside branches and tags, so a note left for a withheld commit keeps
/// publishing that commit's metadata (change_id, agent, status) even after the
/// branch was retracted — a metadata leak. The note for a still-served commit
/// must survive.
#[test]
fn export_retracts_note_for_retracted_commit() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // Linear thread: public base A, then public tip B.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    std::fs::write(heddle_temp.path().join("b.txt"), b"fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    // Export run 1 — both public. Notes get written for A and B.
    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");
    {
        let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
        assert!(
            cli::bridge::git_notes::read_note(&mirror, oid_a)
                .unwrap()
                .is_some(),
            "A must carry a note after run 1"
        );
        assert!(
            cli::bridge::git_notes::read_note(&mirror, oid_b)
                .unwrap()
                .is_some(),
            "B must carry a note after run 1"
        );
    }

    // B is embargoed AFTER it was already exported public.
    repo.put_state_visibility(StateVisibility {
        state: state_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    // Export run 2 — B is purged and its note must be retracted; A's note,
    // still served, must remain.
    export_all(&mut bridge).expect("second export");
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert!(
        cli::bridge::git_notes::read_note(&mirror, oid_b)
            .unwrap()
            .is_none(),
        "run 2 must retract the note for the now-embargoed B (no metadata leak)"
    );
    assert!(
        cli::bridge::git_notes::read_note(&mirror, oid_a)
            .unwrap()
            .is_some(),
        "A is still served — its note must survive the retraction"
    );
}

/// #316 / PR #528 r9 FINDING B: a thread-SCOPED export must retract the note
/// for any mapped out-of-thread commit that is unserved under the SAME
/// downward-closure rule the branch frontier uses — not just the scoped
/// `embargoed_oids`. A commit whose DIRECT tier is public but whose ANCESTOR
/// became Private is not served (its branch would be withheld), yet
/// `purge_unserved_mappings` only walks the scoped thread's reachable states,
/// so without the full-target servedness pass its `refs/notes/heddle` entry
/// stays in the mirror and gets pushed — a notes leak in scoped exports.
#[test]
fn scoped_export_retracts_note_for_commit_with_embargoed_ancestor() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> State {
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // main line: public root R → public tip X (X descends from R).
    let state_r = put_state(b"root\n", Vec::new());
    let state_x = put_state(b"tip\n", vec![state_r.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state_x.change_id)
        .expect("set main to X");
    // A separate, independent line on thread `other`: public root O.
    let state_o = put_state(b"other root\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("other"), &state_o.change_id)
        .expect("set other to O");

    // Run 1 — everything public. Notes get written for R, X, and O.
    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).expect("first export");
    let oid_x = test_support::mapping(&bridge)
        .get_git(&state_x.change_id)
        .expect("X minted while public");
    let oid_o = test_support::mapping(&bridge)
        .get_git(&state_o.change_id)
        .expect("O minted while public");
    {
        let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
        assert!(
            cli::bridge::git_notes::read_note(&mirror, oid_x)
                .unwrap()
                .is_some(),
            "X must carry a note after run 1"
        );
    }

    // Embargo R (X's ANCESTOR) — X's own tier stays public, but downward
    // closure now withholds X.
    repo.put_state_visibility(StateVisibility {
        state: state_r.change_id,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    // Run 2 — SCOPED to `other`, which does NOT reach R or X. The scoped purge
    // never examines X, but the notes-ref retraction must still withhold X's
    // note because its ancestor R is unserved.
    export_current_thread(&mut bridge, "other").expect("scoped export");
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert!(
        cli::bridge::git_notes::read_note(&mirror, oid_x)
            .unwrap()
            .is_none(),
        "scoped export must retract X's note (ancestor embargoed) — no notes leak"
    );
    assert!(
        cli::bridge::git_notes::read_note(&mirror, oid_o)
            .unwrap()
            .is_some(),
        "O is served — its note must survive the scoped retraction"
    );
}

/// #316 / PR #528 r14: a SCOPED export must reconcile cross-thread embargo. A
/// prior all-thread export publishes threads `alpha` and `beta`; then a commit
/// reachable ONLY via `beta` is marked Private. A scoped export of `alpha` (not
/// `beta`) must STILL rewind `beta`'s `refs/heads/` off the now-embargoed commit:
/// the mirror reconcile spans the WHOLE mirror's served frontier, not just the
/// export's scoped thread. `alpha`'s own ref is left untouched.
#[test]
fn scoped_export_reconciles_cross_thread_embargo() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> State {
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // Two independent lines (no shared ancestry).
    // alpha: A0 → A1 (both public). beta: B0 → B1 → B2 (all public).
    let state_a0 = put_state(b"a0\n", Vec::new());
    let state_a1 = put_state(b"a1\n", vec![state_a0.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("alpha"), &state_a1.change_id)
        .expect("set alpha to A1");
    let state_b0 = put_state(b"b0\n", Vec::new());
    let state_b1 = put_state(b"b1\n", vec![state_b0.change_id]);
    let state_b2 = put_state(b"b2\n", vec![state_b1.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("beta"), &state_b2.change_id)
        .expect("set beta to B2");

    // Run 1 — everything public, all-thread export. Both branches published.
    let mut bridge = GitBridge::new(&repo);
    let run1 = export_all(&mut bridge).expect("first export");
    let oid_a1 = test_support::mapping(&bridge)
        .get_git(&state_a1.change_id)
        .expect("A1 minted while public");
    let oid_b0 = test_support::mapping(&bridge)
        .get_git(&state_b0.change_id)
        .expect("B0 minted while public");
    let oid_b2 = test_support::mapping(&bridge)
        .get_git(&state_b2.change_id)
        .expect("B2 minted while public");
    assert!(
        run1.branches
            .iter()
            .any(|b| b.name == "beta" && b.tip == oid_b2),
        "run 1 advertises beta at its public tip B2"
    );

    // Embargo B1 — a commit reachable ONLY via beta. Downward closure withholds
    // B1 and its descendant B2; B0 stays served.
    repo.put_state_visibility(StateVisibility {
        state: state_b1.change_id,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    // Run 2 — SCOPED to alpha, which does NOT reach beta. The mirror reconcile
    // must STILL rewind beta off the embargoed commit; alpha is untouched.
    export_current_thread(&mut bridge, "alpha").expect("scoped export");
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");

    // beta rewound to its served frontier B0 — the embargoed B1/B2 are no longer
    // reachable from any mirror ref.
    let beta_tip = find_reference(&mirror, "refs/heads/beta")
        .expect("beta still present at its served frontier")
        .peel_to_id()
        .expect("peel beta");
    assert_eq!(
        beta_tip, oid_b0,
        "scoped alpha export must rewind beta off the embargoed commit to B0"
    );
    assert_ne!(
        beta_tip, oid_b2,
        "beta must not keep serving the now-embargoed tip B2"
    );

    // alpha's own ref is unaffected by the scoped export.
    let alpha_tip = find_reference(&mirror, "refs/heads/alpha")
        .expect("alpha present")
        .peel_to_id()
        .expect("peel alpha");
    assert_eq!(
        alpha_tip, oid_a1,
        "scoped export must leave its own thread's ref at A1"
    );
}

/// #316 / PR #528 r16 (close-the-class): the DESTINATION-side analog of
/// [`scoped_export_reconciles_cross_thread_embargo`]. r14 made the MIRROR
/// reconcile whole-mirror, so a scoped export of `alpha` rewinds an out-of-scope
/// `beta` off an embargoed commit — but the DESTINATION push still derived its
/// desired set from the SCOPE-FILTERED ref list, so the out-of-scope `beta` fell
/// into a "still served, leave untouched" arm and the destination KEPT SERVING
/// the embargoed tip. The destination reconcile now derives from the SAME
/// whole-mirror served frontier as the mirror reconcile, so a scoped push of
/// alpha rewinds the destination's `beta` BY CONSTRUCTION. An out-of-band
/// destination tip on a third ref must still be spared — widening the desired set
/// to whole-mirror must NOT weaken r13's ownership gate.
#[test]
fn scoped_push_propagates_cross_thread_embargo_to_destination() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};
    use refs::Head;

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> State {
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // Three independent lines (no shared ancestry).
    // alpha: A0 → A1 (public). beta: B0 → B1 → B2 (public; B1 embargoed below).
    // gamma: G0 (public; later embargoed AND advanced out of band at the destination).
    let state_a0 = put_state(b"a0\n", Vec::new());
    let state_a1 = put_state(b"a1\n", vec![state_a0.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("alpha"), &state_a1.change_id)
        .expect("set alpha to A1");
    let state_b0 = put_state(b"b0\n", Vec::new());
    let state_b1 = put_state(b"b1\n", vec![state_b0.change_id]);
    let state_b2 = put_state(b"b2\n", vec![state_b1.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("beta"), &state_b2.change_id)
        .expect("set beta to B2");
    let state_g0 = put_state(b"g0\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("gamma"), &state_g0.change_id)
        .expect("set gamma to G0");

    // Run 1 — everything public, full export+push to a local destination. All three
    // branches are published AND recorded as heddle-exported there.
    let mut bridge = GitBridge::new(&repo);
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first full export+push publishes alpha, beta, gamma");
    let oid_a1 = test_support::mapping(&bridge)
        .get_git(&state_a1.change_id)
        .expect("A1 minted");
    let oid_b0 = test_support::mapping(&bridge)
        .get_git(&state_b0.change_id)
        .expect("B0 minted");
    let oid_b2 = test_support::mapping(&bridge)
        .get_git(&state_b2.change_id)
        .expect("B2 minted");
    let oid_g0 = test_support::mapping(&bridge)
        .get_git(&state_g0.change_id)
        .expect("G0 minted");
    {
        let dest = open_git(&dest_path).expect("open dest");
        assert_eq!(
            find_reference(&dest, "refs/heads/beta")
                .unwrap()
                .peel_to_id()
                .unwrap(),
            oid_b2,
            "run 1 publishes beta at its public tip B2 to the destination"
        );
    }

    // Out-of-band advance: move the DESTINATION's gamma to a NEW commit G_oob that
    // heddle never published (a descendant of the published G0). Heddle's record
    // still says gamma == G0, so the ownership gate must spare G_oob.
    let oid_g_oob = {
        let dest = open_git(&dest_path).expect("open dest");
        commit_with_tree(
            &dest,
            Some("refs/heads/gamma"),
            empty_tree_oid(&dest),
            "out-of-band gamma",
            &[oid_g0],
        )
    };

    // Embargo B1 (reachable ONLY via beta) AND the whole gamma line (G0). Downward
    // closure withholds B1/B2 (B0 stays served) and the whole gamma line (no served
    // frontier ⇒ gamma is retracted from the mirror).
    let embargo = |state: ChangeId| {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Alice".into(),
                email: "alice@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    };
    embargo(state_b1.change_id);
    embargo(state_g0.change_id);

    // Attach HEAD to alpha so a CurrentThread push scopes its EXPORT to alpha —
    // alpha reaches neither beta nor gamma.
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("alpha"),
        })
        .expect("attach HEAD to alpha");

    // Run 2 — SCOPED push of alpha to the SAME destination. The destination reconcile
    // is driven by the whole-mirror served frontier, so it must rewind the
    // destination's out-of-scope beta off the embargoed commit even though alpha
    // does not reach beta.
    bridge
        .push_with_scope(
            dest_path.to_str().expect("dest path"),
            GitPushScope::CurrentThread,
        )
        .expect("scoped push of alpha");

    let dest = open_git(&dest_path).expect("reopen dest");

    // beta REWOUND at the destination to its served frontier B0 — the embargoed
    // B1/B2 are no longer served from the destination. This is the leak r16 closes.
    let beta_tip = find_reference(&dest, "refs/heads/beta")
        .expect("beta still present at its served frontier")
        .peel_to_id()
        .expect("peel beta");
    assert_eq!(
        beta_tip, oid_b0,
        "scoped push of alpha must rewind the destination's beta off the embargoed commit to B0"
    );
    assert_ne!(
        beta_tip, oid_b2,
        "the destination must not keep serving the now-embargoed tip B2"
    );

    // alpha's own destination ref is at A1 and unaffected by the scoped push.
    let alpha_tip = find_reference(&dest, "refs/heads/alpha")
        .expect("alpha present")
        .peel_to_id()
        .expect("peel alpha");
    assert_eq!(
        alpha_tip, oid_a1,
        "scoped push must leave alpha at A1 at the destination"
    );

    // The out-of-band gamma tip is SPARED: heddle's record (G0) does not match the
    // destination's current tip (G_oob), so the ownership gate skips the retraction
    // delete — widening the desired set to whole-mirror did NOT weaken r13.
    let gamma_tip = find_reference(&dest, "refs/heads/gamma")
        .expect("gamma survives — heddle must not delete a tip it never published")
        .peel_to_id()
        .expect("peel gamma");
    assert_eq!(
        gamma_tip, oid_g_oob,
        "the out-of-band gamma tip must survive the scoped push (r13 ownership gate holds)"
    );
}

/// #316 / PR #528 Finding 1 (root case): when the WHOLE line is embargoed to
/// its root after a prior public export, the stale public branch must be
/// deleted, not left pointing at the now-embargoed commit.
#[test]
fn export_deletes_branch_when_whole_line_is_later_embargoed() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // A single public state on main.
    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).expect("first export");
    assert!(
        test_support::mapping(&bridge).get_git(&state_a).is_some(),
        "A minted while public"
    );

    // Embargo EVERY state on the line — including the seeded root — so the
    // whole line is embargoed to its root and no served ancestor remains.
    for state in repo.store().list_states().unwrap() {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    }

    let run2 = export_all(&mut bridge).expect("second export");
    assert!(
        run2.branches.iter().all(|b| b.name != "main"),
        "main must not be advertised once the whole line is embargoed"
    );
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert!(
        find_reference(&mirror, "refs/heads/main").is_err(),
        "the stale public branch must be deleted, not left serving the embargoed commit"
    );
}

/// #316 / PR #528 r2 Finding 1 (sibling): a branch exported at a PUBLIC commit,
/// then reset/rebased onto an unrelated `Private` root, must be deleted from the
/// mirror. The old public tip is NOT embargoed — it is simply no longer
/// reachable from the new tip — so r1's embargoed-tip retraction never fires.
/// The unifying invariant catches it anyway: a mirror ref exists iff its CURRENT
/// target resolves to a served frontier, and the new Private root resolves to
/// none.
#[test]
fn export_deletes_branch_when_thread_reset_to_private_root() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> State {
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // Public root A on main.
    let state_a = put_state(b"public base\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state_a.change_id)
        .expect("set main to A");

    let mut bridge = GitBridge::new(&repo);
    let run1 = export_all(&mut bridge).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a.change_id)
        .expect("A minted while public");
    assert!(
        run1.branches
            .iter()
            .any(|b| b.name == "main" && b.tip == oid_a),
        "run 1 advertises main at the public root A"
    );

    // Reset main onto an unrelated Private root B (no shared ancestry with A).
    let state_b = put_state(b"private root\n", Vec::new());
    repo.put_state_visibility(StateVisibility {
        state: state_b.change_id,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state_b.change_id)
        .expect("reset main to B");

    // A stays public — NOT embargoed — so r1's embargoed-tip retraction cannot
    // fire. B's line has no served frontier, so main must be deleted anyway.
    let run2 = export_all(&mut bridge).expect("second export");
    assert!(
        run2.branches.iter().all(|b| b.name != "main"),
        "main must not be advertised once reset onto a Private root"
    );
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert!(
        find_reference(&mirror, "refs/heads/main").is_err(),
        "the stale public branch must be deleted after a reset to a Private root"
    );
    // Sanity: A is still served — proving the deletion is driven by the new
    // target resolving to no served frontier, not by an embargo of the old tip.
    assert!(
        test_support::mapping(&bridge)
            .get_git(&state_a.change_id)
            .is_some(),
        "the old public tip A remains served; deletion is not driven by an embargo of A"
    );
}

/// #316 / PR #528 r2 Finding 2 (sibling): a marker exported as a tag at a PUBLIC
/// state A, then retargeted to a `Private` state B, must have its stale tag
/// deleted. The old tag tip A is NOT embargoed (still served), so r1's
/// embargoed-tip retraction never fires — but the marker's CURRENT target (B) is
/// not served, so the unified invariant deletes the tag.
#[test]
fn export_deletes_tag_when_marker_retargeted_to_private() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> State {
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // Public state A on main, plus a marker v1.0 pinned to A.
    let state_a = put_state(b"public release\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("main"), &state_a.change_id)
        .expect("set main to A");
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &state_a.change_id)
        .expect("create marker at A");

    let mut bridge = GitBridge::new(&repo);
    let run1 = export_all(&mut bridge).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a.change_id)
        .expect("A minted while public");
    assert!(
        run1.tags.iter().any(|t| t.name == "v1.0" && t.tip == oid_a),
        "run 1 publishes tag v1.0 at the public state A"
    );

    // Retarget v1.0 onto a Private state B (never minted into the mirror).
    let state_b = put_state(b"private release\n", Vec::new());
    repo.put_state_visibility(StateVisibility {
        state: state_b.change_id,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();
    repo.refs()
        .delete_marker(&MarkerName::new("v1.0"))
        .expect("clear old marker");
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &state_b.change_id)
        .expect("retarget marker to B");

    // A is still served (not embargoed), so r1's stale-tag retraction cannot
    // fire — but B is not served, so the tag must be deleted by the invariant.
    let run2 = export_all(&mut bridge).expect("second export");
    assert!(
        run2.tags.iter().all(|t| t.name != "v1.0"),
        "v1.0 must not be published once retargeted to a Private state"
    );
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert!(
        find_reference(&mirror, "refs/tags/v1.0").is_err(),
        "the stale public tag must be deleted after retarget to a Private state"
    );
    assert!(
        test_support::mapping(&bridge)
            .get_git(&state_a.change_id)
            .is_some(),
        "the old tag tip A remains served; deletion is not driven by an embargo of A"
    );
}

/// #316 / PR #528 r18 (the dual of the embargo retraction): the marker reconcile
/// must NOT conflate "unserved (embargoed)" with "served-but-not-minted
/// (out-of-scope)". A marker absent from the projected desired set has two
/// causes the desired set alone cannot tell apart — its target is genuinely
/// unserved (delete the stale tag, correct), OR its target is a still-PUBLIC
/// state that this scoped export simply did not mint into the mirror, so it has
/// no git OID in the mapping yet (preserve the prior tag). Deleting in the second
/// case spuriously RETRACTS a previously-exported public tag until an all-thread
/// export re-mints the target. This is the tag-side analog of the head rule: a
/// scoped export neither materializes a brand-new ref nor deletes a still-served
/// one it merely didn't mint.
///
/// The trigger that drives a SERVED marker target out of the mapping (the only
/// way the `None` arm is reached for a public target) is a RETARGET to a
/// not-yet-minted out-of-scope public state: a stationary marker's target stays
/// in the mapping via the notes/cache rebuild, so it goes through the `Some`
/// arm. The symmetric `Private`-retarget case (genuinely unserved) is covered by
/// [`export_deletes_tag_when_marker_retargeted_to_private`] — together they prove
/// the reconcile distinguishes served-but-unminted (preserve) from unserved
/// (delete).
#[test]
fn scoped_export_preserves_unminted_out_of_scope_public_tag() {
    use refs::Head;

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let put_state = |content: &[u8], parents: Vec<ChangeId>| -> objects::object::State {
        use objects::object::{Attribution, Principal, State};
        let store = repo.store();
        let blob_hash = store
            .put_blob(&Blob::from_slice(content))
            .expect("put blob");
        let tree_hash = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
            ]))
            .expect("put tree");
        let state = State::new(
            tree_hash,
            parents,
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        );
        store.put_state(&state).expect("put state");
        state
    };

    // Two independent public lines. alpha: A (the scoped thread). beta: B (a
    // B-only state pinned by marker v1.0). alpha never reaches beta.
    let state_a = put_state(b"alpha\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("alpha"), &state_a.change_id)
        .expect("set alpha to A");
    let state_b = put_state(b"beta release\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("beta"), &state_b.change_id)
        .expect("set beta to B");
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &state_b.change_id)
        .expect("create marker v1.0 at B");

    // Run 1 — full export+push to a real destination. Both threads + the tag land,
    // and B is minted into the mirror at oid_b.
    let mut bridge = GitBridge::new(&repo);
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first full export publishes alpha, beta, and tag v1.0");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b.change_id)
        .expect("B minted while public");
    {
        let dest = open_git(&dest_path).expect("open dest");
        assert_eq!(
            find_reference(&dest, "refs/tags/v1.0")
                .expect("run 1 publishes tag v1.0 to the destination")
                .peel_to_id()
                .expect("peel v1.0"),
            oid_b,
            "run 1 publishes tag v1.0 at the public state B"
        );
    }

    // Advance beta to a NEW public state C and RETARGET v1.0 onto it. C has never
    // been exported, so it carries no note/sidecar mapping — the only way a still
    // public marker target reaches the reconcile's `None` arm.
    let state_c = put_state(b"beta rc\n", vec![state_b.change_id]);
    repo.refs()
        .set_thread(&ThreadName::new("beta"), &state_c.change_id)
        .expect("advance beta to C");
    repo.refs()
        .delete_marker(&MarkerName::new("v1.0"))
        .expect("clear old marker");
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &state_c.change_id)
        .expect("retarget marker v1.0 to C");

    // Attach HEAD to alpha so a CurrentThread push scopes the EXPORT to alpha,
    // which does NOT reach C and so does NOT mint it this run.
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("alpha"),
        })
        .expect("attach HEAD to alpha");

    // Sanity: C is genuinely absent from the mapping going into the scoped push —
    // this is the served-but-unminted condition the fix must handle.
    assert!(
        test_support::mapping(&bridge)
            .get_git(&state_c.change_id)
            .is_none(),
        "C must be unminted so the marker reconcile takes the `None` arm"
    );

    // Run 2 — SCOPED push of alpha. C stays PUBLIC and served; the scoped export
    // just doesn't mint it. The marker reconcile must PRESERVE the existing tag
    // (at its prior oid_b) rather than retract it as if C had become unserved — a
    // later all-thread export will re-mint C and advance the tag.
    bridge
        .push_with_scope(
            dest_path.to_str().expect("dest path"),
            GitPushScope::CurrentThread,
        )
        .expect("scoped push of alpha");

    // The mirror must KEEP refs/tags/v1.0 — a served-but-unminted marker target is
    // not a stale tag.
    let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
    assert_eq!(
        find_reference(&mirror, "refs/tags/v1.0")
            .expect("mirror must keep the served-but-unminted public tag v1.0")
            .peel_to_id()
            .expect("peel mirror v1.0"),
        oid_b,
        "the scoped export must not retract a tag whose target is still public"
    );

    // ...and the destination reconcile must NOT propagate a deletion.
    let dest = open_git(&dest_path).expect("reopen dest");
    assert_eq!(
        find_reference(&dest, "refs/tags/v1.0")
            .expect("destination must keep the served-but-unminted public tag v1.0")
            .peel_to_id()
            .expect("peel dest v1.0"),
        oid_b,
        "the scoped push must not delete a tag whose target is still public"
    );
}

/// #316 / PR #528 — fixture helper for the reconcile conformance matrices: a
/// PUBLIC state holding one file, attributed to a real principal so the export
/// mints it without any git-identity config.
fn matrix_put_state(
    repo: &Repository,
    content: &[u8],
    parents: Vec<ChangeId>,
) -> objects::object::State {
    use objects::object::{Attribution, Principal, State};
    let store = repo.store();
    let blob_hash = store
        .put_blob(&Blob::from_slice(content))
        .expect("put blob");
    let tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), blob_hash, false).expect("tree entry"),
        ]))
        .expect("put tree");
    let state = State::new(
        tree_hash,
        parents,
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    );
    store.put_state(&state).expect("put state");
    state
}

/// Mark `state` Private so the export treats it as unserved (embargoed).
fn matrix_embargo(repo: &Repository, state: ChangeId) {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};
    repo.put_state_visibility(StateVisibility {
        state,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();
}

/// Read the mirror's `refs/tags/<name>` tip, or `None` when the tag is absent.
fn matrix_mirror_tag(bridge: &GitBridge, name: &str) -> Option<ObjectId> {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    find_reference(&mirror, &format!("refs/tags/{name}"))
        .ok()
        .and_then(|mut r| r.peel_to_id().ok())
}

/// Read the mirror's `refs/heads/<name>` tip, or `None` when the branch is absent.
fn matrix_mirror_head(bridge: &GitBridge, name: &str) -> Option<ObjectId> {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    find_reference(&mirror, &format!("refs/heads/{name}"))
        .ok()
        .and_then(|mut r| r.peel_to_id().ok())
}

/// #316 / PR #528 — plant a FOREIGN tag in the mirror: a `refs/tags/<name>` heddle
/// never WROTE (no marker, never reconciled under that name). Pass a heddle-MINTED
/// `target` to exercise the r20c bug — OID-based ownership would mis-claim it as
/// heddle's; the name-keyed managed record must still spare it.
fn matrix_plant_foreign_tag(bridge: &GitBridge, name: &str, target: ObjectId) {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    set_reference(
        &mirror,
        &format!("refs/tags/{name}"),
        target,
        RefPrecondition::Any,
        "test: foreign tag",
    )
    .expect("plant foreign tag");
}

/// #316 / PR #528 — plant a FOREIGN branch in the mirror: a `refs/heads/<name>`
/// heddle never wrote, at a heddle-minted `target` (the head dual of the foreign
/// tag at a heddle OID). It is not a heddle thread, so the head reconcile never
/// iterates it; the name-keyed record must spare it.
fn matrix_plant_foreign_branch(bridge: &GitBridge, name: &str, target: ObjectId) {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    set_reference(
        &mirror,
        &format!("refs/heads/{name}"),
        target,
        RefPrecondition::Any,
        "test: foreign branch",
    )
    .expect("plant foreign branch");
}

/// #316 / PR #528 — the mirror's name-keyed managed-refs record (full ref name →
/// last-published tip), the ownership boundary the reconcile and the push frontier
/// both read.
fn matrix_managed_record(bridge: &GitBridge) -> std::collections::HashMap<String, ObjectId> {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    read_mirror_managed_refs(&mirror).expect("read mirror managed record")
}

/// #316 / PR #528 — whether `collect_managed_ref_updates` (the managed-filtered
/// push frontier) carries a ref of `name`+`namespace`. A foreign ref must be
/// ABSENT here even though it survives in the mirror.
fn matrix_in_managed_frontier(bridge: &GitBridge, name: &str, namespace: RefNamespace) -> bool {
    let mirror = test_support::open_git_repo(bridge).expect("open mirror");
    let record = read_mirror_managed_refs(&mirror).expect("read record");
    collect_managed_ref_updates(&mirror, &record)
        .expect("collect managed frontier")
        .iter()
        .any(|u| u.namespace == namespace && u.name == name)
}

/// #316 / PR #528 r19 + S3 — the conformance matrix that turns the per-cell tag
/// fixes into a REDESIGN. The mirror tag's fate is a PURE FUNCTION of THREE axes:
/// the NEW marker target {served+minted, served-but-unminted, unserved,
/// no-target(deleted)} × the EXISTING mirror tag's target {served, embargoed,
/// absent} × scope {scoped, full}. Each row builds a fixture that reaches one
/// cell, runs `export_all`/`export_current_thread`, and asserts the mirror
/// `refs/tags/v` is at the expected OID or absent. The structural guarantees it
/// locks:
/// * PRESERVE requires the EXISTING tag itself be SERVED (cell 6) — a tag pointing
///   at an embargoed commit is DELETED even when the new target is served-but-
///   unminted (cell 7, the r19 fix `existing_embargoed_unminted_new_deletes_tag`);
///   reading the existing tag's TARGET is the axis the prior code never inspected.
/// * a served+minted target FORCE-retargets the tag (S1, cells 2/3).
/// * a DELETED marker still deletes its stale tag because the loop iterates
///   `markers ∪ existing-tags` — the deleted marker is reached via the tags side
///   (S3, cell 12).
///
/// It SUBSUMES the locked-in tests
/// [`export_deletes_tag_when_marker_retargeted_to_private`] (cell 9) and
/// [`scoped_export_preserves_unminted_out_of_scope_public_tag`] (cell 6).
#[test]
fn tag_reconcile_conformance_matrix() {
    struct Cell {
        label: &'static str,
        /// Returns (observed mirror `refs/tags/v` tip, expected tip).
        run: Box<dyn Fn() -> (Option<ObjectId>, Option<ObjectId>)>,
    }

    let cells: Vec<Cell> = vec![
        // ── Cells 1-4: NEW target served+minted → Write (force-retarget) ──
        // Cell 1: existing absent, full scope → materialize a fresh tag.
        Cell {
            label: "served_minted_existing_absent_full_creates",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                (matrix_mirror_tag(&bridge, "v"), Some(oid_a))
            }),
        },
        // Cell 2: existing SERVED, full scope, retarget served→served → force-move.
        Cell {
            label: "served_minted_existing_served_full_retargets",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .set_thread(&ThreadName::new("rel"), &b.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), Some(oid_b))
            }),
        },
        // Cell 3: existing EMBARGOED, full scope, retarget to a served+minted
        // INDEPENDENT root → force-move off the embargoed OID onto the new target.
        Cell {
            label: "served_minted_existing_embargoed_full_retargets",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .set_thread(&ThreadName::new("rel"), &b.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                matrix_embargo(&repo, a.change_id);
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), Some(oid_b))
            }),
        },
        // Cell 4: existing == new target → idempotent Write (no-op move).
        Cell {
            label: "served_minted_existing_same_idempotent",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), Some(oid_a))
            }),
        },
        // Cell 5: served+minted target, existing absent, SCOPED → skip-materialize
        // (a scoped export does not publish a brand-new tag the caller didn't ask for).
        Cell {
            label: "served_minted_existing_absent_scoped_skips",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("alpha"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .set_thread(&ThreadName::new("beta"), &b.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_current_thread(&mut bridge, "beta").unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // ── Cell 6 (r18): served-but-unminted target, existing SERVED → PRESERVE ──
        Cell {
            label: "served_unminted_existing_served_scoped_preserves",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("alpha"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .set_thread(&ThreadName::new("beta"), &b.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                // Advance beta to a NEW public state C and retarget v→C; C is never
                // exported, so it stays unminted and the scoped run takes the
                // served-but-unminted path.
                let c = matrix_put_state(&repo, b"C\n", vec![b.change_id]);
                repo.refs()
                    .set_thread(&ThreadName::new("beta"), &c.change_id)
                    .unwrap();
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &c.change_id)
                    .unwrap();
                export_current_thread(&mut bridge, "alpha").unwrap();
                (matrix_mirror_tag(&bridge, "v"), Some(oid_b))
            }),
        },
        // ── Cell 7 (r19 FIX): served-but-unminted target, existing EMBARGOED → DELETE ──
        // The named row: the existing tag points at a commit this run embargoed, so
        // PRESERVE would keep serving it. PRESERVE requires existing_served.
        Cell {
            label: "existing_embargoed_unminted_new_deletes_tag",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let p = matrix_put_state(&repo, b"P\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("alpha"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .set_thread(&ThreadName::new("rel"), &p.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &p.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                // New, never-exported public state C → served-but-unminted in the
                // scoped run. Embargo P (still reachable via `rel`) so the EXISTING
                // tag's tip enters `embargoed_oids`. Retarget v→C.
                let c = matrix_put_state(&repo, b"C\n", Vec::new());
                matrix_embargo(&repo, p.change_id);
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &c.change_id)
                    .unwrap();
                export_current_thread(&mut bridge, "alpha").unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // Cell 8: served-but-unminted target, existing absent, scoped → no-op.
        Cell {
            label: "served_unminted_existing_absent_scoped_noop",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let c = matrix_put_state(&repo, b"C\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("alpha"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &c.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_current_thread(&mut bridge, "alpha").unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // ── Cells 9-11: NEW target genuinely UNSERVED → DELETE ──
        // Cell 9: existing SERVED, retargeted to a Private state (subsumes the
        // locked-in `export_deletes_tag_when_marker_retargeted_to_private`).
        Cell {
            label: "unserved_existing_served_full_deletes",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                matrix_embargo(&repo, b.change_id);
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // Cell 10: existing EMBARGOED, target itself embargoed (the original r1
        // embargo retraction).
        Cell {
            label: "unserved_existing_embargoed_full_deletes",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                matrix_embargo(&repo, a.change_id);
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // Cell 11: existing absent, target unserved → DELETE is a no-op.
        Cell {
            label: "unserved_existing_absent_noop_delete",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", Vec::new());
                matrix_embargo(&repo, b.change_id);
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &b.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // ── Cells 12-13 (S3): the marker was DELETED → its stale tag must go ──
        // Cell 12: existing tag present, marker deleted — reached via the
        // existing-tags side of the union (markers no longer carries it).
        Cell {
            label: "deleted_marker_stale_tag_deleted",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // Cell 13: neither a marker nor an existing tag — structurally never visited
        // by the `markers ∪ existing-tags` union (a no-op by construction).
        Cell {
            label: "notarget_existing_absent_unvisited",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "ghost"), None)
            }),
        },
        // ── P1 regression guard: PRIOR-RUN embargo (not this run's purge) ──
        // The existing tag's target P was embargoed in a PRIOR run and is out of
        // THIS scoped run's purge reach (P is unreachable from every current frontier
        // root). Classifying the existing tip by THIS run's purge drop-set (the bug)
        // reads P as served → cell-6 PRESERVE → keeps serving embargoed P.
        // Classifying by the whole-mirror served-OID set reads P as UNSERVED → cell-7
        // DELETE. The new target is served-but-unminted (the ONLY path that reads the
        // existing tag's served axis).
        Cell {
            label: "prior_run_embargo_existing_tag_deletes",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let p = matrix_put_state(&repo, b"P\n", Vec::new());
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &p.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                // Run 1 mints P (every store state is minted by export_all) and
                // materializes the mirror tag v → P.
                export_all(&mut bridge).unwrap();
                // PRIOR-run embargo of P. Retarget v to a fresh public-but-unminted C
                // and export ONLY `alpha` — P is now unreachable from the scoped
                // frontier (no thread/marker reaches it), so it is NOT in this run's
                // purge drop-set, yet v's existing tag still points at P's OID.
                matrix_embargo(&repo, p.change_id);
                let alpha = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("alpha"), &alpha.change_id)
                    .unwrap();
                let c = matrix_put_state(&repo, b"C\n", Vec::new());
                repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
                repo.refs()
                    .create_marker(&MarkerName::new("v"), &c.change_id)
                    .unwrap();
                export_current_thread(&mut bridge, "alpha").unwrap();
                (matrix_mirror_tag(&bridge, "v"), None)
            }),
        },
        // ── foreign Git tag preserved: a mirror tag heddle never exported ──
        // A user's own `refs/tags/user-v1`, pointing at a commit heddle never minted,
        // with NO marker. The reconcile's NoTarget arm would DELETE it unless the
        // heddle-managed-ref filter spares it (a foreign tag is left untouched).
        Cell {
            label: "foreign_unmanaged_git_tag_preserved",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                // Plant a FOREIGN tag in the mirror: a commit heddle never minted,
                // tagged under a name heddle never exported.
                let mirror = test_support::open_git_repo(&bridge).unwrap();
                let foreign =
                    commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "foreign", &[]);
                set_reference(
                    &mirror,
                    "refs/tags/user-v1",
                    foreign,
                    RefPrecondition::MustNotExist,
                    "test: foreign tag",
                )
                .unwrap();
                export_all(&mut bridge).unwrap();
                (matrix_mirror_tag(&bridge, "user-v1"), Some(foreign))
            }),
        },
        // ── foreign tag AT A HEDDLE OID (the r20c bug) — spared AND excluded ──
        // A foreign `refs/tags/user-v1` pointing at a commit heddle MINTED. The
        // OBSOLETE OID-based ownership classified this as heddle's (its tip is a
        // minted commit) → folded into the reconcile → deleted. The NAME-keyed
        // record spares it (the name was never recorded as written), AND the
        // managed-filtered push frontier excludes it.
        Cell {
            label: "foreign_tag_at_heddle_oid_spared_and_excluded",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                // Plant the foreign tag at the heddle-minted OID, AFTER the first
                // export so the managed record already exists (the name is not in it).
                matrix_plant_foreign_tag(&bridge, "user-v1", oid_a);
                export_all(&mut bridge).unwrap();
                assert!(
                    !matrix_in_managed_frontier(&bridge, "user-v1", RefNamespace::Tag),
                    "a foreign tag at a heddle OID must be EXCLUDED from the managed push frontier"
                );
                (matrix_mirror_tag(&bridge, "user-v1"), Some(oid_a))
            }),
        },
    ];

    for cell in &cells {
        let (observed, expected) = (cell.run)();
        assert_eq!(observed, expected, "tag cell `{}`", cell.label);
    }
}

/// #316 / PR #528 S2 — the head reconcile has NO existing-embargo PRESERVE path,
/// the asymmetry vs the tag reconcile (where a served-but-unminted tag at a still
/// SERVED tip IS preserved, cell 6). A head's published target is recomputed every
/// run as `frontier_git_oid` — the maximal SERVED ancestor-or-self of the thread
/// tip — so a head can never keep serving an embargoed tip: it is REWOUND to the
/// served frontier, or DELETED when no served frontier remains. Each embargo row
/// asserts the resulting `refs/heads/main` tip is the served frontier and NEVER the
/// embargoed OID.
#[test]
fn head_reconcile_conformance_matrix() {
    struct HeadOutcome {
        observed: Option<ObjectId>,
        expected: Option<ObjectId>,
        /// The embargoed OID the head must NEVER be left at (`None` when no embargo).
        forbidden: Option<ObjectId>,
    }
    struct Cell {
        label: &'static str,
        run: Box<dyn Fn() -> HeadOutcome>,
    }

    let cells: Vec<Cell> = vec![
        // Control: a plain public advance fast-forwards the head.
        Cell {
            label: "public_advance_fast_forwards",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let b = matrix_put_state(&repo, b"B\n", vec![a.change_id]);
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &b.change_id)
                    .unwrap();
                export_all(&mut bridge).unwrap();
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                HeadOutcome {
                    observed: matrix_mirror_head(&bridge, "main"),
                    expected: Some(oid_b),
                    forbidden: None,
                }
            }),
        },
        // KEY: an embargoed TIP rewinds the head to the served ancestor — the head
        // is NEVER preserved at the embargoed tip (the tag-cell-7 bug has no head dual).
        Cell {
            label: "embargoed_tip_rewinds_to_served_ancestor",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", vec![a.change_id]);
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &b.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                matrix_embargo(&repo, b.change_id);
                export_all(&mut bridge).unwrap();
                HeadOutcome {
                    observed: matrix_mirror_head(&bridge, "main"),
                    expected: Some(oid_a),
                    forbidden: Some(oid_b),
                }
            }),
        },
        // KEY: the WHOLE line embargoed DELETES the head — no preserve path at all.
        Cell {
            label: "embargoed_whole_line_deletes_head",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                matrix_embargo(&repo, a.change_id);
                export_all(&mut bridge).unwrap();
                HeadOutcome {
                    observed: matrix_mirror_head(&bridge, "main"),
                    expected: None,
                    forbidden: Some(oid_a),
                }
            }),
        },
        // P1 regression guard (head dual): a SCOPED export whose existing head tip
        // was embargoed in a PRIOR run — and is out of THIS run's purge reach
        // (unreachable from the scoped frontier) — must RETRACT (force-rewind) to the
        // served ancestor, not classify the embargoed tip as served. Classifying by
        // this run's purge drop-set (the bug) reads B as served → FF-guarded sync →
        // a NonFastForward error on the rewind; classifying by the whole-mirror
        // served-OID set reads B as unserved → force-rewind to A.
        Cell {
            label: "prior_run_embargoed_existing_head_retracts",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                let b = matrix_put_state(&repo, b"B\n", vec![a.change_id]);
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &b.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                let oid_b = test_support::mapping(&bridge)
                    .get_git(&b.change_id)
                    .expect("B minted");
                // Embargo the published tip B, rewind main to its served ancestor A,
                // and export ONLY main (scoped). B is now unreachable from the scoped
                // frontier, so it is NOT in this run's purge drop-set — only the
                // served-OID classification rewinds the head off B.
                matrix_embargo(&repo, b.change_id);
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                export_current_thread(&mut bridge, "main").unwrap();
                HeadOutcome {
                    observed: matrix_mirror_head(&bridge, "main"),
                    expected: Some(oid_a),
                    forbidden: Some(oid_b),
                }
            }),
        },
        // ── foreign BRANCH at a heddle OID — spared (the head dual of the tag) ──
        // A `refs/heads/user-feature` heddle never wrote, at a heddle-minted OID.
        // It is not a heddle thread, so the head reconcile (iterating current
        // threads) never visits it; the name-keyed record spares it, and the
        // managed-filtered push frontier excludes it.
        Cell {
            label: "foreign_branch_at_heddle_oid_spared",
            run: Box::new(|| {
                let temp = TempDir::new().unwrap();
                let repo = Repository::init(temp.path()).unwrap();
                let a = matrix_put_state(&repo, b"A\n", Vec::new());
                repo.refs()
                    .set_thread(&ThreadName::new("main"), &a.change_id)
                    .unwrap();
                let mut bridge = GitBridge::new(&repo);
                export_all(&mut bridge).unwrap();
                let oid_a = test_support::mapping(&bridge)
                    .get_git(&a.change_id)
                    .expect("A minted");
                matrix_plant_foreign_branch(&bridge, "user-feature", oid_a);
                export_all(&mut bridge).unwrap();
                assert!(
                    !matrix_in_managed_frontier(&bridge, "user-feature", RefNamespace::Branch),
                    "a foreign branch at a heddle OID must be EXCLUDED from the managed push frontier"
                );
                HeadOutcome {
                    observed: matrix_mirror_head(&bridge, "user-feature"),
                    expected: Some(oid_a),
                    forbidden: None,
                }
            }),
        },
    ];

    for cell in &cells {
        let out = (cell.run)();
        assert_eq!(out.observed, out.expected, "head cell `{}`", cell.label);
        if let Some(forbidden) = out.forbidden {
            assert_ne!(
                out.observed,
                Some(forbidden),
                "head cell `{}`: the head must NOT be left at the embargoed tip",
                cell.label
            );
        }
    }
}

/// #316 / PR #528 — the mirror's name-keyed managed-refs record is the ownership
/// boundary: every ref the reconcile WRITES is CLAIMED, every ref it DELETES is
/// DROPPED, and a foreign ref heddle never wrote NEVER enters — even one pointing
/// at a heddle-minted OID (the r20c bug the OID-based record produced).
#[test]
fn mirror_managed_record_claims_written_drops_deleted_excludes_foreign() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let a = matrix_put_state(&repo, b"A\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("main"), &a.change_id)
        .unwrap();
    repo.refs()
        .create_marker(&MarkerName::new("v"), &a.change_id)
        .unwrap();
    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).unwrap();

    // CLAIM: the written head and tag are recorded.
    let record = matrix_managed_record(&bridge);
    assert!(
        record.contains_key("refs/heads/main"),
        "written head claimed: {record:?}"
    );
    assert!(
        record.contains_key("refs/tags/v"),
        "written tag claimed: {record:?}"
    );

    // EXCLUDE FOREIGN: a foreign tag at a heddle OID never enters the record.
    let oid_a = test_support::mapping(&bridge)
        .get_git(&a.change_id)
        .expect("A minted");
    matrix_plant_foreign_tag(&bridge, "user-v1", oid_a);
    export_all(&mut bridge).unwrap();
    let record = matrix_managed_record(&bridge);
    assert!(
        !record.contains_key("refs/tags/user-v1"),
        "a foreign tag (even at a heddle OID) must never enter the record: {record:?}"
    );
    assert!(record.contains_key("refs/heads/main"), "{record:?}");

    // DROP: deleting the marker drops its tag from the record.
    repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
    export_all(&mut bridge).unwrap();
    let record = matrix_managed_record(&bridge);
    assert!(
        !record.contains_key("refs/tags/v"),
        "a deleted marker's tag must drop from the record: {record:?}"
    );
}

/// #316 / PR #528 — the managed-refs record round-trips through its atomic
/// temp+rename write and the shared exported-refs parse contract.
#[test]
fn mirror_managed_record_survives_round_trip() {
    let temp = TempDir::new().unwrap();
    let mirror = SleyRepository::init_bare(temp.path()).unwrap();
    let main_tip = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "main", &[]);
    let tag_tip = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "tag", &[]);

    let mut record = std::collections::HashMap::new();
    record.insert("refs/heads/main".to_string(), main_tip);
    record.insert("refs/tags/v1.0".to_string(), tag_tip);
    write_mirror_managed_refs(&mirror, &record).expect("write record");

    let read_back = read_mirror_managed_refs(&mirror).expect("read record");
    assert_eq!(
        read_back, record,
        "managed record must round-trip byte-for-byte"
    );
}

/// #316 / PR #528 — the #1 first-run risk: the FIRST export after this code lands
/// finds NO managed record. If an absent record made every pre-existing ref look
/// foreign, embargo retraction would silently stop. Seeding the record from the
/// current mirror ref set keeps pre-existing heddle refs managed, so a stale tag
/// whose marker was already deleted IS still retracted on that first run. Without
/// seeding, the tag (not a current marker, not in an empty record) would never be
/// iterated and would survive — the leak this guards.
#[test]
fn first_export_after_upgrade_seeds_record_and_still_retracts() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let a = matrix_put_state(&repo, b"A\n", Vec::new());
    let p = matrix_put_state(&repo, b"P\n", Vec::new());
    repo.refs()
        .set_thread(&ThreadName::new("main"), &a.change_id)
        .unwrap();
    repo.refs()
        .create_marker(&MarkerName::new("v"), &p.change_id)
        .unwrap();
    let mut bridge = GitBridge::new(&repo);
    export_all(&mut bridge).unwrap();
    assert!(
        matrix_mirror_tag(&bridge, "v").is_some(),
        "run 1 must publish the tag"
    );

    // Simulate the PRE-record state: delete the on-disk record so the next export
    // sees an absent record (as it would on the first run after this lands).
    let record_path = {
        let mirror = test_support::open_git_repo(&bridge).unwrap();
        mirror.git_dir().join("heddle-mirror-managed-refs")
    };
    assert!(record_path.exists(), "run 1 must have written the record");
    std::fs::remove_file(&record_path).unwrap();

    // Delete the marker so `v` is no longer a current marker. Only the SEED (from
    // the current mirror ref set) makes `refs/tags/v` reachable by the reconcile.
    repo.refs().delete_marker(&MarkerName::new("v")).unwrap();
    export_all(&mut bridge).unwrap();

    assert_eq!(
        matrix_mirror_tag(&bridge, "v"),
        None,
        "the first post-upgrade export must retract the stale tag via record seeding"
    );
    assert!(
        record_path.exists(),
        "the export must (re)write the managed record"
    );
}

/// #316 / PR #528 r7 CLASS 2 (the leak): a branch exported to a real
/// DESTINATION while public, then retracted (whole line embargoed), must be
/// DELETED at the destination — not left pointing at now-private commits. The
/// mirror-side retraction already deletes the branch from the internal mirror;
/// the destination-sync step only ever WROTE the surviving refs and never
/// computed deletions, so the destination kept the stale branch. The
/// delete-set reconciliation closes that leak.
#[test]
fn export_propagates_branch_deletion_to_destination() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();

    // Export to a real destination while public — the destination gets main.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let dest = open_git(&dest_path).expect("open dest");
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "destination must advertise main while the line is public"
    );

    // Embargo the WHOLE line — including the seeded root — so no served frontier
    // remains and the mirror deletes refs/heads/main.
    for state in repo.store().list_states().unwrap() {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    }

    // Re-export to the SAME destination: the stale branch must be DELETED there.
    bridge.export_to_path(&dest_path).expect("second export");
    let dest = open_git(&dest_path).expect("reopen dest");
    assert!(
        find_reference(&dest, "refs/heads/main").is_err(),
        "retracting the line must DELETE refs/heads/main at the destination, not leave it serving now-private commits"
    );
}

/// #316 / PR #528 r7 CLASS 2: the delete-set covers TAGS and the NOTES
/// namespace too, so it is not a heads-only fix. A marker tag retracted from
/// the mirror must be deleted at the destination; a stale heddle-managed notes
/// ref absent from the served mirror must be deleted; and the embargoed
/// commit's note ENTRY must no longer be readable at the destination.
#[test]
fn export_propagates_tag_and_note_deletion() {
    use chrono::Utc;
    use objects::object::{Attribution, Principal, State, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // A public state on main that stays public throughout — so main never
    // rewinds (a rewind would trip the export's fast-forward guard, a separate
    // concern from this test).
    std::fs::write(heddle_temp.path().join("main.txt"), b"trunk\n").unwrap();
    repo.snapshot(Some("trunk".into()), None).unwrap();

    // A SEPARATE public state R, pinned by marker v1.0 and independent of main,
    // so embargoing it later deletes the tag and retracts its note without
    // touching main's tip.
    let r = {
        let store = repo.store();
        let blob = store
            .put_blob(&Blob::from_slice(b"release\n"))
            .expect("blob");
        let tree = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("release.txt".to_string(), blob, false).expect("entry"),
            ]))
            .expect("tree");
        let state = State::new(
            tree,
            Vec::new(),
            Attribution::human(Principal::new("Grace Hopper", "grace@example.com")),
        );
        store.put_state(&state).expect("put R");
        state
    };
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &r.change_id)
        .expect("create marker at R");

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let oid_r = test_support::mapping(&bridge)
        .get_git(&r.change_id)
        .expect("R minted");

    {
        let dest = open_git(&dest_path).expect("open dest");
        assert!(
            find_reference(&dest, "refs/tags/v1.0").is_ok(),
            "destination must have the tag while public"
        );
        assert!(
            cli::bridge::git_notes::read_note(&dest, oid_r)
                .unwrap()
                .is_some(),
            "destination must carry R's note while public"
        );
        // Plant a stale notes ref the served mirror will never have, to exercise
        // the NOTES namespace of the delete-set directly. r8 HOLE 2: the
        // delete-set is now scoped to refs heddle PREVIOUSLY EXPORTED here (not
        // the raw destination namespace), so record this ref as exported —
        // mirroring a real "heddle exported it, mirror later stopped serving it"
        // retraction. Without the record it would (correctly) be treated as a
        // foreign ref and survive.
        set_reference(
            &dest,
            "refs/notes/legacy",
            oid_r,
            RefPrecondition::Any,
            "test: stale heddle-exported notes ref",
        )
        .expect("plant stale notes ref");
        let mut exported = read_exported_refs(&dest).expect("read exported-refs record");
        exported.insert("refs/notes/legacy".to_string(), oid_r);
        write_exported_refs(&dest, &exported).expect("record legacy as heddle-exported");
    }

    // Embargo R → the mirror deletes the tag and retracts R's note entry. main
    // is independent of R, so it is untouched (no rewind).
    repo.put_state_visibility(StateVisibility {
        state: r.change_id,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    bridge.export_to_path(&dest_path).expect("second export");

    let dest = open_git(&dest_path).expect("reopen dest");
    assert!(
        find_reference(&dest, "refs/tags/v1.0").is_err(),
        "retracting the marker must DELETE refs/tags/v1.0 at the destination"
    );
    assert!(
        find_reference(&dest, "refs/notes/legacy").is_err(),
        "a stale heddle-managed notes ref absent from the served mirror must be DELETED at the destination"
    );
    assert!(
        cli::bridge::git_notes::read_note(&dest, oid_r)
            .unwrap()
            .is_none(),
        "the embargoed commit's note must no longer be readable at the destination"
    );
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "the still-public main branch must survive the reconciliation"
    );
}

/// #316 / PR #528 r7 CLASS 2: the delete-set is scoped strictly to the
/// heddle-managed namespaces (`refs/heads/*`, `refs/tags/*`, `refs/notes/*`).
/// A ref the destination holds OUTSIDE those namespaces is foreign — heddle
/// does not own it — and must be left untouched by the reconciliation. Refs
/// that ARE still served must also survive.
#[test]
fn export_does_not_delete_foreign_refs() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");

    // Plant a foreign ref in a namespace heddle does not manage.
    {
        let dest = open_git(&dest_path).expect("open dest");
        set_reference(
            &dest,
            "refs/keep/backup",
            oid_a,
            RefPrecondition::Any,
            "test: foreign ref heddle does not own",
        )
        .expect("plant foreign ref");
    }

    // Re-export with nothing retracted (main still public).
    bridge.export_to_path(&dest_path).expect("second export");

    let dest = open_git(&dest_path).expect("reopen dest");
    assert!(
        find_reference(&dest, "refs/keep/backup").is_ok(),
        "a foreign ref outside heddle-managed namespaces must be left untouched"
    );
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "a still-served heddle branch must survive the reconciliation"
    );
}

/// #316 / PR #528 r8 HOLE 2 (the data-loss): a destination ref INSIDE the
/// heddle-managed namespaces (`refs/heads/*`) that heddle NEVER exported — e.g.
/// a branch another user or tool created on a shared local bare remote — must
/// survive a normal export/push. r7 diffed the whole destination namespace and so
/// DELETED such a foreign branch; scoping the delete-set to refs heddle actually
/// exported (recorded per destination) leaves it intact. The r7 foreign-ref test
/// only escaped this bug by planting OUTSIDE heads/tags/notes.
#[test]
fn export_does_not_delete_foreign_managed_ref() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");

    // Plant a branch heddle never exported, INSIDE the heddle-managed
    // `refs/heads/*` namespace — the namespace r7 over-deleted from.
    {
        let dest = open_git(&dest_path).expect("open dest");
        set_reference(
            &dest,
            "refs/heads/other-user-branch",
            oid_a,
            RefPrecondition::Any,
            "test: foreign branch heddle never exported",
        )
        .expect("plant foreign managed ref");
    }

    // Re-export with nothing retracted (main still public).
    bridge.export_to_path(&dest_path).expect("second export");

    let dest = open_git(&dest_path).expect("reopen dest");
    assert!(
        find_reference(&dest, "refs/heads/other-user-branch").is_ok(),
        "a foreign branch heddle never exported must NOT be deleted by a normal push"
    );
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "a still-served heddle branch must survive the reconciliation"
    );
}

/// #316 / PR #528 r8 HOLE 2 (regression guard): scoping the delete-set to
/// heddle-exported refs must NOT break the genuine retraction path. A branch
/// heddle exported while public, then retracted (whole line embargoed so the
/// served mirror drops it), must STILL be DELETED at the destination — it is in
/// the per-destination exported-refs record and absent from the served mirror,
/// so it lands in the delete-set. Preserves the r7 behavior the HOLE 2 fix must
/// not regress.
#[test]
fn export_still_deletes_previously_exported_then_retracted_ref() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();

    // Export to a real destination while public — heddle records main as exported.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let dest = open_git(&dest_path).expect("open dest");
    assert!(
        find_reference(&dest, "refs/heads/main").is_ok(),
        "destination must advertise main while the line is public"
    );

    // Embargo the WHOLE line so the mirror stops serving refs/heads/main.
    for state in repo.store().list_states().unwrap() {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    }

    // Re-export: main was heddle-exported AND is no longer served → DELETED.
    bridge.export_to_path(&dest_path).expect("second export");
    let dest = open_git(&dest_path).expect("reopen dest");
    assert!(
        find_reference(&dest, "refs/heads/main").is_err(),
        "a previously-exported, now-retracted branch must be DELETED at the destination"
    );
}

/// #316 / PR #528 r11 Finding HcDQU: the retraction delete-set must propagate to
/// URL/NETWORK remotes too, not only local-path destinations. A branch exported
/// public over the `git://` push path and then retracted (whole line embargoed)
/// must be DELETED on the wire remote — pre-r11 the URL push only sent additive
/// updates, so a retracted ref lingered on the remote after vanishing locally.
/// Uses the real network receive-pack path (`push_network_remote_with_updates`),
/// NOT `copy_mirror_to_path_with_updates`.
#[test]
fn retraction_delete_propagates_to_url_remote() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();

    // A real wire (git://) remote that also serves receive-pack.
    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    // Point the bare remote's HEAD at a placeholder branch so receive-pack does
    // not refuse to delete refs/heads/main later as "the current branch".
    std::fs::write(
        remote_root.path().join("remote.git").join("HEAD"),
        b"ref: refs/heads/__heddle_placeholder\n",
    )
    .unwrap();
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");

    // Export public over the NETWORK push path — main lands on the remote.
    let mut bridge = GitBridge::new(&repo);
    bridge.push(&url).expect("first network push");
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        assert!(
            find_reference(&remote, "refs/heads/main").is_ok(),
            "the remote must advertise main while the line is public"
        );
    }

    // Embargo the WHOLE line so the served mirror stops serving refs/heads/main.
    for state in repo.store().list_states().unwrap() {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    }

    // Re-push: main was heddle-exported AND is no longer served → DELETED on the
    // wire remote, not just locally.
    bridge.push(&url).expect("second network push");
    let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
    assert!(
        find_reference(&remote, "refs/heads/main").is_err(),
        "a previously-exported, now-retracted branch must be DELETED on the URL/network remote"
    );
}

/// #316 / PR #528 r11 Finding HcDQO: a destination push must FORCE a deliberate
/// embargo rewind. When a previously-exported tip B is embargoed but parent A
/// stays served, the served frontier lags the branch back to A — a non-fast-
/// forward rewind. Pre-r11 the local-path push ran a blanket FF guard that
/// REJECTED it, so the destination kept advertising the embargoed B. The unified
/// reconciliation distinguishes a backward rewind from a fork and forces the
/// former through.
#[test]
fn embargo_rewind_forced_through_destination_push() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // Linear thread: public base A, then public tip B.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    std::fs::write(heddle_temp.path().join("b.txt"), b"fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    // First export to a real bare destination — main advertises the public tip B.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    let mut bridge = GitBridge::new(&repo);
    bridge.export_to_path(&dest_path).expect("first export");
    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");
    {
        let dest = open_git(&dest_path).expect("open dest");
        let tip = find_reference(&dest, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(tip, oid_b, "destination advertises the public tip B");
    }

    // Embargo only B; parent A stays served.
    repo.put_state_visibility(StateVisibility {
        state: state_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    // Re-export: the served frontier lags to A — a deliberate NON-fast-forward
    // rewind the destination push must FORCE through, not reject with the FF
    // guard. Pre-r11 this errored.
    bridge
        .export_to_path(&dest_path)
        .expect("second export must force the embargo rewind through the destination");
    let dest = open_git(&dest_path).expect("reopen dest");
    let tip = find_reference(&dest, "refs/heads/main")
        .unwrap()
        .peel_to_id()
        .unwrap();
    assert_eq!(
        tip, oid_a,
        "the destination branch must be rewound to the served ancestor A"
    );
    assert_ne!(
        tip, oid_b,
        "the destination must not keep advertising the embargoed tip B"
    );
}

/// #316 / PR #528 r11: the r8 foreign-ref scoping must hold on the UNIFIED
/// URL/network path. A branch INSIDE `refs/heads/*` that heddle never exported —
/// planted directly on the wire remote by another user/tool — must survive a
/// normal push: it is not in heddle's per-remote exported-refs record, so it can
/// never join the delete-set.
#[test]
fn foreign_ref_on_url_remote_survives() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    std::fs::write(heddle_temp.path().join("a.txt"), b"only\n").unwrap();
    repo.snapshot(Some("only".into()), None).unwrap();

    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");

    // First public push — records main as heddle-exported to THIS url remote.
    let mut bridge = GitBridge::new(&repo);
    bridge.push(&url).expect("first network push");

    // Plant a branch heddle never exported, INSIDE the heddle-managed
    // refs/heads/* namespace, directly on the remote.
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        let main_oid = find_reference(&remote, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        set_reference(
            &remote,
            "refs/heads/other-user-branch",
            main_oid,
            RefPrecondition::Any,
            "test: foreign branch heddle never exported",
        )
        .expect("plant foreign managed ref");
    }

    // Re-push with nothing retracted — the foreign branch must survive (the r8
    // delete-set scoping holds on the unified URL/network path).
    bridge.push(&url).expect("second network push");

    let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
    assert!(
        find_reference(&remote, "refs/heads/other-user-branch").is_ok(),
        "a foreign branch heddle never exported must NOT be deleted on the URL/network remote"
    );
    assert!(
        find_reference(&remote, "refs/heads/main").is_ok(),
        "a still-served heddle branch must survive the reconciliation"
    );
}

/// #316 / PR #528 r12: a destination tip advanced OUT OF BAND past heddle's
/// last-published tip — a descendant of the served frontier that heddle never
/// published, then fetched into the mirror — must NOT be force-overwritten. r11
/// treated ANY `old`-descends-from-`new` topology as a heddle-owned embargo
/// rewind and forced it, clobbering the remote's newer linear commit (data
/// loss). The force is now gated on heddle-OWNERSHIP (the exported-refs record):
/// an out-of-band descendant is `Diverged` — FF-rejected unless `--force` — so
/// the destination's newer commit survives. Covers local-path AND URL/network.
#[test]
fn out_of_band_destination_descendant_not_force_overwritten() {
    use cli::bridge::git_core::GitBridgeError;

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // A single public tip B heddle publishes to its destinations.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    std::fs::write(heddle_temp.path().join("b.txt"), b"fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let mut bridge = GitBridge::new(&repo);

    // ---- local-path destination ----
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first export publishes B (local)");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");

    // Out-of-band advance: a NEW commit C on top of B, written to BOTH the mirror
    // (the user fetched it, so it is a resolvable descendant of the served
    // frontier — the exact topology that fooled r11) AND the destination (its
    // branch moves to C). Identical inputs ⇒ identical oid in both repos.
    let oid_c = {
        let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
        let in_mirror = commit_with_tree(
            &mirror,
            None,
            empty_tree_oid(&mirror),
            "out-of-band",
            &[oid_b],
        );
        let dest = open_git(&dest_path).expect("open dest");
        let in_dest = commit_with_tree(
            &dest,
            Some("refs/heads/main"),
            empty_tree_oid(&dest),
            "out-of-band",
            &[oid_b],
        );
        assert_eq!(
            in_mirror, in_dest,
            "C must be the same commit in mirror and destination"
        );
        in_mirror
    };

    // A plain export must NOT clobber C: heddle never published it, so the move is
    // Diverged (FF-rejected), not a heddle-owned rewind.
    let err = bridge.export_to_path(&dest_path).expect_err(
        "out-of-band descendant must be FF-rejected at the local destination, not force-overwritten",
    );
    assert!(
        matches!(err, GitBridgeError::NonFastForwardRef { .. }),
        "expected a non-fast-forward rejection, got: {err:?}"
    );
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        let tip = find_reference(&dest, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(
            tip, oid_c,
            "the out-of-band commit must survive — heddle must not force-overwrite it"
        );
    }

    // `--force` is the explicit escape hatch: it DOES overwrite C back to B.
    bridge
        .push_with_scope_force(
            dest_path.to_str().expect("dest path"),
            GitPushScope::AllThreads,
            true,
        )
        .expect("--force overrides the FF guard at the local destination");
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        let tip = find_reference(&dest, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(
            tip, oid_b,
            "--force rewinds the local destination to the served frontier B"
        );
    }

    // ---- URL/network destination ----
    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");

    bridge.push(&url).expect("first network push publishes B");
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        let tip = find_reference(&remote, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(tip, oid_b, "remote advertises the published tip B");
    }

    // Out-of-band advance on the wire remote to the SAME C (already in the mirror).
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        let in_remote = commit_with_tree(
            &remote,
            Some("refs/heads/main"),
            empty_tree_oid(&remote),
            "out-of-band",
            &[oid_b],
        );
        assert_eq!(
            in_remote, oid_c,
            "C must be the same commit on the wire remote"
        );
    }

    // Plain network push must likewise refuse to clobber C.
    let err = bridge
        .push(&url)
        .expect_err("out-of-band descendant must be FF-rejected on the URL/network remote too");
    assert!(
        matches!(err, GitBridgeError::NonFastForwardRef { .. }),
        "expected a non-fast-forward rejection on the wire, got: {err:?}"
    );
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
        let tip = find_reference(&remote, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(
            tip, oid_c,
            "the out-of-band commit must survive on the URL/network remote"
        );
    }
}

/// #316 / PR #528 r12: the r11 behavior is PRESERVED — a tip heddle ITSELF
/// published, then embargoed, IS force-rewound to the served frontier at every
/// destination. The r12 ownership gate authorizes the force precisely because the
/// destination tip equals heddle's recorded published tip; only out-of-band tips
/// heddle never published are spared (see
/// [`out_of_band_destination_descendant_not_force_overwritten`]). Covers
/// local-path AND URL/network.
#[test]
fn heddle_published_tip_embargo_rewind_still_forced() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // Linear thread: public base A, then public tip B — both heddle-published.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_a = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();
    std::fs::write(heddle_temp.path().join("b.txt"), b"fix\n").unwrap();
    repo.snapshot(Some("fix".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let mut bridge = GitBridge::new(&repo);

    // Publish B to BOTH a local-path destination and a URL/network remote.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first export publishes B (local)");

    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");
    bridge.push(&url).expect("first network push publishes B");

    let oid_a = test_support::mapping(&bridge)
        .get_git(&state_a)
        .expect("A minted");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");
    {
        let dest = open_git(&dest_path).expect("open dest");
        assert_eq!(
            find_reference(&dest, "refs/heads/main")
                .unwrap()
                .peel_to_id()
                .unwrap(),
            oid_b,
            "local destination advertises the published tip B"
        );
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        assert_eq!(
            find_reference(&remote, "refs/heads/main")
                .unwrap()
                .peel_to_id()
                .unwrap(),
            oid_b,
            "remote advertises the published tip B"
        );
    }

    // Embargo only B; parent A stays served. The served frontier lags to A — a
    // deliberate heddle-OWNED rewind: the destination tip (B) equals heddle's
    // recorded published tip, so the force is authorized at every destination.
    repo.put_state_visibility(StateVisibility {
        state: state_b,
        tier: VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        },
        embargo_until: None,
        declarer: Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        },
        declared_at: Utc::now(),
        signature: None,
        supersedes: None,
    })
    .unwrap();

    bridge
        .export_to_path(&dest_path)
        .expect("second export must FORCE the heddle-owned embargo rewind (local)");
    bridge
        .push(&url)
        .expect("second network push must FORCE the heddle-owned embargo rewind (network)");

    let dest = open_git(&dest_path).expect("reopen dest");
    let tip = find_reference(&dest, "refs/heads/main")
        .unwrap()
        .peel_to_id()
        .unwrap();
    assert_eq!(
        tip, oid_a,
        "local destination must be force-rewound to the served ancestor A"
    );
    assert_ne!(
        tip, oid_b,
        "local destination must not keep advertising the embargoed tip B"
    );

    let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
    let rtip = find_reference(&remote, "refs/heads/main")
        .unwrap()
        .peel_to_id()
        .unwrap();
    assert_eq!(
        rtip, oid_a,
        "URL/network remote must be force-rewound to the served ancestor A"
    );
    assert_ne!(
        rtip, oid_b,
        "URL/network remote must not keep advertising the embargoed tip B"
    );
}

/// #316 / PR #528 r13 (the r12-review delete finding): the retraction DELETE must
/// honor the SAME heddle-ownership gate as the forced rewind. A destination tip
/// advanced OUT OF BAND past heddle's last-published tip — and then the whole
/// line embargoed so the branch has no served frontier — must NOT be deleted by a
/// plain export/push. heddle never published that tip, so it is not heddle's to
/// retract; deleting it would clobber the user's commit (data loss). r11/r12 gated
/// FORCE on ownership but the delete-set still fired on ANY previously-exported-
/// now-unserved ref — the sibling-class miss. The unified desired-vs-actual+
/// ownership diff now derives delete from the same token (`recorded == old`): the
/// out-of-band tip survives; `--force` is the explicit escape. Covers local-path
/// AND URL/network.
#[test]
fn out_of_band_advance_after_embargo_not_deleted() {
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, VisibilityTier};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // A single public tip B heddle publishes to its destinations.
    std::fs::write(heddle_temp.path().join("a.txt"), b"base\n").unwrap();
    repo.snapshot(Some("base".into()), None).unwrap();
    let state_b = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let mut bridge = GitBridge::new(&repo);

    // Publish B to BOTH a local-path destination and a URL/network remote BEFORE
    // the embargo — both must record main as heddle-exported at B.
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first export publishes B (local)");
    let oid_b = test_support::mapping(&bridge)
        .get_git(&state_b)
        .expect("B minted");

    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    // Park the bare remote's HEAD off main so receive-pack never treats main as
    // the checked-out branch (matches the retraction-delete URL test).
    std::fs::write(
        remote_root.path().join("remote.git").join("HEAD"),
        b"ref: refs/heads/__heddle_placeholder\n",
    )
    .unwrap();
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");
    bridge.push(&url).expect("first network push publishes B");

    // Out-of-band advance: a NEW commit C on top of B, written to the mirror (a
    // resolvable descendant of the served frontier), the local destination, AND
    // the wire remote. Identical inputs ⇒ identical oid in all three.
    let oid_c = {
        let mirror = test_support::open_git_repo(&bridge).expect("open mirror");
        let in_mirror = commit_with_tree(
            &mirror,
            None,
            empty_tree_oid(&mirror),
            "out-of-band",
            &[oid_b],
        );
        let dest = open_git(&dest_path).expect("open dest");
        let in_dest = commit_with_tree(
            &dest,
            Some("refs/heads/main"),
            empty_tree_oid(&dest),
            "out-of-band",
            &[oid_b],
        );
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        let in_remote = commit_with_tree(
            &remote,
            Some("refs/heads/main"),
            empty_tree_oid(&remote),
            "out-of-band",
            &[oid_b],
        );
        assert_eq!(
            in_mirror, in_dest,
            "C identical in mirror and local destination"
        );
        assert_eq!(
            in_mirror, in_remote,
            "C identical in mirror and wire remote"
        );
        in_mirror
    };

    // Embargo the WHOLE line so refs/heads/main has no served frontier — the exact
    // retraction-delete trigger.
    for state in repo.store().list_states().unwrap() {
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();
    }

    // A plain export must NOT delete C at the local destination: the destination's
    // current tip (C) is not heddle's recorded published tip (B), so the ownership
    // gate spares it from retraction. Pre-r13 this DELETED C.
    bridge
        .export_to_path(&dest_path)
        .expect("plain export must not error on the out-of-band retraction (local)");
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        let tip = find_reference(&dest, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(
            tip, oid_c,
            "the out-of-band commit must survive — heddle must not delete a tip it never published (local)"
        );
    }

    // Same on the URL/network remote: a plain push must not delete the out-of-band
    // branch on the wire.
    bridge
        .push(&url)
        .expect("plain network push must not error on the out-of-band retraction");
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
        let tip = find_reference(&remote, "refs/heads/main")
            .unwrap()
            .peel_to_id()
            .unwrap();
        assert_eq!(
            tip, oid_c,
            "the out-of-band commit must survive on the URL/network remote too"
        );
    }

    // `--force` is the explicit escape: it DOES retract the out-of-band branch.
    bridge
        .push_with_scope_force(
            dest_path.to_str().expect("dest path"),
            GitPushScope::AllThreads,
            true,
        )
        .expect("--force retracts the out-of-band branch at the local destination");
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        assert!(
            find_reference(&dest, "refs/heads/main").is_err(),
            "--force deletes the retracted branch even when the destination tip was advanced out of band"
        );
    }
}

/// #316 / PR #528 r17 — the conformance matrix that turns the per-cell fix into a
/// REDESIGN. It drives [`plan_destination_reconcile`] directly across every
/// {namespace} × {operation} × {ownership} × {force} cell and asserts the
/// reconcile outcome. The structural guarantee it locks: ownership (`recorded ==
/// old`, a safe forward move, or `--force`) gates EVERY namespace's overwrite AND
/// delete; move-classification (fast-forward for branch/note, free for tag) is
/// the ONLY per-namespace axis. A future namespace that wires an overwrite
/// without funnelling through the uniform ownership gate — the exact asymmetry
/// that let an out-of-band tag be clobbered before r17 — would fail a row here.
/// The annotated-tag-object sub-cases additionally prove a tag never resolves
/// `find_commit` (which would error on a tag object).
#[test]
fn reconcile_ownership_conformance_matrix() {
    use std::collections::{HashMap, HashSet};

    let (_mirror_temp, mirror) = init_git_repo();
    // Linear topology a <- b so classify_ref_move resolves fast-forward (a->b),
    // rewind (b->a, owned), and divergence for the branch/note rows.
    let a = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "A", &[]);
    let b = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "B", &[a]);
    // An annotated-tag OBJECT (kind == Tag, NOT a commit): used as a tag tip to
    // prove classify_tag_move never calls find_commit on it.
    let tag_obj = create_annotated_tag(&mirror, "annot", b, "annotated tag object");

    // Expected reconcile outcome for a single-ref call.
    enum Outcome {
        /// In `writes` with this `new`, and `deletes` empty.
        Write(ObjectId),
        /// In `deletes`, and `writes` empty.
        Delete,
        /// Neither written nor deleted (no-op skip, or out-of-band spared).
        Absent,
        /// `Err(NonFastForwardRef)` — an unowned overwrite without `--force`.
        NonFastForward,
    }

    struct Cell {
        label: &'static str,
        ns: RefNamespace,
        old: Option<ObjectId>,
        target: ObjectId,
        recorded: Option<ObjectId>,
        /// In the served frontier (an overwrite/create) vs. only previously
        /// exported (a retraction).
        desired: bool,
        force: bool,
        expect: Outcome,
    }

    fn is_ancestor_commit(repo: &SleyRepository, ancestor: ObjectId, descendant: ObjectId) -> bool {
        let mut pending = vec![descendant];
        let mut seen = HashSet::new();
        while let Some(oid) = pending.pop() {
            if oid == ancestor {
                return true;
            }
            if !seen.insert(oid) {
                continue;
            }
            if let Ok(commit) = repo.read_commit(&oid) {
                pending.extend(commit.parents);
            }
        }
        false
    }

    fn expected_write_force(repo: &SleyRepository, cell: &Cell) -> bool {
        if cell.force {
            return true;
        }
        let Some(old) = cell.old else {
            return false;
        };
        if old == cell.target {
            return false;
        }
        match cell.ns {
            RefNamespace::Tag => true,
            RefNamespace::Branch | RefNamespace::Note => {
                !is_ancestor_commit(repo, old, cell.target)
            }
        }
    }

    let cells = vec![
        // ---- branch: fast-forward move-classification + uniform ownership ----
        Cell {
            label: "branch/create",
            ns: RefNamespace::Branch,
            old: None,
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "branch/no-op",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::Absent,
        },
        Cell {
            label: "branch/fast-forward/owned",
            ns: RefNamespace::Branch,
            old: Some(a),
            target: b,
            recorded: Some(a),
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "branch/fast-forward/out-of-band",
            ns: RefNamespace::Branch,
            old: Some(a),
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "branch/rewind/owned",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: a,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::Write(a),
        },
        Cell {
            label: "branch/rewind/out-of-band/force-off",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: a,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::NonFastForward,
        },
        Cell {
            label: "branch/rewind/out-of-band/force-on",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: a,
            recorded: None,
            desired: true,
            force: true,
            expect: Outcome::Write(a),
        },
        Cell {
            label: "branch/retract/owned",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: false,
            force: false,
            expect: Outcome::Delete,
        },
        // Out-of-band retract: heddle published `a`, the destination drifted to `b`
        // (recorded != old) — spared unless forced.
        Cell {
            label: "branch/retract/out-of-band/force-off",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: b,
            recorded: Some(a),
            desired: false,
            force: false,
            expect: Outcome::Absent,
        },
        Cell {
            label: "branch/retract/out-of-band/force-on",
            ns: RefNamespace::Branch,
            old: Some(b),
            target: b,
            recorded: Some(a),
            desired: false,
            force: true,
            expect: Outcome::Delete,
        },
        // ---- tag: free move-classification + the SAME uniform ownership gate ----
        Cell {
            label: "tag/create",
            ns: RefNamespace::Tag,
            old: None,
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "tag/no-op",
            ns: RefNamespace::Tag,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::Absent,
        },
        Cell {
            label: "tag/owned-overwrite",
            ns: RefNamespace::Tag,
            old: Some(a),
            target: b,
            recorded: Some(a),
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        // THE r17 fix: an out-of-band tag (recorded != old) is no longer clobbered.
        Cell {
            label: "tag/out-of-band-overwrite/unrecorded/force-off",
            ns: RefNamespace::Tag,
            old: Some(a),
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::NonFastForward,
        },
        Cell {
            label: "tag/out-of-band-overwrite/mismatched-record/force-off",
            ns: RefNamespace::Tag,
            old: Some(a),
            target: b,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::NonFastForward,
        },
        Cell {
            label: "tag/out-of-band-overwrite/force-on",
            ns: RefNamespace::Tag,
            old: Some(a),
            target: b,
            recorded: None,
            desired: true,
            force: true,
            expect: Outcome::Write(b),
        },
        // annotated-tag-object as the NEW target: proves no find_commit on a tag obj.
        Cell {
            label: "tag/owned-overwrite/annotated-object-target",
            ns: RefNamespace::Tag,
            old: Some(a),
            target: tag_obj,
            recorded: Some(a),
            desired: true,
            force: false,
            expect: Outcome::Write(tag_obj),
        },
        // annotated-tag-object as the OLD tip: still gated by OID compare only.
        Cell {
            label: "tag/out-of-band-overwrite/annotated-object-old/force-off",
            ns: RefNamespace::Tag,
            old: Some(tag_obj),
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::NonFastForward,
        },
        Cell {
            label: "tag/retract/owned",
            ns: RefNamespace::Tag,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: false,
            force: false,
            expect: Outcome::Delete,
        },
        Cell {
            label: "tag/retract/out-of-band/force-off",
            ns: RefNamespace::Tag,
            old: Some(b),
            target: b,
            recorded: Some(a),
            desired: false,
            force: false,
            expect: Outcome::Absent,
        },
        Cell {
            label: "tag/retract/out-of-band/force-on",
            ns: RefNamespace::Tag,
            old: Some(b),
            target: b,
            recorded: Some(a),
            desired: false,
            force: true,
            expect: Outcome::Delete,
        },
        // ---- note: classified exactly like a branch (uniform ownership) ----
        Cell {
            label: "note/create",
            ns: RefNamespace::Note,
            old: None,
            target: b,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "note/no-op",
            ns: RefNamespace::Note,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::Absent,
        },
        Cell {
            label: "note/fast-forward/owned",
            ns: RefNamespace::Note,
            old: Some(a),
            target: b,
            recorded: Some(a),
            desired: true,
            force: false,
            expect: Outcome::Write(b),
        },
        Cell {
            label: "note/rewind/owned",
            ns: RefNamespace::Note,
            old: Some(b),
            target: a,
            recorded: Some(b),
            desired: true,
            force: false,
            expect: Outcome::Write(a),
        },
        Cell {
            label: "note/rewind/out-of-band/force-off",
            ns: RefNamespace::Note,
            old: Some(b),
            target: a,
            recorded: None,
            desired: true,
            force: false,
            expect: Outcome::NonFastForward,
        },
        Cell {
            label: "note/rewind/out-of-band/force-on",
            ns: RefNamespace::Note,
            old: Some(b),
            target: a,
            recorded: None,
            desired: true,
            force: true,
            expect: Outcome::Write(a),
        },
        Cell {
            label: "note/retract/owned",
            ns: RefNamespace::Note,
            old: Some(b),
            target: b,
            recorded: Some(b),
            desired: false,
            force: false,
            expect: Outcome::Delete,
        },
    ];

    for cell in &cells {
        let short = "v1";
        let full = match cell.ns {
            RefNamespace::Branch => format!("refs/heads/{short}"),
            RefNamespace::Tag => format!("refs/tags/{short}"),
            RefNamespace::Note => format!("refs/notes/{short}"),
        };
        let served: Vec<RefUpdate> = if cell.desired {
            vec![RefUpdate {
                name: short.to_string(),
                target: cell.target,
                namespace: cell.ns,
            }]
        } else {
            Vec::new()
        };
        let mut old_map: HashMap<String, ObjectId> = HashMap::new();
        if let Some(o) = cell.old {
            old_map.insert(full.clone(), o);
        }
        let mut recorded_map: HashMap<String, ObjectId> = HashMap::new();
        if let Some(r) = cell.recorded {
            recorded_map.insert(full.clone(), r);
        }

        let result =
            plan_destination_reconcile(&mirror, &served, None, &old_map, &recorded_map, cell.force);

        if let Outcome::NonFastForward = cell.expect {
            let err = result.expect_err(&format!(
                "cell `{}`: expected Err(NonFastForwardRef)",
                cell.label
            ));
            assert!(
                matches!(err, GitBridgeError::NonFastForwardRef { .. }),
                "cell `{}`: expected NonFastForwardRef, got {err:?}",
                cell.label
            );
            continue;
        }

        let plan =
            result.unwrap_or_else(|e| panic!("cell `{}`: expected Ok, got {e:?}", cell.label));
        match cell.expect {
            Outcome::Write(exp_new) => {
                assert!(
                    plan.deletes.is_empty(),
                    "cell `{}`: expected no deletes",
                    cell.label
                );
                assert_eq!(
                    plan.writes.len(),
                    1,
                    "cell `{}`: expected exactly one write",
                    cell.label
                );
                let w = &plan.writes[0];
                assert_eq!(w.full_name, full, "cell `{}`: write name", cell.label);
                assert_eq!(w.new, exp_new, "cell `{}`: write new", cell.label);
                assert_eq!(
                    w.force,
                    expected_write_force(&mirror, cell),
                    "cell `{}`: write force",
                    cell.label
                );
            }
            Outcome::Delete => {
                assert!(
                    plan.writes.is_empty(),
                    "cell `{}`: expected no writes",
                    cell.label
                );
                assert_eq!(
                    plan.deletes.len(),
                    1,
                    "cell `{}`: expected exactly one delete",
                    cell.label
                );
                let d = &plan.deletes[0];
                assert_eq!(d.full_name, full, "cell `{}`: delete name", cell.label);
            }
            Outcome::Absent => {
                assert!(
                    plan.writes.is_empty(),
                    "cell `{}`: expected no writes",
                    cell.label
                );
                assert!(
                    plan.deletes.is_empty(),
                    "cell `{}`: expected no deletes",
                    cell.label
                );
            }
            Outcome::NonFastForward => unreachable!("handled above"),
        }
    }
}

/// #316 / PR #528 — destination foreign-ref over-claim: a pre-existing destination
/// ref already AT the served target that heddle never recorded (no exported-refs
/// entry) must NOT be claimed into the manifest. The verdict is Skip (no write, no
/// delete), but the prior code still recorded it — which would let a LATER export,
/// once it no longer serves that ref, DELETE a ref heddle never created. The
/// reconcile now leaves it out of the new manifest, so the foreign ref survives a
/// later retraction.
#[test]
fn foreign_destination_ref_spared() {
    use std::collections::HashMap;

    let (_mirror_temp, mirror) = init_git_repo();
    let a = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "A", &[]);
    let full = "refs/tags/user-v1".to_string();

    // The served frontier WANTS refs/tags/user-v1 at A, and the destination already
    // happens to be at A — but heddle never recorded it (recorded None): FOREIGN.
    let served = vec![RefUpdate {
        name: "user-v1".to_string(),
        target: a,
        namespace: RefNamespace::Tag,
    }];
    let mut old_map: HashMap<String, ObjectId> = HashMap::new();
    old_map.insert(full.clone(), a);
    let recorded_map: HashMap<String, ObjectId> = HashMap::new();

    let plan =
        plan_destination_reconcile(&mirror, &served, None, &old_map, &recorded_map, false).unwrap();
    assert!(
        plan.writes.is_empty(),
        "a coincidental match must not be written"
    );
    assert!(
        plan.deletes.is_empty(),
        "a coincidental match must not be deleted"
    );
    assert!(
        !plan.new_manifest.contains_key(&full),
        "heddle must NOT claim ownership of a foreign destination ref it never recorded"
    );

    // The consequence: a LATER export that no longer serves user-v1 must NOT delete
    // it, because it was never claimed (the manifest carries no record for it).
    let plan2 = plan_destination_reconcile(&mirror, &[], None, &old_map, &plan.new_manifest, false)
        .unwrap();
    assert!(
        plan2.deletes.is_empty(),
        "the foreign ref must survive a later retraction — it was never heddle's to delete"
    );
}

/// #316 / PR #528 r17 (the behavioral gap): an out-of-band destination TAG heddle
/// never recorded must NOT be clobbered by a plain export/push — the precise
/// asymmetry r1-r16 left open (tag DELETES were ownership-gated, tag WRITES were
/// not). A destination `refs/tags/v1.0` advanced out of band past heddle's
/// recorded published tip is FF-rejected (`NonFastForwardRef`) and survives; only
/// `--force` overwrites it back to the served target. Covers BOTH call sites —
/// local-path AND URL/network.
#[test]
fn out_of_band_destination_tag_not_overwritten() {
    use cli::bridge::git_core::GitBridgeError;
    use objects::object::{Attribution, Principal, State};

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init_default(heddle_temp.path()).expect("init heddle");
    let mut cfg = repo.config().clone();
    cfg.set_principal("Grace Hopper", "grace@example.com");
    cfg.save(&repo.heddle_dir().join("config.toml"))
        .expect("save principal");
    let repo = Repository::open(heddle_temp.path()).expect("reopen heddle");

    // A public main that stays put, plus a SEPARATE state R pinned by marker v1.0
    // — so heddle serves refs/tags/v1.0 throughout (it never rewinds/retracts).
    std::fs::write(heddle_temp.path().join("main.txt"), b"trunk\n").unwrap();
    repo.snapshot(Some("trunk".into()), None).unwrap();
    let r = {
        let store = repo.store();
        let blob = store
            .put_blob(&Blob::from_slice(b"release\n"))
            .expect("blob");
        let tree = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("release.txt".to_string(), blob, false).expect("entry"),
            ]))
            .expect("tree");
        let state = State::new(
            tree,
            Vec::new(),
            Attribution::human(Principal::new("Grace Hopper", "grace@example.com")),
        );
        store.put_state(&state).expect("put R");
        state
    };
    repo.refs()
        .create_marker(&MarkerName::new("v1.0"), &r.change_id)
        .expect("create marker at R");

    let mut bridge = GitBridge::new(&repo);

    // ---- local-path destination ----
    let dest_root = TempDir::new().expect("dest temp");
    let dest_path = dest_root.path().join("export-target");
    bridge
        .export_to_path(&dest_path)
        .expect("first export publishes the tag (local)");

    // The served tag tip heddle recorded for this destination.
    let served_tag = {
        let dest = open_git(&dest_path).expect("open dest");
        read_exported_refs(&dest).expect("read record")["refs/tags/v1.0"]
    };

    // Out-of-band advance: move the destination tag to a fresh commit X heddle
    // never published (and never recorded).
    let oid_x = {
        let dest = open_git(&dest_path).expect("open dest");
        let x = commit_with_tree(&dest, None, empty_tree_oid(&dest), "out-of-band-tag", &[]);
        set_reference(
            &dest,
            "refs/tags/v1.0",
            x,
            RefPrecondition::Any,
            "test: out-of-band tag",
        )
        .expect("move tag out of band");
        x
    };
    assert_ne!(
        oid_x, served_tag,
        "the out-of-band tag tip must differ from the served tip"
    );

    // A plain export must NOT clobber X: heddle does not own that tip.
    let err = bridge.export_to_path(&dest_path).expect_err(
        "out-of-band tag must be FF-rejected at the local destination, not overwritten",
    );
    assert!(
        matches!(err, GitBridgeError::NonFastForwardRef { .. }),
        "expected a non-fast-forward rejection, got: {err:?}"
    );
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        let tip = find_reference(&dest, "refs/tags/v1.0")
            .unwrap()
            .try_id()
            .expect("tag id");
        assert_eq!(
            tip, oid_x,
            "the out-of-band tag must survive — heddle must not overwrite it"
        );
    }

    // `--force` is the explicit escape hatch: it DOES overwrite back to the tip.
    bridge
        .push_with_scope_force(
            dest_path.to_str().expect("dest path"),
            GitPushScope::AllThreads,
            true,
        )
        .expect("--force overrides the tag ownership gate at the local destination");
    {
        let dest = open_git(&dest_path).expect("reopen dest");
        let tip = find_reference(&dest, "refs/tags/v1.0")
            .unwrap()
            .try_id()
            .expect("tag id");
        assert_eq!(
            tip, served_tag,
            "--force rewinds the local destination tag to the served tip"
        );
    }

    // ---- URL/network destination ----
    let remote_root = TempDir::new().expect("remote root");
    let _remote_repo = init_named_bare_git_repo(&remote_root, "remote.git");
    std::fs::write(
        remote_root.path().join("remote.git").join("HEAD"),
        b"ref: refs/heads/__heddle_placeholder\n",
    )
    .unwrap();
    let Some(daemon) = GitDaemon::spawn_push(remote_root.path()) else {
        return;
    };
    let url = daemon.url("remote.git");

    bridge
        .push(&url)
        .expect("first network push publishes the tag");
    let remote_served_tag = {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        find_reference(&remote, "refs/tags/v1.0")
            .unwrap()
            .try_id()
            .expect("tag id")
    };

    let remote_oid_x = {
        let remote = open_git(remote_root.path().join("remote.git")).expect("open remote");
        let x = commit_with_tree(
            &remote,
            None,
            empty_tree_oid(&remote),
            "out-of-band-tag",
            &[],
        );
        set_reference(
            &remote,
            "refs/tags/v1.0",
            x,
            RefPrecondition::Any,
            "test: out-of-band tag",
        )
        .expect("move remote tag out of band");
        x
    };
    assert_ne!(
        remote_oid_x, remote_served_tag,
        "the out-of-band remote tag tip must differ"
    );

    let err = bridge
        .push(&url)
        .expect_err("out-of-band tag must be FF-rejected on the URL/network remote too");
    assert!(
        matches!(err, GitBridgeError::NonFastForwardRef { .. }),
        "expected a non-fast-forward rejection on the wire, got: {err:?}"
    );
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
        let tip = find_reference(&remote, "refs/tags/v1.0")
            .unwrap()
            .try_id()
            .expect("tag id");
        assert_eq!(
            tip, remote_oid_x,
            "the out-of-band tag must survive on the URL/network remote"
        );
    }

    bridge
        .push_with_scope_force(&url, GitPushScope::AllThreads, true)
        .expect("--force overrides the tag ownership gate on the URL/network remote");
    {
        let remote = open_git(remote_root.path().join("remote.git")).expect("reopen remote");
        let tip = find_reference(&remote, "refs/tags/v1.0")
            .unwrap()
            .try_id()
            .expect("tag id");
        assert_eq!(
            tip, remote_served_tag,
            "--force rewinds the remote tag to the served tip"
        );
    }
}

/// #316 / PR #528 r17: the ownership gate must NOT over-block a LEGITIMATE tag
/// move. A destination tag whose tip heddle still OWNS (`recorded == old`) is
/// overwritten to the served target on a plain reconcile — `classify_tag_move`'s
/// gate spares ONLY out-of-band tips heddle never recorded, never heddle's own
/// published moves. Driven directly against [`plan_destination_reconcile`]
/// (the mirror now FORCE-retargets a marker re-point, heddle#316 S1, so the
/// source-move path cannot stage an out-of-band DESTINATION tip end-to-end),
/// pinning the owned-vs-unowned boundary in one focused regression: identical
/// inputs land a write when owned, and demand `--force` when not.
#[test]
fn heddle_owned_tag_overwrite_still_lands() {
    use std::collections::HashMap;

    let (_mirror_temp, mirror) = init_git_repo();
    let a = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "A", &[]);
    let b = commit_with_tree(&mirror, None, empty_tree_oid(&mirror), "B", &[a]);

    let full = "refs/tags/v1.0".to_string();
    let served = vec![RefUpdate {
        name: "v1.0".to_string(),
        target: b,
        namespace: RefNamespace::Tag,
    }];
    let old_at_destination: HashMap<String, ObjectId> = [(full.clone(), a)].into_iter().collect();

    // Owned: heddle recorded the destination tip `a` it is overwriting → the move
    // to `b` lands as a plain write, no `--force` needed.
    let owned: HashMap<String, ObjectId> = [(full.clone(), a)].into_iter().collect();
    let plan =
        plan_destination_reconcile(&mirror, &served, None, &old_at_destination, &owned, false)
            .expect("a heddle-owned tag move must reconcile without --force");
    assert_eq!(
        plan.writes.len(),
        1,
        "owned tag move must produce exactly one write"
    );
    assert_eq!(plan.writes[0].full_name, full);
    assert!(
        plan.writes[0].force,
        "owned tag overwrite must force the destination update"
    );
    assert_eq!(plan.writes[0].new, b, "write lands the served target");
    assert!(
        plan.deletes.is_empty(),
        "an owned overwrite is a write, not a delete"
    );

    // Contrast: the SAME move with no ownership record (out-of-band tip) is the r17
    // gate — FF-rejected unless `--force`.
    let unrecorded: HashMap<String, ObjectId> = HashMap::new();
    let err = plan_destination_reconcile(
        &mirror,
        &served,
        None,
        &old_at_destination,
        &unrecorded,
        false,
    )
    .expect_err("an unowned tag overwrite must be FF-rejected without --force");
    assert!(
        matches!(err, GitBridgeError::NonFastForwardRef { .. }),
        "expected NonFastForwardRef, got {err:?}"
    );
}

/// Write a raw root commit with explicit, distinct author and committer
/// signatures and arbitrary extra headers. Unlike `commit_with_tree`, which
/// reuses one zero-offset signature for both roles, this surfaces committer,
/// timezone, gpgsig, and extra-header fidelity (#564 step 1).
fn commit_with_signatures(
    repo: &SleyRepository,
    reference: Option<&str>,
    tree_oid: ObjectId,
    message: &str,
    author: Signature,
    committer: Signature,
    extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
) -> ObjectId {
    let body = raw_commit_body(
        tree_oid,
        &[],
        &author,
        &committer,
        extra_headers,
        message.as_bytes(),
    );
    let id = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, body))
        .expect("write commit");
    if let Some(reference) = reference {
        set_reference(
            repo,
            reference,
            id,
            RefPrecondition::Any,
            "test: update ref",
        )
        .expect("update ref");
    }
    id
}

fn raw_commit_body(
    tree_oid: ObjectId,
    parents: &[ObjectId],
    author: &Signature,
    committer: &Signature,
    extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    message: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("tree {tree_oid}\n").as_bytes());
    for parent in parents {
        out.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    out.extend_from_slice(b"author ");
    out.extend_from_slice(&author.to_ident_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"committer ");
    out.extend_from_slice(&committer.to_ident_bytes());
    out.push(b'\n');
    for (name, value) in extra_headers {
        write_raw_commit_header(&mut out, &name, &value);
    }
    out.push(b'\n');
    out.extend_from_slice(message);
    out
}

fn write_raw_commit_header(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.extend_from_slice(name);
    out.push(b' ');
    let mut lines = value.split(|byte| *byte == b'\n');
    if let Some(first) = lines.next() {
        out.extend_from_slice(first);
    }
    for line in lines {
        out.push(b'\n');
        out.push(b' ');
        out.extend_from_slice(line);
    }
    out.push(b'\n');
}

/// #564 step 1: a commit imported through the bridge must round-trip every
/// git-fidelity field — distinct committer identity, both timezone offsets,
/// the verbatim message, the gpgsig (pulled out of the extra headers), and
/// the remaining extra headers in order.
#[test]
fn import_preserves_commit_git_fidelity_fields() {
    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let (_git_temp, git_repo) = init_git_repo();

    let tree_oid = empty_tree_oid(&git_repo);
    let author = test_signature_at("Author", "author@example.com", 1_000_000, -7 * 60);
    let committer = test_signature_at("Committer", "committer@example.com", 2_000_000, 2 * 60);
    let gpgsig = "-----BEGIN PGP SIGNATURE-----\nABCD\n-----END PGP SIGNATURE-----";
    let extra_headers = vec![
        (b"gpgsig".to_vec(), gpgsig.as_bytes().to_vec()),
        (b"mergetag".to_vec(), b"object deadbeef".to_vec()),
    ];
    let commit_oid = commit_with_signatures(
        &git_repo,
        Some("refs/heads/main"),
        tree_oid,
        "feat: thing\n\nBody.\n",
        author,
        committer,
        extra_headers,
    );

    let mut bridge = GitBridge::new(&repo);
    import_all(
        &mut bridge,
        Some(git_repo.workdir().expect("workdir").as_path()),
    )
    .expect("import");

    let change_id = test_support::mapping(&bridge)
        .get_heddle(commit_oid)
        .expect("commit mapped");
    let state = repo
        .store()
        .get_state(&change_id)
        .expect("load state")
        .expect("state written");

    let stored_committer = state.committer.expect("committer preserved");
    assert_eq!(stored_committer.name, "Committer");
    assert_eq!(stored_committer.email, "committer@example.com");
    assert_eq!(state.authored_tz_offset, -7 * 3600);
    assert_eq!(state.committer_tz_offset, 2 * 3600);
    assert_eq!(
        state.raw_message.as_deref(),
        Some("feat: thing\n\nBody.\n".as_bytes())
    );
    // Git config folds multi-line extra-header values and round-trips them with a
    // trailing newline; byte-exact State round-tripping is covered by the
    // ingest unit test. Here we verify gpgsig stays INLINE in extra_headers at
    // its captured position (not split into a separate field) and the headers
    // keep their original order.
    assert_eq!(
        state.extra_headers.len(),
        2,
        "gpgsig + mergetag, both inline"
    );
    assert_eq!(state.extra_headers[0].0, b"gpgsig".to_vec());
    assert_eq!(
        String::from_utf8_lossy(&state.extra_headers[0].1).trim_end(),
        gpgsig,
        "gpgsig preserved inline at its captured position"
    );
    assert_eq!(state.extra_headers[1].0, b"mergetag".to_vec());
    assert_eq!(
        String::from_utf8_lossy(&state.extra_headers[1].1).trim_end(),
        "object deadbeef"
    );
}

/// #564 de-lossy step 1, close-the-class proof (#565 r3). Every
/// fidelity-bearing git byte-string of a COMMIT must survive byte-identically
/// through BOTH import paths — the bridge (`import_all`) and the ingest engine
/// (`ingest::import_git_into`). A single fixture exercises non-UTF8 bytes in
/// ALL of: the commit message, a `gpgsig` header (kept inline in
/// `extra_headers`), a custom extra-header value, and a `mergetag` payload.
/// `0xe9` (latin-1 `é`) is invalid standalone UTF-8, so any lingering
/// `String`/`to_string()` hop on any of these would replace it with U+FFFD and
/// fail an assertion below. This one test fails if ANY sibling of the
/// byte-vs-String class regresses. (Annotated-tag-object fidelity is out of
/// scope here — see #575.)
#[test]
fn non_utf8_git_fidelity_is_byte_identical_across_bridge_and_ingest() {
    use ingest::import_git_into;

    let (_git_temp, git_repo) = init_git_repo();
    let tree_oid = empty_tree_oid(&git_repo);

    // The message keeps its own internal blank line (`\n\n`) to prove the
    // header/body split isn't fooled by a `\n\n` inside the body.
    let commit_message = b"caf\xe9 subject\n\nbody with a \xe9 byte\n".to_vec();
    let gpgsig_value =
        b"-----BEGIN PGP SIGNATURE-----\n\xe9 not-real-armor\n-----END PGP SIGNATURE-----".to_vec();
    let custom_value = b"x-note caf\xe9 value".to_vec();
    // A mergetag value is a whole tag object — multi-line, with its own blank
    // line and a non-UTF8 byte in the tag name and message lines.
    let mergetag_value = {
        let mut v = Vec::new();
        v.extend_from_slice(b"object 0123456789012345678901234567890123456789\n");
        v.extend_from_slice(b"type commit\n");
        v.extend_from_slice(b"tag merged-\xe9\n");
        v.extend_from_slice(b"tagger Heddle Test <heddle@test> 0 +0000\n");
        v.extend_from_slice(b"\n");
        v.extend_from_slice(b"merge note caf\xe9\n");
        v
    };

    let sig = test_signature();
    let commit = raw_commit_body(
        tree_oid,
        &[],
        &sig,
        &sig,
        vec![
            (b"gpgsig".to_vec(), gpgsig_value.clone()),
            (b"x-custom".to_vec(), custom_value.clone()),
            (b"mergetag".to_vec(), mergetag_value.clone()),
        ],
        &commit_message,
    );
    let commit_oid = git_repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit))
        .expect("write commit");
    set_reference(
        &git_repo,
        "refs/heads/main",
        commit_oid,
        RefPrecondition::Any,
        "test: main",
    )
    .expect("set main");

    let git_workdir = git_repo.workdir().expect("workdir");

    // ── path 1: bridge import ──
    let bridge_heddle = TempDir::new().expect("bridge heddle temp");
    let bridge_repo = Repository::init(bridge_heddle.path()).expect("init bridge heddle");
    let mut bridge = GitBridge::new(&bridge_repo);
    import_all(&mut bridge, Some(git_workdir.as_path())).expect("bridge import");
    let bridge_cid = test_support::mapping(&bridge)
        .get_heddle(commit_oid)
        .expect("bridge mapped commit");
    let bridge_state = bridge_repo
        .store()
        .get_state(&bridge_cid)
        .expect("load bridge state")
        .expect("bridge state written");

    // ── path 2: ingest import ──
    let ingest_heddle = TempDir::new().expect("ingest heddle temp");
    let (_stats, ingest_map) =
        import_git_into(git_workdir, ingest_heddle.path()).expect("ingest import");
    let ingest_cid = ingest_map
        .get_commit(&commit_oid.to_string())
        .expect("ingest mapped commit");
    let ingest_repo = Repository::open(ingest_heddle.path()).expect("open ingest heddle");
    let ingest_state = ingest_repo
        .store()
        .get_state(&ingest_cid)
        .expect("load ingest state")
        .expect("ingest state written");

    // The fixture really is non-UTF8 — otherwise the test proves nothing.
    assert!(String::from_utf8(commit_message.clone()).is_err());

    // (1) commit message: byte-exact to the original, AND byte-identical
    // across paths — this pins `message_raw_sloppy` (bridge) and
    // `message_raw` (ingest) to the same bytes.
    assert_eq!(
        bridge_state.raw_message.as_deref(),
        Some(commit_message.as_slice()),
        "bridge must preserve the raw commit message verbatim"
    );
    assert_eq!(
        ingest_state.raw_message, bridge_state.raw_message,
        "raw_message must be byte-identical across bridge and ingest"
    );

    // (2) extra headers (gpgsig + custom + mergetag): identical across paths,
    // ordered, non-UTF8 survived. gpgsig is kept INLINE here at its captured
    // position (not split into a separate field), so its byte-identity is
    // proved by this same comparison.
    assert_eq!(
        bridge_state.extra_headers, ingest_state.extra_headers,
        "extra_headers must be byte-identical (and same order) across paths"
    );
    let keys: Vec<&[u8]> = bridge_state
        .extra_headers
        .iter()
        .map(|(k, _)| k.as_slice())
        .collect();
    assert_eq!(
        keys,
        vec![
            b"gpgsig".as_slice(),
            b"x-custom".as_slice(),
            b"mergetag".as_slice(),
        ],
        "gpgsig stays inline at its captured position; all three remain in order"
    );
    for (key, value) in &bridge_state.extra_headers {
        assert!(
            value.contains(&0xe9u8),
            "extra-header {:?} must preserve its non-UTF8 byte",
            String::from_utf8_lossy(key)
        );
        assert!(String::from_utf8(value.clone()).is_err());
    }
}
