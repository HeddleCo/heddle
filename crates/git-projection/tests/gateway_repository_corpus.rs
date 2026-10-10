// SPDX-License-Identifier: Apache-2.0
//! Synthetic representation/limit corpus. These tests exercise real converters
//! and receiver reconstruction, not account authorization or production load.
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    gateway_received::project_received_git_history,
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{
        LocalPush, PreparedGitPush, PushAuthor, PushUpdate, WriteLimits, parse_receive_pack,
        prepare_git_push,
    },
};
use objects::{
    object::{
        Attribution, Blob, ContentHash, Principal, State, StateId, Tree, TreeEntry,
        thread_replication::{
            AuthoredCapture, SourceAuthor, ThreadOperation, ThreadOperationBody,
            initial_base::synthetic_initial_base,
        },
    },
    store::{InMemoryStore, ObjectStore},
};
use repo::Repository;
use sley::{GitObjectType, ObjectId, Repository as GitRepository};
use std::collections::BTreeSet;

fn attribution() -> Attribution {
    Attribution::human(Principal::new("Synthetic Corpus", "corpus@example.invalid"))
}
struct Native {
    store: InMemoryStore,
    originals: Vec<SignedOperation>,
    signer: Ed25519Signer,
}
impl Native {
    fn new() -> Self {
        Self {
            store: InMemoryStore::new(),
            originals: vec![],
            signer: Ed25519Signer::from_seed(&[74; 32]).unwrap(),
        }
    }
    fn tree(&self, count: usize, bytes: &[u8]) -> ContentHash {
        let blob = Blob::new(bytes.to_vec());
        self.store.put_blob(&blob).unwrap();
        let tree = Tree::from_git_entries(
            (0..count)
                .map(|i| TreeEntry::file(format!("file-{i:05}"), blob.hash(), false).unwrap())
                .collect(),
        )
        .unwrap();
        self.store.put_tree(&tree).unwrap()
    }
    fn state(&mut self, tree: ContentHash, parents: Vec<StateId>, label: &str) -> StateId {
        let state = State::new_snapshot(tree, parents, attribution()).with_intent(label);
        self.store.put_state(&state).unwrap();
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::compute(b"corpus-thread"),
            parents: self
                .originals
                .last()
                .map(|s| BTreeSet::from([s.verify().unwrap().id().unwrap()]))
                .unwrap_or_default(),
            publisher: self.signer.public_key().try_into().unwrap(),
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                state.encode_current_msgpack().unwrap().into(),
            )),
        };
        self.originals
            .push(SignedOperation::sign(&operation, &self.signer).unwrap());
        state.id()
    }
    fn project(
        &self,
        tip: StateId,
    ) -> Result<
        heddle_git_projection::gateway_received::ReceivedGitProjection,
        heddle_git_projection::GitProjectionError,
    > {
        project_received_git_history(&self.store, &self.originals, tip, ViewLimits::default())
    }
}
#[test]
fn deep_history_exact_boundary_counts_the_canonical_seed() {
    let mut f = Native::new();
    let tree = f.tree(1, b"small");
    let mut tip = synthetic_initial_base().unwrap().id();
    for i in 0..127 {
        tip = f.state(tree, vec![tip], &format!("depth {i}"));
    }
    assert_eq!(
        f.project(tip)
            .expect("127 real States plus canonical seed")
            .history()
            .states()
            .len(),
        127
    );
    tip = f.state(tree, vec![tip], "one beyond the complete-view cap");
    assert!(
        f.project(tip)
            .err()
            .unwrap()
            .to_string()
            .contains("state limit")
    );
}
#[test]
fn merge_dag_is_projected_once_in_parent_before_child_order() {
    let mut f = Native::new();
    let tree = f.tree(1, b"merge");
    let base = f.state(tree, vec![synthetic_initial_base().unwrap().id()], "base");
    let left = f.state(tree, vec![base], "left");
    let right = f.state(tree, vec![base], "right");
    let tip = f.state(tree, vec![left, right], "merge");
    let projection = f.project(tip).expect("complete merge graph");
    assert_eq!(projection.history().states(), &[base, left, right, tip]);
}
#[test]
fn wide_tree_boundary_and_full_history_entry_budget_are_enforced() {
    let mut f = Native::new();
    let tree = f.tree(10_000, b"x");
    let tip = f.state(
        tree,
        vec![synthetic_initial_base().unwrap().id()],
        "at width bound",
    );
    f.project(tip).expect("ten thousand exact entries");
    let tip = f.state(
        tree,
        vec![tip],
        "repeated full tree consumes history budget",
    );
    assert!(
        f.project(tip)
            .err()
            .unwrap()
            .to_string()
            .contains("entry limit")
    );
    let mut f = Native::new();
    let tree = f.tree(10_001, b"x");
    let tip = f.state(
        tree,
        vec![synthetic_initial_base().unwrap().id()],
        "too wide",
    );
    assert!(
        f.project(tip)
            .err()
            .unwrap()
            .to_string()
            .contains("entry limit")
    );
}
#[test]
fn large_blob_exact_boundary_and_combined_history_bytes_are_bounded() {
    let mut f = Native::new();
    let tree = f.tree(1, &vec![b'x'; 16 * 1024 * 1024]);
    let tip = f.state(
        tree,
        vec![synthetic_initial_base().unwrap().id()],
        "16 MiB blob",
    );
    f.project(tip).expect("blob at configured cap");
    let mut f = Native::new();
    let tree = f.tree(1, &vec![b'x'; 16 * 1024 * 1024 + 1]);
    let tip = f.state(
        tree,
        vec![synthetic_initial_base().unwrap().id()],
        "over blob cap",
    );
    assert!(
        f.project(tip)
            .err()
            .unwrap()
            .to_string()
            .contains("blob limit")
    );
    let mut f = Native::new();
    let mut tip = synthetic_initial_base().unwrap().id();
    for i in 0..5 {
        let tree = f.tree(1, &vec![i as u8; 13 * 1024 * 1024]);
        tip = f.state(tree, vec![tip], &format!("aggregate {i}"));
    }
    assert!(
        f.project(tip)
            .err()
            .unwrap()
            .to_string()
            .contains("history byte limit")
    );
}

