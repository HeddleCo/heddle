//! Byte-exact commit serializer conformance gate (heddle#566, epic #564 step 2).
//!
//! `reconstruct_commit_bytes(state)` must rebuild a git commit object's content
//! byte-for-byte from Heddle state alone — the de-lossy fidelity fields #565
//! captured — so that re-framing (spike §0) and SHA-1-hashing the result
//! reproduces the *original* commit SHA. This is what lets the git mirror be
//! removed (#568): a commit can be replayed from state without a stored object.
//!
//! The corpus is the spike's §9 commit cases. The plain cases are generated
//! in-process with the `git` CLI (deterministic SHAs, no signing key):
//!   * C1 plain commit (normal trailing newline)
//!   * C2 empty message (the `\n\n` separator with a zero-byte body)
//!   * C3 message with NO trailing newline (object ends mid-line)
//!   * C4 CRLF in the message (preserved verbatim)
//!   * C5 unusual/negative timezones with author time/tz != committer time/tz
//!   * C6 non-UTF8 (`encoding ISO-8859-1`) message — a raw `0xe9` latin-1 byte
//!   * C7 octopus (3-parent) merge
//!
//! The two cases that need a GPG key live in a checked-in bundle (generated
//! offline by `tests/commit_conformance_fixtures/gen-commit-corpus.sh`, exactly
//! like the round-trip gate's signed-objects bundle, so CI never needs gpg):
//!   * C8 signed commit (folded `gpgsig` header)
//!   * C9 signed merge carrying a `mergetag` header (mergetag + gpgsig ordering)
//!
//! For every commit reachable in the source, the harness imports it through the
//! real `GitBridge`, calls `reconstruct_commit_bytes`, then asserts BOTH:
//!   1. byte-identity with the original object (`git cat-file commit`), the
//!      debuggable check that pinpoints a diverging header/byte; and
//!   2. the framed SHA-1 equals the original commit SHA (the authoritative
//!      fidelity check).
//!
//! Tag-object reconstruction (`reconstruct_tag_bytes`) is deferred to #575,
//! where annotated tags become first-class content-addressed objects.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use cli::{
    Repository,
    bridge::{git_core::GitBridge, git_reconstruct::commit_object_id, test_support},
};
use sley::{ObjectId, Repository as SleyRepository};
use tempfile::TempDir;

fn ingest_into_bridge(bridge: &mut GitBridge<'_>, source: &Path) -> Result<(), String> {
    let target = test_support::heddle_repo(bridge).root();
    ingest::import_git_into_with_options(source, target, ingest::ImportOptions { lossy: false })
        .map_err(|error| error.to_string())?;
    test_support::build_existing_mapping(bridge, Some(source)).map_err(|error| error.to_string())
}

/// Pinned identity + config so the in-process fixtures produce stable SHAs
/// regardless of when/where the test runs. A default commit date is pinned too
/// so any committing command that forgets to override it still gets a fixed
/// time rather than "now" (which would be non-deterministic).
const ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Heddle Conformance"),
    ("GIT_AUTHOR_EMAIL", "conformance@heddle.test"),
    ("GIT_COMMITTER_NAME", "Heddle Conformance"),
    ("GIT_COMMITTER_EMAIL", "conformance@heddle.test"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
];

/// Run a git command in `dir` (optionally overriding the author/committer dates
/// for a committing command), panicking with stderr on failure. Returns the raw
/// stdout bytes — `git cat-file commit` content can be non-UTF8 (the C6 latin-1
/// case), so callers that need golden bytes must NOT go through a lossy String.
fn run_git(dir: &Path, args: &[&str], dates: Option<(&str, &str)>) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir).envs(ENV.iter().copied());
    if let Some((author, committer)) = dates {
        cmd.env("GIT_AUTHOR_DATE", author)
            .env("GIT_COMMITTER_DATE", committer);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

/// Trimmed stdout of a non-committing git command (rev-list, rev-parse, …).
fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run_git(dir, args, None))
        .trim()
        .to_string()
}

/// A committing git command with explicit author/committer dates.
fn git_dated(dir: &Path, author_date: &str, committer_date: &str, args: &[&str]) {
    run_git(dir, args, Some((author_date, committer_date)));
}

