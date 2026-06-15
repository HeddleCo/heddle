//! Round-trip fidelity conformance gate (heddle#533).
//!
//! heddle is a lossless git overlay. For a **public** repo (no visibility
//! tiers — absence ≡ public, so nothing is gated), adopting/importing a git
//! repo and exporting it back must reproduce **byte-identical git object
//! SHAs**: identical commit, tree, blob, and annotated-tag object IDs, and a
//! `git fsck --full` clean export. This is heddle's foundational promise; a
//! regression here must fail CI the instant it lands.
//!
//! Each fixture builds a small, deterministic real git repo (fixed
//! author/committer identity + dates so SHAs are stable), records every
//! object SHA reachable from every ref, runs `import` → `export_to_path`
//! through the same `GitBridge` surface real users drive, then asserts:
//!   1. every ref (branch / tag / note) in the source is present in the
//!      export pointing at the **same** object id — which, by git's
//!      content-addressing, transitively proves every reachable commit /
//!      tree / blob / tag object is byte-identical;
//!   2. every commit/tree/blob object reachable in the source is present in
//!      the export with an identical SHA (an explicit object-set check on
//!      top of the transitive ref-tip guarantee); and
//!   3. `git fsck --full` on the export reports no corruption.

use std::{collections::BTreeMap, path::Path, process::Command};

use cli::{
    ObjectStore, Repository,
    bridge::{git_core::GitBridge, test_support},
};
use tempfile::TempDir;

fn ingest_into_bridge(
    bridge: &mut GitBridge<'_>,
    source: &Path,
    lossy: bool,
) -> Result<(), String> {
    let target = test_support::heddle_repo(bridge).root();
    ingest::import_git_into_with_options(source, target, ingest::ImportOptions { lossy })
        .map_err(|error| error.to_string())?;
    test_support::build_existing_mapping(bridge, Some(source)).map_err(|error| error.to_string())
}

/// Deterministic identity + dates so every fixture produces stable SHAs
/// regardless of when/where the test runs. A drifting SHA here would be a
/// test bug, not a fidelity bug — pin everything.
const ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Heddle Conformance"),
    ("GIT_AUTHOR_EMAIL", "conformance@heddle.test"),
    ("GIT_COMMITTER_NAME", "Heddle Conformance"),
    ("GIT_COMMITTER_EMAIL", "conformance@heddle.test"),
    ("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200"),
    ("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200"),
    // Pin everything else that can perturb object bytes or ref layout.
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
];

/// Run a git command in `dir`, panicking with stderr on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(ENV.iter().copied())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Initialise a non-bare repo with a fixed initial branch (so HEAD and the
/// default branch name don't depend on the runner's git config).
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "--initial-branch=main"]);
}

/// Write a file (creating parent dirs), `git add`, then `git commit`.
fn write_and_commit(dir: &Path, path: &str, contents: &[u8], msg: &str) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(&full, contents).expect("write fixture file");
    git(dir, &["add", "--", path]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// Map of `refname -> objectname` for every ref (branches, tags — annotated
/// tags resolve to the tag-object SHA, lightweight to the commit SHA — and
/// notes). Ref-tip equality is the strong, transitive fidelity assertion:
/// matching a commit SHA proves the entire reachable tree/blob graph is
/// byte-identical, and matching an annotated-tag SHA proves the tag object
/// bytes match.
fn ref_map(dir: &Path) -> BTreeMap<String, String> {
    let raw = git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    raw.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(name, oid)| (name.to_string(), oid.to_string()))
        .collect()
}

/// Set of every commit/tree/blob SHA reachable from every ref. `--all`
/// covers refs/heads, refs/tags, and refs/notes. This is the explicit
/// per-object check layered on top of the transitive ref-tip guarantee.
fn object_set(dir: &Path) -> Vec<String> {
    let raw = git(dir, &["rev-list", "--objects", "--all"]);
    let mut ids: Vec<String> = raw
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|s| s.to_string())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Adopt `source` into a fresh heddle repo, export it back to a fresh bare
/// repo, and assert byte-identical SHAs + fsck-clean. `case` names the
/// fixture for failure messages.
fn assert_roundtrip_fidelity(case: &str, source: &Path) {
    assert_roundtrip_fidelity_opts(case, source, false);
}

