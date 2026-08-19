// SPDX-License-Identifier: Apache-2.0

use objects::object::{AnnotatedTagMarker, Attribution, MarkerName, Principal};
use repo::Repository as HeddleRepository;
use sley::{
    CommitObject, EntryKind, GitObjectType, GitTime, ObjectFormat, ObjectId, RefPrecondition,
    Repository as SleyRepository, Signature, TagObject, TreeEditor,
    plumbing::{sley_core::ByteString, sley_object::EncodedObject},
};

use super::*;
use crate::GitProjection;

const COMMIT_OID: &str = "1111111111111111111111111111111111111111";
const OTHER_OID: &str = "2222222222222222222222222222222222222222";

fn tag_body(object: &str, kind: &str, name: &str) -> Vec<u8> {
    format!(
        "object {object}\ntype {kind}\ntag {name}\ntagger Test <test@example.com> 1700000000 +0000\n\nrelease\n"
    )
    .into_bytes()
}

fn write_commit(repo: &SleyRepository, message: &str) -> ObjectId {
    let blob = repo.write_blob(b"hello\n").expect("write blob");
    let mut tree = TreeEditor::new();
    tree.upsert("hello.txt", EntryKind::Blob, blob);
    let tree_oid = repo.write_tree(tree).expect("write tree");
    let signature = Signature {
        name: ByteString::new(b"Test".to_vec()),
        email: ByteString::new(b"test@example.com".to_vec()),
        time: GitTime::new(1_700_000_000, 0),
        raw: b"Test <test@example.com> 1700000000 +0000".to_vec(),
    };
    let commit = CommitObject {
        tree: tree_oid,
        parents: Vec::new(),
        author: signature.to_ident_bytes(),
        committer: signature.to_ident_bytes(),
        encoding: None,
        message: format!("{message}\n").into_bytes(),
    };
    repo.write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("write commit")
}

fn write_annotated_tag(repo: &SleyRepository, name: &str, target: ObjectId) -> ObjectId {
    let tag = TagObject {
        object: target,
        object_type: GitObjectType::Commit,
        name: name.as_bytes().to_vec(),
        tagger: Some(
            Signature {
                name: ByteString::new(b"Test".to_vec()),
                email: ByteString::new(b"test@example.com".to_vec()),
                time: GitTime::new(1_700_000_001, 0),
                raw: b"Test <test@example.com> 1700000001 +0000".to_vec(),
            }
            .to_ident_bytes(),
        ),
        message: b"annotated release\n".to_vec(),
        raw_body: None,
    };
    repo.write_object(EncodedObject::new(GitObjectType::Tag, tag.write()))
        .expect("write annotated tag")
}

#[test]
fn peel_native_annotated_tag_rejects_git_target_disagreeing_with_target_tag() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = HeddleRepository::init_default(temp.path()).unwrap();
    let inner = AnnotatedTag::new(
        ObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "inner"),
        None,
        None,
    )
    .expect("inner tag");
    let inner_hash = repo.store().put_annotated_tag(&inner).unwrap();
    let outer = AnnotatedTag::new(
        ObjectFormat::Sha1,
        tag_body(OTHER_OID, "tag", "outer"),
        Some(inner_hash),
        Some(AnnotatedTagMarker {
            name: "outer".to_string(),
            peeled_state: StateId::from_bytes([9; 32]),
        }),
    )
    .expect("outer tag shape");
    let error =
        peel_native_annotated_tag(&repo, &outer).expect_err("Git target must match inner git_oid");
    assert!(
        error.to_string().contains("disagrees"),
        "unexpected error: {error}"
    );
}

#[test]
fn native_annotated_tag_oid_rejects_peel_disagreeing_with_peeled_state() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = HeddleRepository::init_default(temp.path()).unwrap();
    let state = repo.snapshot(Some("tagged".to_string()), None).unwrap();
    repo.create_marker_recorded(&MarkerName::new("v1.0"), &state.state_id)
        .unwrap();
    let tag = AnnotatedTag::new(
        ObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "v1.0"),
        None,
        Some(AnnotatedTagMarker {
            name: "v1.0".to_string(),
            peeled_state: state.state_id,
        }),
    )
    .expect("lying tag shape is valid until peel is checked");
    repo.store().put_annotated_tag(&tag).unwrap();
    let mapped = ObjectId::from_hex(ObjectFormat::Sha1, OTHER_OID).expect("mapped oid");
    let error = native_annotated_tag_oid(&repo, &MarkerName::new("v1.0"), state.state_id, mapped)
        .expect_err("peeled_state mapping must match Git target");
    assert!(
        error.to_string().contains("disagrees"),
        "unexpected error: {error}"
    );
}