/// Raw object content bytes (no trailing newline added by git).
fn cat_commit(dir: &Path, sha: &str) -> Vec<u8> {
    run_git(dir, &["cat-file", "commit", sha], None)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn all_commit_shas(source: &Path) -> Vec<String> {
    git(source, &["rev-list", "--all"])
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Build the plain (no-GPG) §9 corpus C1..C7 in a single fresh repo.
fn build_plain_corpus(dir: &Path) {
    git(dir, &["init", "-q", "--initial-branch=main"]);

    // C1 — plain commit with a normal trailing newline.
    std::fs::write(dir.join("f"), b"hello\n").unwrap();
    git(dir, &["add", "f"]);
    git_dated(
        dir,
        "1700000000 +0000",
        "1700000000 +0000",
        &["commit", "-q", "-m", "first commit"],
    );

    // C2 — empty message.
    git_dated(
        dir,
        "1700000100 +0000",
        "1700000100 +0000",
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--allow-empty-message",
            "-m",
            "",
        ],
    );

    // C3 — message with NO trailing newline (verbatim cleanup).
    std::fs::write(dir.join("m3"), b"no trailing newline").unwrap();
    git_dated(
        dir,
        "1700000200 +0000",
        "1700000200 +0000",
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--cleanup=verbatim",
            "-F",
            "m3",
        ],
    );

    // C4 — CRLF in the message, preserved verbatim.
    std::fs::write(dir.join("m4"), b"line one\r\nline two\r\n").unwrap();
    git_dated(
        dir,
        "1700000300 +0000",
        "1700000300 +0000",
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--cleanup=verbatim",
            "-F",
            "m4",
        ],
    );

    // C5 — unusual/negative tz, with author time/tz distinct from committer's.
    git_dated(
        dir,
        "1700000400 -0830",
        "1700000450 +1245",
        &["commit", "-q", "--allow-empty", "-m", "weird tz"],
    );

    // C6 — non-UTF8 message body recorded under `encoding ISO-8859-1`.
    git(dir, &["config", "i18n.commitEncoding", "ISO-8859-1"]);
    std::fs::write(dir.join("m6"), b"caf\xe9\n").unwrap();
    git_dated(
        dir,
        "1700000500 +0000",
        "1700000500 +0000",
        &["commit", "-q", "--allow-empty", "-F", "m6"],
    );
    git(dir, &["config", "--unset", "i18n.commitEncoding"]);

    // C7 — octopus (3-parent) merge of two sibling branches off main.
    git(dir, &["checkout", "-q", "-b", "a", "main"]);
    std::fs::write(dir.join("a"), b"a\n").unwrap();
    git(dir, &["add", "a"]);
    git_dated(
        dir,
        "1700000600 +0000",
        "1700000600 +0000",
        &["commit", "-q", "-m", "a"],
    );
    git(dir, &["checkout", "-q", "-b", "b", "main"]);
    std::fs::write(dir.join("b"), b"b\n").unwrap();
    git(dir, &["add", "b"]);
    git_dated(
        dir,
        "1700000700 +0000",
        "1700000700 +0000",
        &["commit", "-q", "-m", "b"],
    );
    git(dir, &["checkout", "-q", "main"]);
    git_dated(
        dir,
        "1700000800 +0000",
        "1700000800 +0000",
        &["merge", "-q", "--no-ff", "-m", "octopus", "a", "b"],
    );
}

/// Materialize the checked-in signed/mergetag bundle (C8/C9) into a fresh repo.
/// The bundle's signed-object SHAs were minted with an ephemeral key, so they
/// are stable now that it is committed — and the harness recomputes every SHA
/// from this live repo rather than hardcoding one.
fn extract_commit_bundle(dir: &Path) -> PathBuf {
    let bundle = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("commit_conformance_fixtures")
        .join("commit-corpus.bundle");
    assert!(
        bundle.exists(),
        "commit-corpus fixture missing: {} (regenerate with \
         tests/commit_conformance_fixtures/gen-commit-corpus.sh)",
        bundle.display()
    );
    let repo = dir.join("corpus");
    std::fs::create_dir_all(&repo).expect("create corpus repo dir");
    git(&repo, &["init", "-q", "--initial-branch=__bootstrap"]);
    git(
        &repo,
        &[
            "fetch",
            "-q",
            bundle.to_str().expect("bundle path utf8"),
            "refs/heads/*:refs/heads/*",
        ],
    );
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    repo
}