/// As [`assert_roundtrip_fidelity`], but `lossy` opts into the explicit
/// `--lossy` import surface. Gitlinks (submodules) are the one tree-entry
/// kind heddle refuses to import silently — it converts them to a
/// `heddle-submodule` blob only under the opt-in, then export reconstitutes
/// the gitlink. The fidelity bar is unchanged: the round-trip must still be
/// byte-identical.
fn assert_roundtrip_fidelity_opts(case: &str, source: &Path, lossy: bool) {
    // git fsck the source first: a corrupt fixture would make the whole
    // comparison meaningless.
    git(source, &["fsck", "--full", "--strict"]);

    let source_refs = ref_map(source);
    assert!(
        !source_refs.is_empty(),
        "[{case}] fixture has no refs to round-trip"
    );
    let source_objects = object_set(source);

    let heddle_home = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_home.path()).expect("init heddle repo");
    let mut bridge = GitBridge::new(&repo);
    ingest_into_bridge(&mut bridge, source, lossy)
        .unwrap_or_else(|e| panic!("[{case}] import from git failed: {e}"));

    let dest_home = TempDir::new().expect("dest temp");
    let dest = dest_home.path().join("export");
    bridge
        .export_to_path(&dest)
        .unwrap_or_else(|e| panic!("[{case}] export_to_path failed: {e}"));

    // (3) the export must be a structurally sound git repo.
    git(&dest, &["fsck", "--full", "--strict"]);

    // (1) every source ref must reappear in the export at the identical
    // object id. This is the load-bearing fidelity assertion.
    let export_refs = ref_map(&dest);
    for (name, oid) in &source_refs {
        match export_refs.get(name) {
            Some(got) => assert_eq!(
                got, oid,
                "[{case}] ref {name} round-tripped to a DIFFERENT object: \
                 source {oid} != export {got} (byte-identity broken)"
            ),
            None => panic!(
                "[{case}] ref {name} (-> {oid}) was DROPPED on round-trip; \
                 export refs: {export_refs:?}"
            ),
        }
    }

    // (2) explicit per-object check: every reachable commit/tree/blob in
    // the source must exist verbatim in the export.
    let export_objects = object_set(&dest);
    for oid in &source_objects {
        assert!(
            export_objects.contains(oid),
            "[{case}] object {oid} present in source but MISSING from export \
             (byte-identity broken)"
        );
    }
}

#[test]
fn roundtrip_linear_history() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "a.txt", b"first\n", "c1");
    write_and_commit(dir, "b.txt", b"second\n", "c2");
    write_and_commit(dir, "a.txt", b"first updated\n", "c3");
    assert_roundtrip_fidelity("linear", dir);
}

#[test]
fn roundtrip_two_parent_merge() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "base.txt", b"base\n", "base");
    git(dir, &["checkout", "-q", "-b", "feature"]);
    write_and_commit(dir, "feature.txt", b"feature\n", "feature work");
    git(dir, &["checkout", "-q", "main"]);
    write_and_commit(dir, "main.txt", b"main\n", "main work");
    git(
        dir,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    assert_roundtrip_fidelity("two-parent-merge", dir);
}

#[test]
fn roundtrip_octopus_merge() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "base.txt", b"base\n", "base");
    for branch in ["b1", "b2", "b3"] {
        git(dir, &["checkout", "-q", "-b", branch, "main"]);
        write_and_commit(dir, &format!("{branch}.txt"), b"x\n", branch);
    }
    git(dir, &["checkout", "-q", "main"]);
    // >2-parent (octopus) merge of three sibling branches.
    git(
        dir,
        &["merge", "-q", "--no-ff", "-m", "octopus", "b1", "b2", "b3"],
    );
    let parents = git(dir, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert!(
        parents.split_whitespace().count() >= 4,
        "expected an octopus (>2-parent) merge, got: {parents}"
    );
    assert_roundtrip_fidelity("octopus-merge", dir);
}

#[test]
fn roundtrip_annotated_and_lightweight_tags() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "f.txt", b"v1\n", "release base");
    // Lightweight tag (objectname == commit SHA).
    git(dir, &["tag", "v1.0-light"]);
    write_and_commit(dir, "f.txt", b"v2\n", "release follow-up");
    // Annotated tag (objectname == tag-object SHA; must round-trip verbatim).
    git(dir, &["tag", "-a", "v2.0", "-m", "annotated release v2.0"]);
    assert_roundtrip_fidelity("tags", dir);
}

