// SPDX-License-Identifier: Apache-2.0
//! Differential proof against Git, using the production native import converter.
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use heddle_git_serialize::{
    NativeObjectId, ObjectFormat, ObjectId, ObjectReader, ObjectSink, ReconstructionLimits,
    reconstruct_closure,
};
use heddle_object_model::object::{
    Blob, ContentHash, EntryType, State, StateId, Tree, TreeEntry, parse_git_tree,
    thread_replication::{
        git_import_converter::{GitImportGraph, GitImportRawCommit},
        git_import_graph::{GitObjectFormat, GitObjectId},
    },
};

#[derive(Default)]
struct NativeStore {
    states: BTreeMap<StateId, State>,
    trees: BTreeMap<ContentHash, Tree>,
    blobs: BTreeMap<ContentHash, Blob>,
}

impl ObjectReader for NativeStore {
    type Error = &'static str;
    async fn read_state(&self, id: StateId, _max_bytes: usize) -> Result<State, Self::Error> {
        self.states.get(&id).cloned().ok_or("missing state")
    }
    async fn read_tree(&self, id: ContentHash, _max_bytes: usize) -> Result<Tree, Self::Error> {
        self.trees.get(&id).cloned().ok_or("missing tree")
    }
    async fn read_blob(&self, id: ContentHash, _max_bytes: usize) -> Result<Blob, Self::Error> {
        self.blobs.get(&id).cloned().ok_or("missing blob")
    }
}

#[derive(Default)]
struct Verifier {
    expected: BTreeMap<ObjectId, (&'static str, Vec<u8>)>,
    seen: BTreeMap<ObjectId, usize>,
}

impl ObjectSink for Verifier {
    type Error = &'static str;
    async fn write_object(
        &mut self,
        kind: &'static str,
        id: ObjectId,
        content: &[u8],
    ) -> Result<(), Self::Error> {
        let (expected_kind, expected_body) = self
            .expected
            .get(&id)
            .expect("reconstructed original object ID");
        assert_eq!(kind, *expected_kind);
        assert_eq!(content, expected_body, "byte difference for {kind} {id}");
        *self.seen.entry(id).or_default() += 1;
        Ok(())
    }
}

fn git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
    }
    let result = child.wait_with_output().expect("wait git");
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}

fn write_object(dir: &Path, kind: &str, content: &[u8], format: ObjectFormat) -> ObjectId {
    let oid = git(
        dir,
        &["hash-object", "--literally", "-w", "-t", kind, "--stdin"],
        Some(content),
    );
    ObjectId::from_hex(format, String::from_utf8(oid).expect("hex").trim()).expect("object ID")
}

fn tree(dir: &Path, entries: &[(&str, &str, ObjectId)], format: ObjectFormat) -> ObjectId {
    let mut body = Vec::new();
    for (mode, name, id) in entries {
        body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        body.extend_from_slice(id.as_bytes());
    }
    write_object(dir, "tree", &body, format)
}

