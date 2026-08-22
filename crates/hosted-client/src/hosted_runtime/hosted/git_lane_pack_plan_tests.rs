// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use sley::{
    CommitObject, EntryKind, GitObjectType, ObjectFormat, ObjectId as GitObjectId,
    Repository as SleyRepository,
    plumbing::{sley_object::EncodedObject, sley_odb, sley_pack::PackFile},
};
use tempfile::TempDir;

use super::{build_git_lane_multi_root_pack_plan, write_git_lane_reachable_pack};

const TEST_IDENT: &[u8] = b"Heddle Test <heddle@test> 0 +0000";

struct PackedFixture {
    _temp: TempDir,
    repo: SleyRepository,
    first: GitObjectId,
    second: GitObjectId,
    first_blob: GitObjectId,
    second_blob: GitObjectId,
}

fn write_commit(
    repo: &SleyRepository,
    name: &str,
    data: &[u8],
    parents: &[GitObjectId],
) -> (GitObjectId, GitObjectId) {
    let blob = repo.write_blob(data.to_vec()).expect("write blob");
    let empty = GitObjectId::empty_tree(repo.object_format());
    let mut tree = repo.edit_tree(&empty).expect("edit tree");
    tree.upsert(name, EntryKind::Blob, blob);
    let tree_oid = repo.write_tree(tree).expect("write tree");
    let commit = CommitObject {
        tree: tree_oid,
        parents: parents.to_vec(),
        author: TEST_IDENT.to_vec(),
        committer: TEST_IDENT.to_vec(),
        encoding: None,
        message: format!("commit {name}\n").into_bytes(),
    };
    let oid = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("write commit");
    (oid, blob)
}

fn install_reachable_pack(repo: &SleyRepository, roots: &[GitObjectId]) {
    let objects = repo.objects();
    let pack = sley_odb::build_reachable_pack(
        objects.as_ref(),
        repo.object_format(),
        roots.iter().copied(),
        &HashSet::new(),
    )
    .expect("build source pack")
    .expect("non-empty source pack");
    objects.install_pack(&pack).expect("install source pack");
    repo.refresh_objects();
}

fn packed_history() -> PackedFixture {
    let temp = TempDir::new().expect("temp git repo");
    let repo = SleyRepository::init(temp.path()).expect("init git repo");
    let (first, first_blob) = write_commit(&repo, "a.bin", &vec![b'a'; 16_384], &[]);
    let mut second_body = vec![b'a'; 16_384];
    second_body[100..200].fill(b'b');
    let (second, second_blob) = write_commit(&repo, "a.bin", &second_body, &[first]);
    install_reachable_pack(&repo, &[second]);
    PackedFixture {
        _temp: temp,
        repo,
        first,
        second,
        first_blob,
        second_blob,
    }
}

fn rebuild_without_reuse(
    repo: &SleyRepository,
    roots: &[GitObjectId],
    excluded: &[GitObjectId],
) -> Option<Vec<u8>> {
    let plan = repo
        .reachable_pack_plan()
        .roots(roots.iter().copied())
        .exclusions(excluded.iter().copied())
        .build()
        .expect("plan old reachable pack")?;
    Some(plan.prepare_to_memory().expect("rebuild old pack").pack)
}

fn parse_objects(bytes: &[u8], format: ObjectFormat) -> HashMap<GitObjectId, EncodedObject> {
    PackFile::parse(bytes, format)
        .expect("parse pack")
        .entries
        .into_iter()
        .map(|entry| (entry.entry.oid, entry.object))
        .collect()
}

fn write_reuse_pack(
    repo: &SleyRepository,
    roots: &[GitObjectId],
    excluded: &[GitObjectId],
) -> Option<(Vec<u8>, sley_odb::ReachablePackReuseWrite)> {
    let mut bytes = Vec::new();
    let written =
        write_git_lane_reachable_pack(repo, roots, excluded, &mut bytes).expect("reuse write")?;
    Some((bytes, written))
}

#[test]
fn reuse_pack_contains_the_same_objects_as_the_old_rebuild() {
    let fixture = packed_history();
    let old = rebuild_without_reuse(&fixture.repo, &[fixture.second], &[]).expect("old pack");
    let (reused, stats) =
        write_reuse_pack(&fixture.repo, &[fixture.second], &[]).expect("reuse pack");

    assert!(
        stats.reuse.whole_pack || stats.reuse.verbatim_entries > 0,
        "packed fixture must engage reuse, got {:?}",
        stats.reuse
    );

    let format = fixture.repo.object_format();
    let old_objects = parse_objects(&old, format);
    let reused_objects = parse_objects(&reused, format);
    assert_eq!(
        reused_objects.keys().copied().collect::<HashSet<_>>(),
        old_objects.keys().copied().collect::<HashSet<_>>(),
        "reuse must ship the same object ids as the old rebuild"
    );
    for (oid, object) in &old_objects {
        assert_eq!(
            reused_objects.get(oid),
            Some(object),
            "decoded object {oid} must be identical after reuse"
        );
    }

    let planned = build_git_lane_multi_root_pack_plan(
        &fixture.repo,
        vec![fixture.second],
        Vec::new(),
        64 * 1024,
    )
    .expect("plan git-lane pack")
    .expect("non-empty git-lane pack");
    assert_eq!(planned.pack_size, reused.len() as u64);
    assert_eq!(planned.pack_id, stats.summary.checksum.as_bytes());
}

