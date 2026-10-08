// SPDX-License-Identifier: Apache-2.0
use super::*;

fn reason(name: &str) -> Option<ReservedMetadataName> {
    reserved_metadata_name(name.as_bytes())
}

fn expect(name: &str, dir: MetadataDir, alias: MetadataAlias) {
    assert_eq!(
        reason(name),
        Some(ReservedMetadataName { dir, alias }),
        "{name:?}"
    );
}

#[test]
fn every_git_alias_is_reserved() {
    use MetadataAlias::*;
    for (name, alias) in [
        (".git", Exact),
        (".GIT", Case),
        (".Git", Case),
        (".git.", TrailingDotsOrSpaces),
        (".git ", TrailingDotsOrSpaces),
        (".git. . ", TrailingDotsOrSpaces),
        (".git::$INDEX_ALLOCATION", NtfsStream),
        (".git:", NtfsStream),
        (".GIT . :stream", NtfsStream),
        (".git\\hooks", BackslashSeparator),
        ("GIT~1", NtfsShortName),
        ("git~1", NtfsShortName),
        ("Git~1.", NtfsShortName),
        ("git~1::$INDEX_ALLOCATION", NtfsShortName),
        (".g\u{200c}it", HfsIgnorable),
        ("\u{feff}.git", HfsIgnorable),
        (".GI\u{206f}T\u{200d}", HfsIgnorable),
        (".\u{202a}g\u{202e}it", HfsIgnorable),
    ] {
        expect(name, MetadataDir::Git, alias);
    }
}

#[test]
fn every_heddle_alias_is_reserved() {
    use MetadataAlias::*;
    for (name, alias) in [
        (".heddle", Exact),
        (".HEDDLE", Case),
        (".Heddle", Case),
        (".heddle.", TrailingDotsOrSpaces),
        (".heddle ", TrailingDotsOrSpaces),
        (".heddle::$INDEX_ALLOCATION", NtfsStream),
        (".heddle\\hooks", BackslashSeparator),
        ("HEDDLE~1", NtfsShortName),
        ("heddle~1:x", NtfsShortName),
        (".hed\u{200c}dle", HfsIgnorable),
    ] {
        expect(name, MetadataDir::Heddle, alias);
    }
}

#[test]
fn ordinary_dotfiles_are_not_reserved() {
    for name in [
        ".github",
        ".gitignore",
        ".gitattributes",
        ".gitmodules",
        ".gitkeep",
        ".git-blame-ignore-revs",
        ".git~1",
        ".git.bak",
        ".gi",
        "git",
        ".heddleignore",
        ".heddle.identity",
        ".heddle.last-turn",
        ".heddles",
        "heddle",
        "GIT~2",
        "git~10",
        "HEDDLE~2",
        "x.git",
        ".g\u{200b}it", // U+200B is not one HFS+ ignores
        ".gıt",         // dotless i does not fold to ASCII
        "..git",
    ] {
        assert_eq!(reason(name), None, "{name:?}");
    }
}

#[test]
fn malformed_utf8_ends_the_name_like_git() {
    // Git's HFS check treats a malformed sequence as the end of the name.
    assert_eq!(
        reserved_metadata_name(b".git\xff"),
        Some(ReservedMetadataName {
            dir: MetadataDir::Git,
            alias: MetadataAlias::HfsIgnorable,
        })
    );
    assert_eq!(reserved_metadata_name(b".g\xffit"), None);
}

#[test]
fn heddle_is_reserved_only_at_the_root() {
    assert!(reserved_tree_entry_name(b".heddle", true).is_some());
    assert!(reserved_tree_entry_name(b".HEDDLE", true).is_some());
    assert_eq!(reserved_tree_entry_name(b".heddle", false), None);
    assert_eq!(reserved_tree_entry_name(b"HEDDLE~1", false), None);
    assert!(reserved_tree_entry_name(b".git", false).is_some());
    assert!(reserved_tree_entry_name(b"GIT~1", false).is_some());
    assert!(is_reserved_metadata_name(".heddle"));
}

#[test]
fn path_components_apply_the_depth_rule() {
    let found = |path: &str| reserved_path_component(path.as_bytes());

    let nested = found("a/.git/hooks/x").expect("nested .git");
    assert_eq!(nested.index, 1);
    assert_eq!(nested.component, ".git");
    assert_eq!(nested.reason.dir, MetadataDir::Git);

    for path in [
        ".git",
        ".git/hooks/pre-commit",
        "src/GIT~1/config",
        "deep/er/.GIT./x",
        ".heddle/config.toml",
        ".HEDDLE/hooks/pre-capture",
        "./.heddle/config.toml",
        "/.heddle/config.toml",
        "a\\.git\\config",
    ] {
        assert!(found(path).is_some(), "{path:?}");
    }
    for path in [
        "examples/calculator/.heddle/config.toml",
        "src/.HEDDLE/x",
        ".github/workflows/ci.yml",
        ".gitignore",
        "docs/.heddleignore",
        "",
    ] {
        assert_eq!(found(path), None, "{path:?}");
    }
}

#[test]
fn reasons_name_the_directory_and_the_alias() {
    let message = reserved_path_component(b"a/GIT~1/x").unwrap().to_string();
    assert_eq!(
        message,
        "'GIT~1' is the NTFS 8.3 short name of the .git metadata directory"
    );
}

/// A peer cannot send a tree holding a `.git` alias: every decoder refuses
/// it. The bytes are made by encoding a same-length placeholder name and
/// swapping the alias in, since no constructor will build such a tree.
#[test]
fn wire_trees_with_a_git_alias_do_not_decode() {
    use crate::object::{ContentHash, Tree, TreeEntry};

    for alias in [
        ".git",
        ".GIT",
        ".git.",
        ".git ",
        "GIT~1",
        ".git::$INDEX_ALLOCATION",
        ".g\u{200c}it",
    ] {
        assert!(
            TreeEntry::file(alias, ContentHash::compute(b"x"), false).is_err(),
            "{alias:?} must not construct"
        );
        let placeholder = "Q".repeat(alias.len());
        let tree = Tree::from_entries(vec![
            TreeEntry::file(placeholder.as_str(), ContentHash::compute(b"hook"), true).unwrap(),
        ]);
        let swap = |mut bytes: Vec<u8>| {
            let at = bytes
                .windows(alias.len())
                .position(|window| window == placeholder.as_bytes())
                .expect("placeholder name in encoding");
            bytes[at..at + alias.len()].copy_from_slice(alias.as_bytes());
            bytes
        };

        let msgpack = swap(rmp_serde::to_vec_named(&tree).unwrap());
        let error = rmp_serde::from_slice::<Tree>(&msgpack).unwrap_err();
        assert!(
            error.to_string().contains("metadata directory"),
            "{alias:?}: {error}"
        );

        let canonical = swap(tree.encode_canonical().unwrap());
        let error = Tree::decode_canonical(&canonical).unwrap_err();
        assert!(
            error.to_string().contains("metadata directory"),
            "{alias:?}: {error}"
        );
    }
}