#[test]
fn native_annotated_tag_oid_returns_tag_oid_after_peel_matches_mapped_commit() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = HeddleRepository::init_default(temp.path()).unwrap();
    let state = repo.snapshot(Some("tagged".to_string()), None).unwrap();
    repo.create_marker_recorded(&MarkerName::new("v1.0"), &state.state_id)
        .unwrap();
    let mapped = ObjectId::from_hex(ObjectFormat::Sha1, COMMIT_OID).expect("mapped oid");
    let tag = AnnotatedTag::new(
        ObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "v1.0"),
        None,
        Some(AnnotatedTagMarker {
            name: "v1.0".to_string(),
            peeled_state: state.state_id,
        }),
    )
    .expect("bound tag");
    repo.store().put_annotated_tag(&tag).unwrap();
    let chosen = native_annotated_tag_oid(&repo, &MarkerName::new("v1.0"), state.state_id, mapped)
        .expect("peel agrees")
        .expect("annotated tag present");
    assert_eq!(chosen, tag.git_oid().expect("tag git oid"));
    assert_ne!(
        chosen, mapped,
        "refs/tags must name the tag object, not the commit"
    );
}

#[test]
fn export_peels_annotated_tag_before_writing_refs_tags() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source_path = tmp.path().join("source.git");
    let source = SleyRepository::init_bare(&source_path).unwrap();
    let commit = write_commit(&source, "release base");
    set_reference(
        &source,
        "refs/heads/main",
        commit,
        RefPrecondition::MustNotExist,
        "test: main",
    )
    .unwrap();
    let tag_oid = write_annotated_tag(&source, "v1.0", commit);
    set_reference(
        &source,
        "refs/tags/v1.0",
        tag_oid,
        RefPrecondition::MustNotExist,
        "test: annotated tag",
    )
    .unwrap();
    std::fs::write(source_path.join("HEAD"), b"ref: refs/heads/main\n").unwrap();

    let heddle_path = tmp.path().join("heddle");
    ingest::import_git_into(&source_path, &heddle_path).expect("import annotated tag");
    let repo = HeddleRepository::open(&heddle_path).unwrap();
    let mut bridge = GitProjection::new(&repo);
    let dest_path = tmp.path().join("export.git");
    bridge.export_to_path(&dest_path).expect("export");

    let exported = SleyRepository::open(&dest_path).expect("open export");
    let written = direct_ref_oid(&exported, "refs/tags/v1.0").expect("exported tag ref");
    assert_eq!(
        written, tag_oid,
        "export must write the annotated tag object"
    );
    let peeled = peel_to_commit_oid(&exported, written).expect("peel exported tag");
    assert_eq!(
        peeled, commit,
        "export must peel before publishing refs/tags"
    );
}

#[test]
fn export_rejects_annotated_tag_whose_git_target_disagrees_with_peeled_state() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = HeddleRepository::init_default(temp.path()).unwrap();
    let state = repo
        .snapshot_with_attribution(
            Some("tagged".to_string()),
            None,
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
        .unwrap();
    repo.create_marker_recorded(&MarkerName::new("v1.0"), &state.state_id)
        .unwrap();
    let tag = AnnotatedTag::new(
        ObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "v1.0"),
        None,
        Some(AnnotatedTagMarker {
            name: "v1.0".to_string(),
            peeled_state: state.state_id,
        }),
    )
    .expect("lying tag");
    repo.store().put_annotated_tag(&tag).unwrap();

    let mut bridge = GitProjection::new(&repo);
    let dest = temp.path().join("export.git");
    let error = bridge
        .export_to_path(&dest)
        .expect_err("export must fail closed when peel disagrees");
    assert!(
        error.to_string().contains("disagrees"),
        "unexpected error: {error}"
    );
}
