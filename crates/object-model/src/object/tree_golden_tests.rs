// SPDX-License-Identifier: Apache-2.0
//! Canonical tree golden corpus (heddle#2018).
//!
//! Pins the native id AND the exact bytes of every durable tree encoding for a
//! corpus of ordinary trees. These values were captured before the optional
//! Git-layout extension (raw Git mode + source order) existed, so a passing run
//! proves that trees which do not use the extension hash and encode exactly as
//! they did before: same ids, same bytes, same sizes.

use sley_core::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::*;
use crate::compact;

fn hash(label: &str) -> ContentHash {
    ContentHash::compute(label.as_bytes())
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Every entry kind, plus names whose native (byte) order differs from Git's
/// canonical order (`lib` is a tree, so Git sorts it as `lib/`, after
/// `lib.rs` and `lib-extra`).
fn corpus() -> Vec<(&'static str, Tree)> {
    let sha1 = GitObjectId::from_raw(GitObjectFormat::Sha1, &[0x11; 20]).unwrap();
    let sha256 = GitObjectId::from_raw(GitObjectFormat::Sha256, &[0x22; 32]).unwrap();
    let spool = SpoolId::parse("acme/child").unwrap();
    let mixed = vec![
        TreeEntry::file("README.md", hash("readme"), false).unwrap(),
        TreeEntry::file("build.sh", hash("build"), true).unwrap(),
        TreeEntry::directory("lib", hash("lib-tree")).unwrap(),
        TreeEntry::file("lib-extra", hash("lib-extra"), false).unwrap(),
        TreeEntry::file("lib.rs", hash("lib.rs"), false).unwrap(),
        TreeEntry::symlink("link", hash("link-target")).unwrap(),
        TreeEntry::gitlink("vendor-sha1", sha1).unwrap(),
        TreeEntry::gitlink("vendor-sha256", sha256).unwrap(),
        TreeEntry::spoollink("child", spool, StateId::from_bytes([3; 32])).unwrap(),
    ];
    let wide = (0..40)
        .map(|index| {
            TreeEntry::file(
                format!("file-{index:03}.txt"),
                hash(&format!("wide-{index}")),
                index % 7 == 0,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    vec![
        ("empty", Tree::new()),
        (
            "single",
            Tree::from_entries(vec![
                TreeEntry::file("hello.txt", hash("hello"), false).unwrap(),
            ]),
        ),
        ("mixed", Tree::from_entries(mixed)),
        ("wide", Tree::from_entries(wide)),
    ]
}

fn render(name: &str, tree: &Tree) -> String {
    let canonical = tree.encode_canonical().unwrap();
    let lean = tree.encode_lean().unwrap();
    let msgpack = rmp_serde::to_vec_named(tree).unwrap();
    let compact = compact::encode_tree_frame(std::slice::from_ref(tree)).unwrap();
    format!(
        "{name} id={} htr4={}:{} hlr1={}:{} msgpack={}:{} hct1={}:{}",
        tree.hash(),
        canonical.len(),
        digest(&canonical),
        lean.len(),
        digest(&lean),
        msgpack.len(),
        digest(&msgpack),
        compact.len(),
        digest(&compact),
    )
}

fn render_v4() -> String {
    let entries = vec![
        TreeEntry::file("a.txt", hash("a"), false).unwrap(),
        TreeEntry::directory("dir", hash("dir")).unwrap(),
        TreeEntry::file("run", hash("run"), true).unwrap(),
    ];
    let salts = vec![[1; 32], [2; 32], [3; 32]];
    let tree = Tree::from_entries_salted_v4(entries, salts).unwrap();
    let salted = tree.encode_canonical().unwrap();
    let msgpack = rmp_serde::to_vec_named(&tree).unwrap();
    format!(
        "v4 id={} hsr1={}:{} msgpack={}:{}",
        tree.hash(),
        salted.len(),
        digest(&salted),
        msgpack.len(),
        digest(&msgpack),
    )
}

const GOLDEN: &[&str] = &[
    "empty id=32fc0aff346b886a86300ab1c7ac94d076be8c821f9f845104d60cb983ab87c8 htr4=61:95ab4295ef165c10579e99d18904e11b6beb77f6d8ca5a4920014443f937cb8e hlr1=5:0203f2f70fdaf7abc79d6f47407fd950a5021f2eb337df790fe4764522e42a01 msgpack=19:ec6cf31e424406247ce91cfe32caf352c06b6ab7c551ae30dc45fbc6c348db9f hct1=38:5c78753a56f579812d811f6dd1986d288b0c3edb6c01c7044b02aabc4d89eefc",
    "single id=7b853604c52d628b5b82bee9b2ff7c234c3f0bbdfb891d79a16f8dd30e06ac5a htr4=110:bd7657d50da932a6d52ff3de56fe5c746af422cbfb177c46582a5e79189f4e17 hlr1=49:c2227d5eaad77499b7d71bb12d5b51cf3718dcc945a278595c94019954aa8d19 msgpack=132:cc0c626ef9c89f66c4279413ccc486966533471cac4d1f58c7440261af19add7 hct1=82:86a603289faf45cc2fd481c32f1004caadc85f6efbf4121c04eaa4889e9b5dab",
    "mixed id=303289a27f402e62109ab3610e3edfe64002b9ae6558f25f7a9610167280f2d8 htr4=491:e66c385d91d14a43b16a3ecd3ac1e20c17f85d843fa0ffafb90dd88b8a4174ab hlr1=371:ffd7b93f4521ec5d44ca9517eeeaf897f363be8461fc4cf320adf2364fd5fa14 msgpack=981:2d2a8ff38c757ba9eed7246dbf36af285688e73747011815aef0906aad0044cb hct1=422:5f0abf02239c4339ace8edf9d1c05e493836b20ee6f5b1357dde03e81541645a",
    "wide id=e32262941623635fe1776ff6daac7ed7f48914983ce330a448b9e4282687a149 htr4=2141:6befdf9788ffb3d7d699675cab446ca920621a2b05bee3bf8ff82d55e4570800 hlr1=1615:8ce0e8de4b323717d5043d22b613455bf34f30ba7744f255b40456e0c02afeb7 msgpack=4561:cc9ed68c42658b386c979cba4c9160febb36677cceb758fdd5697f3a1913275b hct1=1918:819e16d842c6f3b92f32f69593403c279938b8e2a009a28792e483dfa98b7c46",
    "v4 id=f00614b42559c83856aeca340c963c34e1fa2056ac7e2036a3459326859b7264 hsr1=288:01946532b18f62b65843594a2f2f577eac7819b45f522ccdd0d896a84cc23e34 msgpack=447:a6d73862767a09736b4a90eb80209c6f1eea8bf1f5201905082c39522a73940a",
];

#[test]
fn canonical_tree_corpus_matches_pinned_ids_and_bytes() {
    let mut actual = corpus()
        .iter()
        .map(|(name, tree)| render(name, tree))
        .collect::<Vec<_>>();
    actual.push(render_v4());
    for line in &actual {
        println!("{line}");
    }
    assert_eq!(actual, GOLDEN);
}

/// The remaining canonical forms: an HDC1 delta, an HRT1 redacted
/// projection and (with zstd) a block-compressed HTR4 body. Captured before
/// the Git-layout extension existed, like `GOLDEN`.
fn render_other_forms() -> Vec<String> {
    let corpus = corpus();
    let anchor = &corpus[2].1;
    let mut current = anchor.clone();
    current.insert(TreeEntry::file("README.md", hash("readme-2"), false).unwrap());
    current.insert(TreeEntry::file("zz-new", hash("zz-new"), true).unwrap());
    let ops = crate::object::tree_delta(anchor, &current);
    let delta = crate::object::encode_tree_delta(anchor.hash(), anchor, &current, &ops).unwrap();

    let v4 = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("a.txt", hash("a"), false).unwrap(),
            TreeEntry::directory("dir", hash("dir")).unwrap(),
            TreeEntry::file("run", hash("run"), true).unwrap(),
        ],
        vec![[1; 32], [2; 32], [3; 32]],
    )
    .unwrap();
    let hidden = v4.v4_leaf_hash_for("dir").unwrap();
    let partial = PartialTree::project(&v4, &std::iter::once(hidden).collect()).unwrap();
    let redacted = crate::object::encode_redacted_projection(&partial).unwrap();

    #[allow(unused_mut)]
    let mut lines = vec![
        format!("hdc1 {}:{}", delta.len(), digest(&delta)),
        format!("hrt1 {}:{}", redacted.len(), digest(&redacted)),
    ];
    #[cfg(feature = "zstd")]
    {
        let blocked = corpus[3].1.encode_canonical_blocked(3, 0).unwrap();
        lines.push(format!(
            "htr4-blocked {}:{}",
            blocked.len(),
            digest(&blocked)
        ));
    }
    lines
}

const GOLDEN_OTHER_FORMS: &[&str] = &[
    "hdc1 146:9204f87671da29a6167fdf39b8ebc4a4c8cf574703a225e5e81563e69f1c5de3",
    "hrt1 232:85272f14c630c2973245fdf8fcaa4a3963baf678a263205262a55f28270039a6",
];
#[cfg(feature = "zstd")]
const GOLDEN_BLOCKED: &str =
    "htr4-blocked 1551:53513dcb34c11f6ab20e78aa496bb8370f96af754b29ea4794562f77d35969cc";

#[test]
fn canonical_delta_redacted_and_blocked_forms_match_pinned_bytes() {
    let actual = render_other_forms();
    for line in &actual {
        println!("{line}");
    }
    assert_eq!(&actual[..2], GOLDEN_OTHER_FORMS);
    #[cfg(feature = "zstd")]
    assert_eq!(actual[2], GOLDEN_BLOCKED);
}

/// A tree that records a Git layout (heddle#2018), pinned in its trailer
/// form: `b.txt` before `a.txt` (source order) and a `100664` file. Each
/// frame's mode byte carries the layout flags (`0xc0` = raw mode + position,
/// `0x40` = position only), and the trailer follows the target: `u32` LE mode
/// value, a leading-zero count, then the `u32` LE source position.
#[test]
fn layout_tree_matches_pinned_id_and_bytes() {
    let mode = crate::object::RawGitMode::parse(b"100664").unwrap();
    let tree = Tree::from_git_entries(vec![
        TreeEntry::file("b.txt", hash("b"), false).unwrap(),
        TreeEntry::file("a.txt", hash("a"), false)
            .unwrap()
            .with_raw_git_mode(mode)
            .unwrap(),
    ])
    .unwrap();
    let body = tree.encode_canonical().unwrap();
    let actual = format!("id={} htr4={}", tree.hash(), hex::encode(&body));
    println!("{actual}");
    assert_eq!(actual, LAYOUT_GOLDEN);
}

const LAYOUT_GOLDEN: &str = concat!(
    "id=10918952092182b3a78447253c6c8cc28c7ad32cc18d118a6453643f6f0cb0b5 htr4=485",
    "452340410918952092182b3a78447253c6c8cc28c7ad32cc18d118a6453643f6f0cb0b502000",
    "0000000000067000000000000005d0000000000000032000000c0000500612e74787417762fd",
    "dd969a453925d65717ac3eea21320b66b54342fde15128d6caf21215fb481000000010000002",
    "d00000040000500622e74787410e5cf3d3c8a4f9f3468c8cc58eea84892a22fdadbc1acb2241",
    "0190044c1d55300000000",
);