fn commit(
    dir: &Path,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &[u8],
    committer: &[u8],
    extra: &[u8],
    message: &[u8],
) -> ObjectId {
    let mut body = format!("tree {tree}\n").into_bytes();
    for parent in parents {
        body.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    body.extend_from_slice(b"author ");
    body.extend_from_slice(author);
    body.extend_from_slice(b"\ncommitter ");
    body.extend_from_slice(committer);
    body.push(b'\n');
    body.extend_from_slice(extra);
    body.push(b'\n');
    body.extend_from_slice(message);
    write_object(dir, "commit", &body, tree.format())
}

fn build_corpus(dir: &Path, format: ObjectFormat) {
    let format_flag = format!("--object-format={}", format.name());
    git(
        dir,
        &["init", "-q", "--initial-branch=main", &format_flag],
        None,
    );
    let a = write_object(dir, "blob", b"a\0binary\xff\n", format);
    let link = write_object(dir, "blob", b"a.txt", format);
    let leaf = tree(dir, &[("100644", "x", a)], format);
    let ordinary = tree(
        dir,
        &[("100644", "a.txt", a), ("40000", "lib", leaf)],
        format,
    );
    let root = commit(
        dir,
        ordinary,
        &[],
        b"A <a@x> 1700000000 +0000",
        b"C <c@x> 1700000001 +0530",
        b"",
        b"ordinary\n",
    );
    let odd = tree(
        dir,
        &[
            ("100664", "z", a),
            ("040000", "lib", leaf),
            ("100775", "run", a),
            ("120777", "link", link),
            ("0160000", "vendor", root),
            ("100645", "lib.rs", a),
        ],
        format,
    );
    let left = commit(
        dir,
        odd,
        &[root],
        b"<only@email.example> 1700000002 -0000",
        b"C <c@x> 1700000003 -0000",
        b"x-raw \xff\n continuation\nencoding ISO-8859-1\n",
        b"raw \xe9 no trailing newline",
    );
    let right = commit(
        dir,
        ordinary,
        &[root],
        b"A \xff <a@x> 1700000004 -0830",
        b"C \xfe <c@x> 1700000005 +1245",
        b"",
        b"",
    );
    let merge = commit(dir, odd, &[left, right], b"A <a@x> 1700000006 +0000", b"<c@x> 1700000007 -0000", b"x-first value\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n fake preserved armor\n -----END PGP SIGNATURE-----\nmergetag object deadbeef\n type commit\n tag preserved\n \n tag body\nx-last \xfe\n", b"merge\r\n");
    git(
        dir,
        &["update-ref", "refs/heads/main", &merge.to_string()],
        None,
    );
}

fn import_native(
    dir: &Path,
    format: ObjectFormat,
) -> (NativeStore, Verifier, Vec<(StateId, ObjectId)>) {
    let mut store = NativeStore::default();
    let mut verifier = Verifier::default();
    let mut trees = Vec::new();
    let mut mapped = BTreeMap::<ObjectId, ContentHash>::new();
    let list = git(dir, &["rev-list", "--objects", "--all"], None);
    for line in String::from_utf8(list).expect("object list").lines() {
        let sha = line.split_whitespace().next().expect("oid");
        let oid = ObjectId::from_hex(format, sha).expect("oid");
        let kind = git(dir, &["cat-file", "-t", sha], None);
        let kind = String::from_utf8(kind).expect("kind");
        let kind = match kind.trim() {
            "blob" => "blob",
            "tree" => "tree",
            "commit" => "commit",
            _ => continue,
        };
        let body = git(dir, &["cat-file", kind, sha], None);
        verifier.expected.insert(oid, (kind, body.clone()));
        if kind == "blob" {
            let blob = Blob::new(body);
            mapped.insert(oid, blob.hash());
            store.blobs.insert(blob.hash(), blob);
        } else if kind == "tree" {
            trees.push((oid, body));
        }
    }
    while !trees.is_empty() {
        let before = trees.len();
        trees.retain(|(oid, body)| {
            let entries = parse_git_tree(format, body).expect("parse tree");
            if entries.iter().any(|entry| {
                entry.mode.entry_type() != Some(EntryType::Gitlink)
                    && !mapped.contains_key(&entry.oid)
            }) {
                return true;
            }
            let entries = entries
                .into_iter()
                .map(|entry| {
                    let name = std::str::from_utf8(entry.name).expect("fixture name");
                    let native = match entry.mode.entry_type().expect("kind") {
                        EntryType::Tree => TreeEntry::directory(name, mapped[&entry.oid]),
                        EntryType::Blob => {
                            TreeEntry::file(name, mapped[&entry.oid], entry.mode.is_executable())
                        }
                        EntryType::Symlink => TreeEntry::symlink(name, mapped[&entry.oid]),
                        EntryType::Gitlink => TreeEntry::gitlink(name, entry.oid),
                        EntryType::Spoollink => unreachable!("Git cannot store a spoollink"),
                    }
                    .expect("native entry");
                    native.with_raw_git_mode(entry.mode).expect("raw mode")
                })
                .collect();
            let native = Tree::from_git_entries(entries).expect("native tree");
            mapped.insert(*oid, native.hash());
            store.trees.insert(native.hash(), native);
            false
        });
        assert!(trees.len() < before, "fixture tree closure must progress");
    }
    let list = git(
        dir,
        &["rev-list", "--reverse", "--topo-order", "--all"],
        None,
    );
    let mut commits = BTreeMap::<ObjectId, StateId>::new();
    let mut roots = Vec::new();
    for sha in String::from_utf8(list).expect("commits").lines() {
        let oid = ObjectId::from_hex(format, sha).expect("commit oid");
        let body = &verifier.expected[&oid].1;
        let headers = body
            .split(|b| *b == b'\n')
            .take_while(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let tree = ObjectId::from_hex(
            format,
            std::str::from_utf8(&headers[0][5..]).expect("tree oid"),
        )
        .expect("tree oid");
        let parents = headers
            .iter()
            .filter_map(|line| line.strip_prefix(b"parent "))
            .map(|raw| {
                let parent =
                    ObjectId::from_hex(format, std::str::from_utf8(raw).expect("parent oid"))
                        .expect("parent oid");
                commits[&parent]
            })
            .collect();
        let import_oid = match format {
            ObjectFormat::Sha1 => GitObjectId::Sha1(oid.as_bytes().try_into().expect("sha1")),
            ObjectFormat::Sha256 => GitObjectId::Sha256(oid.as_bytes().try_into().expect("sha256")),
        };
        let state = GitImportGraph::convert_raw_commit(
            GitImportRawCommit {
                oid: &import_oid,
                object_format: match format {
                    ObjectFormat::Sha1 => GitObjectFormat::Sha1,
                    ObjectFormat::Sha256 => GitObjectFormat::Sha256,
                },
                raw_commit: body,
                heddle_note: None,
            },
            mapped[&tree],
            parents,
            false,
            |_| Ok(None),
        )
        .expect("native import converter");
        commits.insert(oid, state.id());
        roots.push((state.id(), oid));
        store.states.insert(state.id(), state);
    }
    (store, verifier, roots)
}

fn assert_corpus(dir: &Path, format: ObjectFormat) {
    let (store, mut verifier, roots) = import_native(dir, format);
    for (state, original) in roots {
        let future = reconstruct_closure(
            &store,
            &mut verifier,
            NativeObjectId::Commit(state),
            format,
            ReconstructionLimits::default(),
        );
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&future);
        let reconstructed = pollster::block_on(future).expect("reconstruct closure");
        assert_eq!(reconstructed, original);
    }
    assert_eq!(
        verifier.seen.len(),
        verifier.expected.len(),
        "every original commit/tree/blob was verified"
    );
    // The diamond merge shares its base; the bounded walk emits it repeatedly.
    assert!(verifier.seen.values().any(|count| *count > 1));
}

#[test]
fn git_fixture_bytes_and_ids_sha1() {
    let dir = tempfile::tempdir().expect("fixture");
    build_corpus(dir.path(), ObjectFormat::Sha1);
    assert_corpus(dir.path(), ObjectFormat::Sha1);
}

#[test]
fn git_fixture_bytes_and_ids_sha256() {
    let dir = tempfile::tempdir().expect("fixture");
    build_corpus(dir.path(), ObjectFormat::Sha256);
    assert_corpus(dir.path(), ObjectFormat::Sha256);
}

#[test]
fn signed_fixture_bytes_and_ids() {
    let dir = tempfile::tempdir().expect("fixture");
    let bundle = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cli/tests/commit_conformance_fixtures/commit-corpus.bundle");
    git(
        dir.path(),
        &["init", "-q", "--initial-branch=__bootstrap"],
        None,
    );
    git(
        dir.path(),
        &[
            "fetch",
            "-q",
            bundle.to_str().expect("bundle path"),
            "refs/heads/*:refs/heads/*",
        ],
        None,
    );
    let (store, verifier, roots) = import_native(dir.path(), ObjectFormat::Sha1);
    assert!(
        verifier
            .expected
            .values()
            .any(|(kind, body)| *kind == "commit" && body.windows(8).any(|w| w == b"\ngpgsig "))
    );
    assert!(
        verifier
            .expected
            .values()
            .any(|(kind, body)| *kind == "commit" && body.windows(10).any(|w| w == b"\nmergetag "))
    );
    let mut verifier = verifier;
    for (state, oid) in roots {
        assert_eq!(
            pollster::block_on(reconstruct_closure(
                &store,
                &mut verifier,
                NativeObjectId::Commit(state),
                ObjectFormat::Sha1,
                ReconstructionLimits::default()
            ))
            .expect("signed closure"),
            oid
        );
    }
    assert_eq!(verifier.seen.len(), verifier.expected.len());
}