struct GitFixture {
    _root: tempfile::TempDir,
    native: Repository,
    git: GitRepository,
    base: StateId,
    old: ObjectId,
    publisher: [u8; 32],
}
impl GitFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let native = Repository::init_default(root.path().join("native")).unwrap();
        std::fs::write(native.root().join("base.txt"), b"base\n").unwrap();
        let base = native
            .snapshot_with_attribution(Some("base".into()), None, attribution())
            .unwrap()
            .state_id;
        let git = GitRepository::init_bare(root.path().join("quarantine.git")).unwrap();
        let mapping = export_public_git_history(
            &native,
            &git,
            &[HistoryTip {
                thread: "main",
                state: base,
            }],
            &["main"],
            ViewLimits::default(),
        )
        .unwrap();
        let publisher = native
            .native_thread_signer(&native.native_thread("main").unwrap())
            .unwrap()
            .public_key()
            .try_into()
            .unwrap();
        Self {
            _root: root,
            native,
            git,
            base,
            old: mapping.get_git(&base).unwrap(),
            publisher,
        }
    }
    fn tree(&self, entries: &[(&[u8], &[u8], &[u8])]) -> ObjectId {
        let mut entries = entries.to_vec();
        entries.sort_by(|a, b| a.1.cmp(b.1));
        let mut raw = vec![];
        for (mode, name, content) in entries {
            let id = self.git.write_blob(content).unwrap();
            raw.extend_from_slice(mode);
            raw.push(b' ');
            raw.extend_from_slice(name);
            raw.push(0);
            raw.extend_from_slice(id.as_bytes());
        }
        self.git.write_raw_object(GitObjectType::Tree, raw).unwrap()
    }
    fn commit(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        headers: &[u8],
        message: &[u8],
    ) -> ObjectId {
        let mut raw = format!("tree {tree}\n").into_bytes();
        for p in parents {
            raw.extend_from_slice(format!("parent {p}\n").as_bytes());
        }
        raw.extend_from_slice(b"author Synthetic Git <git@example.invalid> 1700000000 -0730\ncommitter Synthetic Git <git@example.invalid> 1700000031 +0545\n");
        raw.extend_from_slice(headers);
        raw.push(b'\n');
        raw.extend_from_slice(message);
        self.git
            .write_raw_object(GitObjectType::Commit, raw)
            .unwrap()
    }
    fn prepare(
        &self,
        new: ObjectId,
    ) -> Result<PreparedGitPush, heddle_git_projection::GitProjectionError> {
        prepare_git_push(
            &self.native,
            &self.git,
            LocalPush {
                update: &PushUpdate {
                    thread: "main".into(),
                    old: self.old,
                    new,
                },
                expected_native: self.base,
                policy_generation: "synthetic-corpus-policy",
            },
            PushAuthor {
                actor: "synthetic-corpus",
                publisher: self.publisher,
                source_author: &SourceAuthor::LocalKey,
            },
            WriteLimits::default(),
            |_| Ok(()),
        )
    }
}
#[test]
fn unicode_case_aliases_symlinks_executable_and_lfs_pointer_bytes_preserve_git_identity() {
    let f = GitFixture::new();
    let lfs=b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 123456\n";
    let tree = f.tree(&[
        (b"100644", b"README", b"upper"),
        (b"100644", b"Readme", b"mixed"),
        (b"100644", "caf\u{e9}.txt".as_bytes(), b"unicode"),
        (b"100644", "cafe\u{301}.txt".as_bytes(), b"decomposed"),
        (b"100755", b"script.sh", b"#!/bin/sh\nexit 0\n"),
        (b"120000", b"link", b"../target"),
        (b"100644", b"large.asset", lfs),
    ]);
    let new = f.commit(
        tree,
        &[f.old],
        b"",
        b"strange but byte-exact names and modes\n",
    );
    let prepared = f
        .prepare(new)
        .expect("all file bytes and modes are representable");
    assert_eq!(prepared.receipt().new_git, new.to_string());
    assert_eq!(
        f.native
            .native_thread("main")
            .unwrap()
            .projection()
            .unwrap()
            .source_heads,
        [f.base],
        "preparation grants no native acceptance"
    );
}
#[test]
fn empty_commits_renames_and_unusual_commit_headers_round_trip_exactly() {
    let f = GitFixture::new();
    let empty = f.tree(&[]);
    let first = f.commit(empty, &[f.old], b"", b"empty tree\n");
    f.prepare(first)
        .expect("existing branch can become an empty tree");
    let renamed = f.tree(&[(b"100644", b"renamed.txt", b"base\n")]);
    let renamed = f.commit(
        renamed,
        &[f.old],
        b"",
        b"rename represented as an exact tree\n",
    );
    f.prepare(renamed).expect("rename tree accepted");
    let same_tree = f.tree(&[(b"100644", b"base.txt", b"base\n")]);
    let unusual=f.commit(same_tree,&[f.old],b"encoding ISO-8859-1\nx-corpus one\n continuation\ngpgsig -----BEGIN PGP SIGNATURE-----\n synthetic-not-a-valid-signature\n -----END PGP SIGNATURE-----\n",b"raw message \xff\n");
    f.prepare(unusual)
        .expect("commit metadata retained byte-exact; signature text grants no identity");
}
#[test]
fn non_utf8_names_and_reserved_case_aliases_are_rejected_without_native_head_changes() {
    let f = GitFixture::new();
    for name in [b"bad-\xff".as_slice(), b".GIT", b".HeDdLe"] {
        let tree = f.tree(&[(b"100644", name, b"x")]);
        let new = f.commit(tree, &[f.old], b"", b"invalid name\n");
        assert!(
            f.prepare(new).is_err(),
            "unrepresentable or reserved name {name:?}"
        );
    }
    let tree = f.tree(&[(b"120000", b".gitmodules", b"outside")]);
    let new = f.commit(tree, &[f.old], b"", b"reserved symlink\n");
    assert!(
        f.prepare(new)
            .err()
            .unwrap()
            .to_string()
            .contains("reserved")
    );
    assert_eq!(
        f.native
            .native_thread("main")
            .unwrap()
            .projection()
            .unwrap()
            .source_heads,
        [f.base]
    );
}
#[test]
fn gitlinks_and_oversized_merge_parent_lists_are_explicitly_rejected() {
    let f = GitFixture::new();
    let mut raw = b"160000 submodule\0".to_vec();
    raw.extend_from_slice(f.old.as_bytes());
    let tree = f.git.write_raw_object(GitObjectType::Tree, raw).unwrap();
    let new = f.commit(tree, &[f.old], b"", b"gitlink\n");
    assert!(
        f.prepare(new)
            .err()
            .unwrap()
            .to_string()
            .contains("unsupported Git tree mode")
    );
    let tree = f.tree(&[]);
    let new = f.commit(tree, &vec![f.old; 17], b"", b"oversized merge\n");
    assert!(
        f.prepare(new)
            .err()
            .unwrap()
            .to_string()
            .contains("oversized merges")
    );
}
#[test]
fn shallow_alternates_and_missing_partial_clone_content_fail_closed() {
    let f = GitFixture::new();
    let tree = f.tree(&[(b"100644", b"file", b"x")]);
    let new = f.commit(tree, &[f.old], b"", b"complete\n");
    for path in [
        "shallow",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        let p = f.git.git_dir().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"untrusted\n").unwrap();
        assert!(
            f.prepare(new)
                .err()
                .unwrap()
                .to_string()
                .contains("isolated complete SHA-1")
        );
        std::fs::remove_file(p).unwrap();
    }
    let missing: ObjectId = "1111111111111111111111111111111111111111".parse().unwrap();
    let mut tree = b"100644 missing\0".to_vec();
    tree.extend_from_slice(missing.as_bytes());
    let tree = f.git.write_raw_object(GitObjectType::Tree, tree).unwrap();
    assert!(
        f.prepare(f.commit(tree, &[f.old], b"", b"missing promised blob\n"))
            .is_err()
    );
    assert!(
        f.prepare(f.commit(f.tree(&[]), &[missing], b"", b"missing parent\n"))
            .is_err()
    );
}
fn request(reference: &str, old: &str, new: &str, caps: &str) -> Vec<u8> {
    let command = format!("{old} {new} {reference}\0{caps}");
    let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    body.extend_from_slice(b"PACK\0\0\0\x02\0\0\0\0");
    body.extend_from_slice(&[0; 20]);
    body
}
#[test]
fn wire_scope_rejects_unborn_refs_tags_deletes_multi_ref_and_partial_capabilities() {
    let old = "1111111111111111111111111111111111111111";
    let new = "2222222222222222222222222222222222222222";
    let zero = "0000000000000000000000000000000000000000";
    assert!(
        parse_receive_pack(
            &request("refs/heads/main", old, new, "report-status"),
            WriteLimits::default()
        )
        .is_ok(),
        "framing control, not pack checksum/admission proof"
    );
    for body in [
        request("refs/heads/main", zero, new, "report-status"),
        request("refs/heads/main", old, zero, "report-status"),
        request("refs/tags/v1", old, new, "report-status"),
        request("refs/heads/main", old, new, "report-status filter"),
        request("refs/heads/main", old, new, "report-status shallow"),
        request(
            "refs/heads/main",
            old,
            new,
            "report-status object-format=sha256",
        ),
    ] {
        assert!(parse_receive_pack(&body, WriteLimits::default()).is_err());
    }
    let first = format!("{old} {new} refs/heads/main\0report-status");
    let second = format!("{old} {new} refs/heads/other");
    let body = format!(
        "{:04x}{first}{:04x}{second}0000PACK",
        first.len() + 4,
        second.len() + 4
    )
    .into_bytes();
    assert!(parse_receive_pack(&body, WriteLimits::default()).is_err());
}
#[test]
fn deeply_nested_trees_and_unrelated_roots_are_refused() {
    let f = GitFixture::new();
    let mut tree = f.tree(&[]);
    for _ in 0..66 {
        let mut body = b"40000 d\0".to_vec();
        body.extend_from_slice(tree.as_bytes());
        tree = f.git.write_raw_object(GitObjectType::Tree, body).unwrap();
    }
    assert!(
        f.prepare(f.commit(tree, &[f.old], b"", b"too deep\n"))
            .err()
            .unwrap()
            .to_string()
            .contains("tree depth limit")
    );
    assert!(
        f.prepare(f.commit(f.tree(&[]), &[], b"", b"new disconnected root\n"))
            .err()
            .unwrap()
            .to_string()
            .contains("new roots")
    );
}

#[test]
fn many_ref_discovery_is_bounded_and_empty_repository_is_not_an_existing_git_branch() {
    let f = GitFixture::new();
    let sink_root = tempfile::tempdir().unwrap();
    let sink = GitRepository::init_bare(sink_root.path()).unwrap();
    let tips: Vec<_> = (0..129)
        .map(|_| HistoryTip {
            thread: "main",
            state: f.base,
        })
        .collect();
    assert!(
        export_public_git_history(&f.native, &sink, &tips, &["main"], ViewLimits::default())
            .is_err(),
        "read projection cannot advertise an unbounded ref set"
    );
    let empty = Native::new();
    assert!(
        empty
            .project(synthetic_initial_base().unwrap().id())
            .is_err(),
        "no accepted source original means no existing native Git history"
    );
}