/// Import every commit reachable in `source` through the real `GitBridge`, then
/// assert each reconstructs byte-identically AND its framed SHA-1 reproduces the
/// original commit SHA.
fn assert_all_commits_reconstruct(case: &str, source: &Path) {
    // A corrupt fixture would make the comparison meaningless.
    git(source, &["fsck", "--full", "--strict"]);
    let shas = all_commit_shas(source);
    assert!(!shas.is_empty(), "[{case}] no commits to reconstruct");

    let heddle_home = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_home.path()).expect("init heddle repo");
    let mut bridge = GitBridge::new(&repo);
    ingest_into_bridge(&mut bridge, source)
        .unwrap_or_else(|e| panic!("[{case}] import from git failed: {e}"));

    // A writable odb for tree-OID resolution (git trees are content-addressed,
    // so the OID is independent of which repo it lands in).
    let recon_repo = bridge
        .reconstruction_repo()
        .unwrap_or_else(|e| panic!("[{case}] open reconstruction repo failed: {e}"));

    for sha in &shas {
        let golden = cat_commit(source, sha);
        let reconstructed = bridge
            .reconstruct_commit_for_git_sha(&recon_repo, sha)
            .unwrap_or_else(|e| panic!("[{case}] reconstruct {sha} failed: {e}"))
            .unwrap_or_else(|| panic!("[{case}] no Heddle state maps to commit {sha}"));

        // (1) byte-identity with the original object — pinpoints a diverging
        // header/byte on failure.
        assert_eq!(
            reconstructed,
            golden,
            "[{case}] commit {sha} reconstructed to DIFFERENT bytes\n  \
             reconstructed: {:?}\n  golden:        {:?}",
            String::from_utf8_lossy(&reconstructed),
            String::from_utf8_lossy(&golden),
        );

        // (2) the framed SHA-1 reproduces the original object id (the
        // authoritative fidelity check, spike §0).
        assert_eq!(
            commit_object_id(&reconstructed).to_string(),
            *sha,
            "[{case}] commit {sha} framed-SHA mismatch (byte-identity broken)"
        );
    }
}

/// #567 export-from-state gate: every commit reachable in `source` must be
/// REGENERATED from Heddle state into a FRESH repo that has never held the
/// verbatim imported bytes, landing at its ORIGINAL git SHA with byte-identical
/// content. Where [`assert_all_commits_reconstruct`] checks the serializer in
/// isolation (#566), this drives the export's actual reconstruct-and-WRITE step
/// ([`GitBridge::reconstruct_and_write_commit_for_git_sha`]) and proves the
/// minted object no longer depends on the git mirror's verbatim copy — the
/// dependency #568 removes. The fresh repo is asserted empty of each commit
/// BEFORE reconstruction, so a regenerated object appearing there can only have
/// come from Heddle state.
fn assert_all_commits_export_from_state(case: &str, source: &Path) {
    git(source, &["fsck", "--full", "--strict"]);
    let shas = all_commit_shas(source);
    assert!(!shas.is_empty(), "[{case}] no commits to reconstruct");

    let heddle_home = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_home.path()).expect("init heddle repo");
    let mut bridge = GitBridge::new(&repo);
    ingest_into_bridge(&mut bridge, source)
        .unwrap_or_else(|e| panic!("[{case}] import from git failed: {e}"));

    // A FRESH bare repo, separate from the bridge mirror: it has never held any
    // commit object, so a regenerated commit landing here is provably rebuilt
    // from state rather than copied from the mirror's verbatim import.
    let fresh_home = TempDir::new().expect("fresh temp");
    let fresh = SleyRepository::init_bare(fresh_home.path().join("fresh.git"))
        .unwrap_or_else(|e| panic!("[{case}] init fresh bare repo failed: {e}"));

    for sha in &shas {
        let oid = ObjectId::from_hex(sley::ObjectFormat::Sha1, sha)
            .unwrap_or_else(|e| panic!("[{case}] bad sha {sha}: {e}"));
        assert!(
            fresh.read_object(&oid).is_err(),
            "[{case}] fresh repo unexpectedly already holds commit {sha} before \
             reconstruction — the from-state independence guarantee is void"
        );

        let written = bridge
            .reconstruct_and_write_commit_for_git_sha(&fresh, sha)
            .unwrap_or_else(|e| panic!("[{case}] reconstruct+write {sha} failed: {e}"))
            .unwrap_or_else(|| panic!("[{case}] no Heddle state maps to commit {sha}"));

        // (1) the regenerated object lands at the ORIGINAL git SHA ...
        assert_eq!(
            written.to_string(),
            *sha,
            "[{case}] commit {sha} regenerated to a DIFFERENT object {written}"
        );
        // (2) ... and now physically exists in the fresh repo (written from
        // state, not copied) ...
        assert!(
            fresh.read_object(&written).is_ok(),
            "[{case}] commit {sha} absent from fresh repo after reconstruct+write"
        );
        // (3) ... byte-identical to git's own view of the original object.
        let golden = cat_commit(source, sha);
        let object = fresh
            .read_object(&written)
            .unwrap_or_else(|e| panic!("[{case}] find regenerated {sha} failed: {e}"));
        assert_eq!(
            object.body,
            golden,
            "[{case}] commit {sha} regenerated to DIFFERENT bytes\n  \
             reconstructed: {:?}\n  golden:        {:?}",
            String::from_utf8_lossy(&object.body),
            String::from_utf8_lossy(&golden),
        );
    }
}