/// Materialize the checked-in signed-object bundle into a fresh working repo
/// under `dir` and return the path to it. The bundle (see
/// `tests/roundtrip_fidelity_fixtures/gen-signed-objects.sh`) was generated
/// once with an ephemeral GPG key, so its signed-object SHAs are stable now
/// that it is committed — but the caller never hardcodes them; the fidelity
/// assertion recomputes every SHA from this live repo.
///
/// We `init` on a throwaway branch then `fetch` the bundle's heads + tags so
/// the ref layout is clean (`refs/heads/*` + `refs/tags/*`, no `refs/remotes/*`
/// that a `git clone` from a bundle would introduce), matching the in-process
/// fixtures above.
fn extract_signed_bundle(dir: &Path) -> std::path::PathBuf {
    let bundle = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("roundtrip_fidelity_fixtures")
        .join("signed-objects.bundle");
    assert!(
        bundle.exists(),
        "signed-object fixture missing: {} (regenerate with \
         tests/roundtrip_fidelity_fixtures/gen-signed-objects.sh)",
        bundle.display()
    );
    let repo = dir.join("signed");
    std::fs::create_dir_all(&repo).expect("create signed repo dir");
    git(&repo, &["init", "-q", "--initial-branch=__bootstrap"]);
    git(
        &repo,
        &[
            "fetch",
            "-q",
            bundle.to_str().expect("bundle path utf8"),
            "refs/heads/*:refs/heads/*",
            "refs/tags/*:refs/tags/*",
        ],
    );
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    repo
}

/// Signed commits (folded `gpgsig` header) and signed annotated tags
/// (signature appended unfolded in the tag body) are the most error-prone
/// fidelity cases. The vendored real-world fixtures deliberately can't cover
/// them: they are shallow clones re-rooted through `fast-export | fast-import`,
/// which rewrites commit OIDs and so invalidates any tag signature it carries —
/// `vendor.sh` therefore `--signed-tags=strip`s them rather than shipping a
/// fixture that looks signed but fails `git verify-tag` (heddle#562). This
/// directly-generated fixture (never passed through fast-export, so its
/// signatures stay valid — and `gen-signed-objects.sh` `git verify-commit`/
/// `verify-tag`s the bundle at generation time) is the authoritative signed
/// coverage. It feeds the signed objects through the same adopt → export
/// round-trip and asserts the signature bytes survive verbatim (identical
/// commit + tag-object SHAs). A failure here is a real fidelity bug: export is
/// not preserving the `gpgsig` / tag-body signature bytes.
#[test]
fn roundtrip_signed_commit_and_tag() {
    let tmp = TempDir::new().unwrap();
    let source = extract_signed_bundle(tmp.path());

    // Guard against a silent no-op: the fixture must actually carry a signed
    // commit (folded gpgsig header) and a signed annotated tag (inline PGP
    // signature in the tag body). If a future bundle refresh drops either,
    // this fails loudly rather than passing without testing signatures.
    let main_oid = git(&source, &["rev-parse", "refs/heads/main"]);
    let commit_obj = git(&source, &["cat-file", "commit", main_oid.trim()]);
    assert!(
        commit_obj.lines().any(|l| l.starts_with("gpgsig ")),
        "signed-object fixture lost its signed commit (no gpgsig header):\n{commit_obj}"
    );
    let tag_oid = git(&source, &["rev-parse", "refs/tags/v1.0"]);
    let tag_obj = git(&source, &["cat-file", "tag", tag_oid.trim()]);
    assert!(
        tag_obj.contains("-----BEGIN PGP SIGNATURE-----"),
        "signed-object fixture lost its signed annotated tag (no inline signature):\n{tag_obj}"
    );

    assert_roundtrip_fidelity("signed-commit-and-tag", &source);
}

#[test]
fn roundtrip_notes() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "f.txt", b"noted\n", "commit with a note");
    git(dir, &["notes", "add", "-m", "a code-review note", "HEAD"]);
    // Confirm the fixture actually created refs/notes/commits before relying
    // on it for the round-trip assertion.
    let refs = ref_map(dir);
    assert!(
        refs.keys().any(|r| r.starts_with("refs/notes/")),
        "fixture failed to create a notes ref: {refs:?}"
    );
    assert_roundtrip_fidelity("notes", dir);
}

#[test]
fn roundtrip_submodule_gitlink() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "top.txt", b"super\n", "superproject base");

    // Build an independent commit to act as the submodule's pinned tip, then
    // splice a gitlink (mode 160000 tree entry) pointing at it directly via
    // the index — no network, no nested clone, fully deterministic.
    let sub = TempDir::new().unwrap();
    init_repo(sub.path());
    write_and_commit(sub.path(), "lib.txt", b"library\n", "submodule base");
    let sub_oid = git(sub.path(), &["rev-parse", "HEAD"]);
    let sub_oid = sub_oid.trim();

    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},vendor/lib"),
        ],
    );
    git(dir, &["commit", "-q", "-m", "add submodule gitlink"]);

    // Verify the gitlink really landed as a commit-type tree entry.
    let ls = git(dir, &["ls-tree", "HEAD", "vendor/lib"]);
    assert!(
        ls.contains("160000 commit"),
        "expected a 160000 gitlink tree entry, got: {ls}"
    );
    // Gitlinks are heddle's one opt-in lossy tree-entry kind; the round-trip
    // must still reproduce the gitlink (and thus the tree/commit SHA) exactly.
    assert_roundtrip_fidelity_opts("submodule-gitlink", dir, true);
}