#[test]
fn have_boundary_excludes_ancestor_objects_from_reused_pack() {
    let fixture = packed_history();
    let old = rebuild_without_reuse(&fixture.repo, &[fixture.second], &[fixture.first])
        .expect("old want-only pack");
    let (reused, stats) = write_reuse_pack(&fixture.repo, &[fixture.second], &[fixture.first])
        .expect("reuse want-only pack");

    assert!(
        !stats.reuse.whole_pack,
        "a have-boundary must not copy the entire source pack"
    );

    let format = fixture.repo.object_format();
    let old_objects = parse_objects(&old, format);
    let reused_objects = parse_objects(&reused, format);
    assert_eq!(
        reused_objects.keys().copied().collect::<HashSet<_>>(),
        old_objects.keys().copied().collect::<HashSet<_>>(),
        "want-only reuse must match the old object set"
    );
    assert!(
        reused_objects.contains_key(&fixture.second),
        "new tip must be packed"
    );
    assert!(
        reused_objects.contains_key(&fixture.second_blob),
        "new blob must be packed"
    );
    assert!(
        !reused_objects.contains_key(&fixture.first),
        "excluded ancestor commit must not appear"
    );
    assert!(
        !reused_objects.contains_key(&fixture.first_blob),
        "objects only reachable from the have-boundary must not appear"
    );

    PackFile::parse(&reused, format).expect("have-boundary pack must be self-contained");
}

#[test]
fn excluded_delta_base_is_not_emitted_as_a_thin_dependency() {
    let fixture = packed_history();
    let (reused, _) =
        write_reuse_pack(&fixture.repo, &[fixture.second_blob], &[fixture.first_blob])
            .expect("pack only the child blob");

    let objects = parse_objects(&reused, fixture.repo.object_format());
    assert_eq!(objects.len(), 1);
    assert!(objects.contains_key(&fixture.second_blob));
    assert!(
        !objects.contains_key(&fixture.first_blob),
        "excluded delta base must not leak into the transfer pack"
    );
    PackFile::parse(&reused, fixture.repo.object_format())
        .expect("pack with an excluded delta base must not be thin");
}

#[test]
fn fully_covered_have_boundary_returns_no_pack() {
    let fixture = packed_history();
    assert!(
        write_reuse_pack(&fixture.repo, &[fixture.first], &[fixture.first]).is_none(),
        "excluding the only root must short-circuit to no pack"
    );
    assert!(
        build_git_lane_multi_root_pack_plan(
            &fixture.repo,
            vec![fixture.first],
            vec![fixture.first],
            64 * 1024,
        )
        .expect("plan empty want-only pack")
        .is_none()
    );
}

#[test]
fn reuse_beats_full_repack_on_an_already_packed_closure() {
    let temp = TempDir::new().expect("temp git repo");
    let repo = SleyRepository::init(temp.path()).expect("init git repo");
    let mut tree = repo
        .edit_tree(&GitObjectId::empty_tree(repo.object_format()))
        .expect("edit tree");
    for index in 0..48 {
        let mut body = vec![b'x'; 32_768];
        body[index * 16] = index as u8;
        let blob = repo.write_blob(body).expect("write blob");
        tree.upsert(
            format!("blob-{index:02}.bin").as_str(),
            EntryKind::Blob,
            blob,
        );
    }
    let tree_oid = repo.write_tree(tree).expect("write tree");
    let commit = CommitObject {
        tree: tree_oid,
        parents: Vec::new(),
        author: TEST_IDENT.to_vec(),
        committer: TEST_IDENT.to_vec(),
        encoding: None,
        message: b"packed corpus\n".to_vec(),
    };
    let root = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("write commit");
    install_reachable_pack(&repo, &[root]);

    let rebuild_started = Instant::now();
    let old = rebuild_without_reuse(&repo, &[root], &[]).expect("old rebuild");
    let rebuild_elapsed = rebuild_started.elapsed();

    let reuse_started = Instant::now();
    let (reused, stats) = write_reuse_pack(&repo, &[root], &[]).expect("reuse write");
    let reuse_elapsed = reuse_started.elapsed();

    eprintln!(
        "git-lane pack reuse: old={}B/{rebuild_elapsed:?} reuse={}B/{reuse_elapsed:?} stats={:?}",
        old.len(),
        reused.len(),
        stats.reuse
    );
    assert!(
        stats.reuse.whole_pack || stats.reuse.verbatim_entries > 0,
        "already-packed closure must reuse local pack entries"
    );
    assert!(
        reused.len() <= old.len(),
        "reuse must not grow past the old rebuild ({} > {})",
        reused.len(),
        old.len()
    );
    assert!(
        reuse_elapsed <= rebuild_elapsed || stats.reuse.whole_pack,
        "reuse ({reuse_elapsed:?}) should not be slower than rebuild ({rebuild_elapsed:?}) unless the sample is too small"
    );
}