#[test]
fn commit_conformance_plain_corpus() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    build_plain_corpus(dir);

    // Guard against silent corpus shrinkage: the trickiest cases must actually
    // be present, or a generation regression would let the gate pass without
    // exercising them.
    let bodies: Vec<Vec<u8>> = all_commit_shas(dir)
        .iter()
        .map(|sha| cat_commit(dir, sha))
        .collect();
    assert!(
        bodies
            .iter()
            .any(|b| contains(b, b"\nencoding ISO-8859-1\n")),
        "C6 non-UTF8 encoding case missing"
    );
    assert!(
        bodies.iter().any(|b| b.contains(&0xe9)),
        "C6 raw latin-1 0xe9 message byte missing"
    );
    assert!(
        bodies.iter().any(|b| b.ends_with(b"\n\n")),
        "C2 empty-message case missing"
    );
    assert!(
        bodies.iter().any(|b| b.ends_with(b"no trailing newline")),
        "C3 no-trailing-newline case missing"
    );
    assert!(
        bodies.iter().any(|b| contains(b, b"\r\n")),
        "C4 CRLF case missing"
    );
    assert!(
        bodies
            .iter()
            .any(|b| contains(b, b" -0830\n") && contains(b, b" +1245\n")),
        "C5 weird/negative-tz case missing"
    );
    assert!(
        bodies.iter().any(|b| count(b, b"\nparent ") >= 3),
        "C7 octopus (3-parent) case missing"
    );

    assert_all_commits_reconstruct("plain-corpus", dir);
}

#[test]
fn commit_conformance_signed_and_mergetag() {
    let tmp = TempDir::new().unwrap();
    let source = extract_commit_bundle(tmp.path());

    // Guard: the signed commit (folded gpgsig) and the signed merge (mergetag +
    // gpgsig) must be present, so a future bundle refresh that drops either
    // fails loudly rather than passing without exercising signatures.
    let main = git(&source, &["rev-parse", "refs/heads/main"]);
    let merge = cat_commit(&source, &main);
    assert!(
        contains(&merge, b"\nmergetag "),
        "C9 fixture lost the mergetag header:\n{}",
        String::from_utf8_lossy(&merge)
    );
    assert!(
        contains(&merge, b"\ngpgsig "),
        "C9 fixture lost the gpgsig header on the merge"
    );
    let has_signed_commit = all_commit_shas(&source)
        .iter()
        .any(|sha| contains(&cat_commit(&source, sha), b"\ngpgsig "));
    assert!(
        has_signed_commit,
        "C8 fixture lost a signed commit (no gpgsig header in any commit)"
    );

    assert_all_commits_reconstruct("signed-and-mergetag", &source);
}

/// #567: the plain §9 corpus (C1..C7) must export-FROM-STATE — every commit
/// regenerated into a fresh repo at its original SHA, byte-identical — covering
/// empty message, no-trailing-newline, CRLF, weird/negative tz, non-UTF8
/// encoding, and an octopus merge.
#[test]
fn export_from_state_plain_corpus() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    build_plain_corpus(dir);
    assert_all_commits_export_from_state("plain-corpus", dir);
}

/// #567: the signed commit (folded `gpgsig`) and signed merge (`mergetag` +
/// `gpgsig`) — the most error-prone fidelity cases — must likewise export
/// byte-identically from state, with no mirror verbatim copy involved.
#[test]
fn export_from_state_signed_and_mergetag() {
    let tmp = TempDir::new().unwrap();
    let source = extract_commit_bundle(tmp.path());
    assert_all_commits_export_from_state("signed-and-mergetag", &source);
}