#[test]
fn roundtrip_binary_blob() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    // Non-UTF-8 bytes incl. NULs and the full byte range.
    let mut blob = vec![0u8, 1, 2, 3, 255, 254, 0, 0, 0, 10, 13];
    blob.extend((0..=255u8).cycle().take(1024));
    write_and_commit(dir, "data.bin", &blob, "add binary blob");
    assert_roundtrip_fidelity("binary-blob", dir);
}

#[test]
fn roundtrip_unicode_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    // core.quotepath off so the names are written raw; SHA fidelity is the
    // real check regardless of how git renders them.
    git(dir, &["config", "core.quotepath", "false"]);
    write_and_commit(dir, "café/résumé.txt", "naïve\n".as_bytes(), "unicode path");
    write_and_commit(
        dir,
        "日本語/ファイル.txt",
        "こんにちは\n".as_bytes(),
        "cjk path",
    );
    write_and_commit(dir, "emoji-🚀.txt", b"rocket\n", "emoji path");
    assert_roundtrip_fidelity("unicode-paths", dir);
}

#[test]
fn roundtrip_executable_bit() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    let script = dir.join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
    git(dir, &["add", "--", "run.sh"]);
    // Force the exec bit through the index so the test is OS-independent
    // (the 100755 file mode must round-trip).
    git(dir, &["update-index", "--chmod=+x", "run.sh"]);
    git(dir, &["commit", "-q", "-m", "add executable script"]);
    let ls = git(dir, &["ls-tree", "HEAD", "run.sh"]);
    assert!(
        ls.starts_with("100755"),
        "expected a 100755 executable entry, got: {ls}"
    );
    assert_roundtrip_fidelity("executable-bit", dir);
}

#[test]
fn roundtrip_empty_and_nested_trees() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    // A near-empty tree: a single file at root, then a deeply nested tree
    // with a single leaf (exercises tree recursion + minimal trees).
    write_and_commit(dir, "only.txt", b"x\n", "single-file tree");
    write_and_commit(dir, "a/b/c/d/leaf.txt", b"deep\n", "deeply nested tree");
    assert_roundtrip_fidelity("trees", dir);
}

/// Run a git command feeding `stdin`, returning trimmed stdout. The `git`
/// helper above can't write stdin, which `hash-object --stdin` needs.
fn git_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    use std::{io::Write, process::Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(ENV.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().expect("wait git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Close-the-class conformance (bridge path): a commit whose extension headers
/// are in NON-canonical order — `x-custom`, then a folded `gpgsig`, then
/// `encoding`, then a folded `mergetag` — must import into `State.extra_headers`
/// in that exact on-the-wire order with byte-exact values. Real git always
/// emits `encoding` first, so this order is hand-crafted with `hash-object
/// --literally`; it is precisely what a typed-field-stitching importer mangles.
/// Mirrors the ingest-path test and the shared parser's unit test, so both
/// import paths are proven consistent.
#[test]
fn import_preserves_noncanonical_extension_header_order() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "f.txt", b"base\n", "base commit");

    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let head = git(dir, &["rev-parse", "HEAD"]).trim().to_string();

    let content: Vec<u8> = [
        format!("tree {tree}").into_bytes(),
        format!("parent {head}").into_bytes(),
        b"author Alice <alice@example.com> 1700000000 +0000".to_vec(),
        b"committer Bob <bob@example.com> 1700000100 +0000".to_vec(),
        b"x-custom custom value".to_vec(),
        b"gpgsig -----BEGIN PGP SIGNATURE-----".to_vec(),
        b" sig-line-1".to_vec(),
        b" -----END PGP SIGNATURE-----".to_vec(),
        b"encoding ISO-8859-1".to_vec(),
        b"mergetag object 3333333333333333333333333333333333333333".to_vec(),
        b" type commit".to_vec(),
        b" tag sidetag".to_vec(),
        b"".to_vec(),
        b"the commit message".to_vec(),
        b"".to_vec(),
    ]
    .join(&b'\n');

    let sha = git_stdin(
        dir,
        &[
            "hash-object",
            "--literally",
            "-w",
            "-t",
            "commit",
            "--stdin",
        ],
        &content,
    );
    git(dir, &["update-ref", "refs/heads/crafted", &sha]);

    let heddle_home = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_home.path()).expect("init heddle repo");
    let mut bridge = GitBridge::new(&repo);
    ingest_into_bridge(&mut bridge, dir, false).expect("import from git failed");

    // Re-open to read the freshly installed states without a borrow tangle.
    let repo = Repository::open(heddle_home.path()).expect("reopen heddle repo");
    let with_headers: Vec<_> = repo
        .store()
        .list_states()
        .expect("list states")
        .iter()
        .filter_map(|id| repo.store().get_state(id).expect("get state"))
        .filter(|s| !s.extra_headers.is_empty())
        .collect();

    assert_eq!(
        with_headers.len(),
        1,
        "only the crafted commit carries extension headers"
    );
    let expected: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (b"x-custom".to_vec(), b"custom value".to_vec()),
        (
            b"gpgsig".to_vec(),
            b"-----BEGIN PGP SIGNATURE-----\nsig-line-1\n-----END PGP SIGNATURE-----".to_vec(),
        ),
        (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
        (
            b"mergetag".to_vec(),
            b"object 3333333333333333333333333333333333333333\ntype commit\ntag sidetag".to_vec(),
        ),
    ];
    assert_eq!(with_headers[0].extra_headers, expected);
}

