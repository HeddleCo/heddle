use crate::object::*;
use crate::object::{EntryType, TreeEntry};
use crate::store::{InMemoryStore, ObjectStore};
use std::path::Path;

fn create_blob(store: &InMemoryStore, content: &[u8]) -> ContentHash {
    ObjectStore::put_blob(store, &Blob::from_slice(content)).unwrap()
}

fn create_tree(store: &InMemoryStore, entries: Vec<(&str, ContentHash, EntryType)>) -> ContentHash {
    let entries = entries
        .into_iter()
        .map(|(name, hash, entry_type)| match entry_type {
            EntryType::Blob => TreeEntry::file(name.to_string(), hash, false),
            EntryType::Tree => TreeEntry::directory(name.to_string(), hash),
            EntryType::Symlink => TreeEntry::symlink(name.to_string(), hash),
            EntryType::Gitlink => unreachable!("tree path tests do not build gitlinks"),
            EntryType::Spoollink => {
                unreachable!("tree path tests do not build spoollinks")
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    ObjectStore::put_tree(store, &Tree::from_entries(entries)).unwrap()
}

struct Fixture {
    store: InMemoryStore,
    root: ContentHash,
    blob_hash: ContentHash,
    symlink_hash: ContentHash,
    nested_blob_hash: ContentHash,
    missing_subtree_hash: ContentHash,
}

fn fixture() -> Fixture {
    let store = InMemoryStore::new();
    let blob_hash = create_blob(&store, b"blob content");
    let symlink_hash = create_blob(&store, b"target.txt");
    let nested_blob_hash = create_blob(&store, b"nested content");

    let nested_tree = create_tree(
        &store,
        vec![("inner.txt", nested_blob_hash, EntryType::Blob)],
    );
    let missing_subtree_hash = ContentHash::compute(b"not-in-store");
    let missing_subtree_parent = create_tree(
        &store,
        vec![("ghost", missing_subtree_hash, EntryType::Tree)],
    );
    let root = create_tree(
        &store,
        vec![
            ("file.txt", blob_hash, EntryType::Blob),
            ("link", symlink_hash, EntryType::Symlink),
            ("dir", nested_tree, EntryType::Tree),
            ("missing", missing_subtree_parent, EntryType::Tree),
        ],
    );

    Fixture {
        store,
        root,
        blob_hash,
        symlink_hash,
        nested_blob_hash,
        missing_subtree_hash,
    }
}

#[test]
fn leaf_content_blob_resolves_symlinks_and_nested_paths() {
    let fx = fixture();

    let file = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("file.txt"),
        LeafPolicy::LeafContentBlob,
    )
    .unwrap()
    .unwrap();
    assert_eq!(file.content_hash, Some(fx.blob_hash));
    assert_eq!(file.blob.as_ref().unwrap().content(), b"blob content");

    let link = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("link"),
        LeafPolicy::LeafContentBlob,
    )
    .unwrap()
    .unwrap();
    assert_eq!(link.content_hash, Some(fx.symlink_hash));
    assert_eq!(link.blob.as_ref().unwrap().content(), b"target.txt");

    let nested = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("dir/inner.txt"),
        LeafPolicy::LeafContentBlob,
    )
    .unwrap()
    .unwrap();
    assert_eq!(nested.content_hash, Some(fx.nested_blob_hash));

    assert!(
        resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("dir"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .is_none()
    );
    assert!(
        resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("nope.txt"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .is_none()
    );
    assert!(
        resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("missing/ghost/inner.txt"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn entry_policy_returns_terminal_entry_for_any_leaf_type() {
    let fx = fixture();

    let file = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("file.txt"),
        LeafPolicy::Entry,
    )
    .unwrap()
    .unwrap();
    assert_eq!(file.entry.blob_hash(), Some(fx.blob_hash));

    let link = resolve_tree_path(&fx.store, &fx.root, Path::new("link"), LeafPolicy::Entry)
        .unwrap()
        .unwrap();
    assert!(link.entry.is_symlink());
    assert_eq!(link.entry.leaf_content_hash(), Some(fx.symlink_hash));

    let dir = resolve_tree_path(&fx.store, &fx.root, Path::new("dir"), LeafPolicy::Entry)
        .unwrap()
        .unwrap();
    assert!(dir.entry.is_tree());

    assert!(
        resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("dir/missing"),
            LeafPolicy::Entry
        )
        .unwrap()
        .is_none()
    );
    assert!(
        resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("missing/ghost/inner.txt"),
            LeafPolicy::Entry,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn blob_only_excludes_symlinks_and_errors_on_missing_subtree() {
    let fx = fixture();

    let file = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("file.txt"),
        LeafPolicy::BlobOnly,
    )
    .unwrap()
    .unwrap();
    assert_eq!(file.content_hash, Some(fx.blob_hash));

    assert!(
        resolve_tree_path(&fx.store, &fx.root, Path::new("link"), LeafPolicy::BlobOnly)
            .unwrap()
            .is_none()
    );

    let nested = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("dir/inner.txt"),
        LeafPolicy::BlobOnly,
    )
    .unwrap()
    .unwrap();
    assert_eq!(nested.content_hash, Some(fx.nested_blob_hash));

    assert!(
        resolve_tree_path(&fx.store, &fx.root, Path::new("dir"), LeafPolicy::BlobOnly)
            .unwrap()
            .is_none()
    );

    let err = resolve_tree_path(
        &fx.store,
        &fx.root,
        Path::new("missing/ghost/inner.txt"),
        LeafPolicy::BlobOnly,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        TreePathResolveError::SubtreeMissing(hash) if hash == fx.missing_subtree_hash
    ));
}
