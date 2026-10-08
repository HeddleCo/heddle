// SPDX-License-Identifier: Apache-2.0
use super::*;

fn reason(name: &str) -> Option<ReservedMetadataName> {
    reserved_metadata_name(name.as_bytes())
}

fn expect(name: &str, target: MetadataName, alias: MetadataAlias) {
    assert_eq!(
        reason(name),
        Some(ReservedMetadataName {
            name: target,
            alias
        }),
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
        expect(name, MetadataName::Git, alias);
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
        ("heddle~2", NtfsShortName),
        ("Heddle~3.", NtfsShortName),
        ("HEDDLE~4", NtfsShortName),
        ("heddle~1:x", NtfsShortName),
        (".hed\u{200c}dle", HfsIgnorable),
    ] {
        expect(name, MetadataName::Heddle, alias);
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
        "HEDDLE~5",
        "HEDDLE~10",
        "HEDDL~1",
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
            name: MetadataName::Git,
            alias: MetadataAlias::HfsIgnorable,
        })
    );
    assert_eq!(reserved_metadata_name(b".g\xffit"), None);
}

#[test]
fn heddle_is_reserved_only_at_the_root() {
    assert!(reserved_tree_entry_name_at(b".heddle", true).is_some());
    assert!(reserved_tree_entry_name_at(b".HEDDLE", true).is_some());
    assert_eq!(reserved_tree_entry_name_at(b".heddle", false), None);
    assert_eq!(reserved_tree_entry_name_at(b"HEDDLE~1", false), None);
    assert!(reserved_tree_entry_name_at(b".git", false).is_some());
    assert!(reserved_tree_entry_name_at(b"GIT~1", false).is_some());
    assert!(is_reserved_metadata_name(".heddle"));
}

#[test]
fn path_components_apply_the_depth_rule() {
    let found = |path: &str| reserved_path_component(path.as_bytes(), false);

    let nested = found("a/.git/hooks/x").expect("nested .git");
    assert_eq!(nested.index, 1);
    assert_eq!(nested.component, ".git");
    assert_eq!(nested.reason.name, MetadataName::Git);

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
    let message = reserved_path_component(b"a/GIT~1/x", false)
        .unwrap()
        .to_string();
    assert_eq!(
        message,
        "'GIT~1' is an NTFS 8.3 short name of the .git metadata directory"
    );
}

/// Decoding stays permissive: repositories captured before heddle#2028 can
/// hold a nested `.git` (a vendored clone, a submodule's gitfile), and they
/// must stay readable. Import refuses these names and checkout never writes
/// them; a stored tree that carries one still constructs, encodes and
/// decodes.
#[test]
fn stored_trees_with_a_git_alias_still_decode() {
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
        let tree = Tree::from_entries(vec![
            TreeEntry::file(alias, ContentHash::compute(b"gitdir: ../x"), false).unwrap(),
        ]);
        let msgpack = rmp_serde::to_vec_named(&tree).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<Tree>(&msgpack).unwrap(),
            tree,
            "{alias:?}"
        );
        let canonical = tree.encode_canonical().unwrap();
        assert_eq!(
            Tree::decode_canonical(&canonical).unwrap(),
            tree,
            "{alias:?}"
        );
    }
}

#[test]
fn gitmodules_is_reserved_only_as_a_symlink() {
    use MetadataAlias::*;
    for (name, alias) in [
        (".gitmodules", Exact),
        (".GITMODULES", Case),
        (".gitmodules.", TrailingDotsOrSpaces),
        (".gitmodules::$DATA", NtfsStream),
        ("GITMOD~1", NtfsShortName),
        ("gitmod~4", NtfsShortName),
        ("GI7EBA~1", NtfsShortName),
        ("gi7eba~9", NtfsShortName),
        ("gi7eb~12", NtfsShortName),
        (".git\u{200c}modules", HfsIgnorable),
    ] {
        assert_eq!(
            reserved_tree_entry_name(name.as_bytes(), false, true),
            Some(ReservedMetadataName {
                name: MetadataName::GitModules,
                alias
            }),
            "{name:?}"
        );
        assert_eq!(
            reserved_tree_entry_name(name.as_bytes(), true, false),
            None,
            "{name:?} as a file"
        );
    }
    for name in [
        "gitmod~5",
        "gi7eba~0",
        "gi7ebc~1",
        ".gitmodule",
        ".gitmodulesx",
    ] {
        assert_eq!(
            reserved_tree_entry_name(name.as_bytes(), false, true),
            None,
            "{name:?}"
        );
    }
    assert!(reserved_path_component(b"sub/.gitmodules", true).is_some());
    assert!(reserved_path_component(b".gitmodules/x", true).is_none());
    assert!(reserved_path_component(b"sub/.gitmodules", false).is_none());
}

#[test]
fn only_the_exact_root_metadata_is_the_repositorys_own() {
    let own = |path: &str| {
        reserved_path_component(path.as_bytes(), false)
            .unwrap()
            .is_own_metadata()
    };
    assert!(own(".git/config"));
    assert!(own(".heddle/config.toml"));
    assert!(!own("vendor/.git/config"));
    assert!(!own(".GIT/config"));
    assert!(!own("GIT~1"));
}

fn reserved_tree_entry_name_at(name: &[u8], at_root: bool) -> Option<ReservedMetadataName> {
    reserved_tree_entry_name(name, at_root, false)
}