/// Fidelity-guard boundary (#567): a commit whose AUTHOR identity carries a raw
/// non-UTF8 byte cannot be reconstructed byte-exactly from Heddle state, because
/// `Principal.name` is a `String` and import lossily replaces the byte with
/// U+FFFD (the #564-deferred gap). Export must detect this and serve the
/// VERBATIM mirror bytes at the original OID rather than mint a reconstructed
/// (wrong-SHA) object. A regression that reconstructs unconditionally fails this
/// round-trip (the commit OID drifts) — and the pre-fix #567 code errored out of
/// the export entirely on the mapped-OID mismatch.
#[test]
fn roundtrip_non_utf8_author_identity() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "f.txt", b"base\n", "base commit");

    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let head = git(dir, &["rev-parse", "HEAD"]).trim().to_string();

    // `\xff` is never valid UTF-8, so `author.name.to_string()` on import maps it
    // to U+FFFD; the committer is left clean to isolate the author-identity case.
    let mut content = Vec::new();
    content.extend_from_slice(format!("tree {tree}\n").as_bytes());
    content.extend_from_slice(format!("parent {head}\n").as_bytes());
    content.extend_from_slice(b"author Sven \xff <sven@example.com> 1700000000 +0000\n");
    content.extend_from_slice(b"committer Bob <bob@example.com> 1700000100 +0000\n");
    content.extend_from_slice(b"\n");
    content.extend_from_slice(b"commit with a non-UTF8 author name\n");
    assert!(content.contains(&0xff), "fixture lost its non-UTF8 byte");

    let sha = git_stdin(
        dir,
        &[
            "hash-object",
            "--literally",
            "-w",
            "-t",
            "commit",
            "--stdin",
        ],
        &content,
    );
    git(dir, &["update-ref", "refs/heads/crafted", &sha]);

    assert_roundtrip_fidelity("non-utf8-author-identity", dir);
}

/// As [`roundtrip_non_utf8_author_identity`], for the COMMITTER identity — the
/// guard checks the distinct `State.committer` principal too, so a non-UTF8
/// committer name must likewise route to the verbatim-mirror fallback.
#[test]
fn roundtrip_non_utf8_committer_identity() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_and_commit(dir, "f.txt", b"base\n", "base commit");

    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    let head = git(dir, &["rev-parse", "HEAD"]).trim().to_string();

    let mut content = Vec::new();
    content.extend_from_slice(format!("tree {tree}\n").as_bytes());
    content.extend_from_slice(format!("parent {head}\n").as_bytes());
    content.extend_from_slice(b"author Alice <alice@example.com> 1700000000 +0000\n");
    content.extend_from_slice(b"committer Lars \xff <lars@example.com> 1700000100 +0000\n");
    content.extend_from_slice(b"\n");
    content.extend_from_slice(b"commit with a non-UTF8 committer name\n");
    assert!(content.contains(&0xff), "fixture lost its non-UTF8 byte");

    let sha = git_stdin(
        dir,
        &[
            "hash-object",
            "--literally",
            "-w",
            "-t",
            "commit",
            "--stdin",
        ],
        &content,
    );
    git(dir, &["update-ref", "refs/heads/crafted", &sha]);

    assert_roundtrip_fidelity("non-utf8-committer-identity", dir);
}
