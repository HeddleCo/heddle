// SPDX-License-Identifier: Apache-2.0

use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    Result, invalid,
    io::{Reader, Writer, varint_len},
};
use crate::object::{ContentHash, EntryType, FileMode, SpoolId, StateId, Tree, TreeEntry};

const TREE_MAGIC: &[u8; 4] = b"HCT1";

/// Whether `bytes` begin with the compact-tree frame discriminator.
pub fn is_tree_frame(bytes: &[u8]) -> bool {
    bytes.starts_with(TREE_MAGIC)
}

/// Exact raw-frame size for `tree`, including its per-tree entry count.
pub fn encoded_tree_size(tree: &Tree) -> usize {
    varint_len(tree.len())
        + tree.len() * 2
        + tree
            .entries()
            .iter()
            .map(|entry| varint_len(entry.name().len()) + entry.name().len() + target_len(entry))
            .sum::<usize>()
}

/// Encode name-sorted trees as columnar mode/kind/name/target payloads.
pub fn encode_tree_frame(trees: &[Tree]) -> Result<Vec<u8>> {
    let mut output = Writer::new(TREE_MAGIC);
    output.put_u64(trees.len() as u64);
    for tree in trees {
        tree.validate()?;
        output.put_u64(tree.len() as u64);
        for entry in tree.entries() {
            output.put_u8(entry.mode().to_byte());
        }
        for entry in tree.entries() {
            output.put_u8(entry.entry_type().to_byte());
        }
        for entry in tree.entries() {
            output.put_bytes(entry.name().as_bytes());
        }
        for entry in tree.entries() {
            encode_target(&mut output, entry);
        }
    }
    Ok(output.finish())
}

/// Decode and whole-frame-verify every tree in a compact frame.
pub fn decode_tree_frame(bytes: &[u8]) -> Result<Vec<Tree>> {
    let mut input = Reader::verified(bytes, TREE_MAGIC)?;
    let tree_count = input.get_count("tree frame")?;
    let mut trees = Vec::with_capacity(tree_count);
    for _ in 0..tree_count {
        trees.push(decode_tree(&mut input)?);
    }
    input.finish()?;
    Ok(trees)
}

fn decode_tree(input: &mut Reader<'_>) -> Result<Tree> {
    let count = input.get_count("tree entry")?;
    let modes = (0..count)
        .map(|_| decode_mode(input.get_u8()?))
        .collect::<Result<Vec<_>>>()?;
    let kinds = (0..count)
        .map(|_| decode_kind(input.get_u8()?))
        .collect::<Result<Vec<_>>>()?;
    let names = (0..count)
        .map(|_| {
            String::from_utf8(input.get_bytes()?).map_err(|_| invalid("tree name is not UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        entries.push(decode_entry(
            input,
            names[index].clone(),
            kinds[index],
            modes[index],
        )?);
    }
    let tree = Tree::from_entries(entries);
    tree.validate()?;
    Ok(tree)
}

fn encode_target(output: &mut Writer, entry: &TreeEntry) {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            output.put_fixed(entry.require_content_hash().as_bytes());
        }
        EntryType::Gitlink => {
            let target = entry.gitlink_target().expect("gitlink target");
            output.put_u8(git_format_tag(target.format()));
            output.put_fixed(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().expect("spoollink target");
            output.put_bytes(spool.as_str().as_bytes());
            output.put_fixed(state.as_bytes());
        }
    }
}

fn decode_entry(
    input: &mut Reader<'_>,
    name: String,
    kind: EntryType,
    mode: FileMode,
) -> Result<TreeEntry> {
    let entry = match kind {
        EntryType::Blob => TreeEntry::file(
            name,
            ContentHash::from_bytes(input.get_fixed()?),
            mode == FileMode::Executable,
        )?,
        EntryType::Tree => TreeEntry::directory(name, ContentHash::from_bytes(input.get_fixed()?))?,
        EntryType::Symlink => {
            TreeEntry::symlink(name, ContentHash::from_bytes(input.get_fixed()?))?
        }
        EntryType::Gitlink => {
            let format = decode_git_format(input.get_u8()?)?;
            let oid_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            let target = GitObjectId::from_raw(format, input.take(oid_len)?)
                .map_err(|error| super::CompactError::GitObjectId(error.to_string()))?;
            TreeEntry::gitlink(name, target)?
        }
        EntryType::Spoollink => {
            let spool = String::from_utf8(input.get_bytes()?)
                .map_err(|_| invalid("spool id is not UTF-8"))?;
            TreeEntry::spoollink(
                name,
                SpoolId::parse(spool)
                    .map_err(|error| super::CompactError::SpoolId(error.to_string()))?,
                StateId::from_bytes(input.get_fixed()?),
            )?
        }
    };
    if entry.mode() != mode {
        return Err(invalid(format!(
            "tree kind/mode mismatch for {}: {kind:?}/{mode:?}",
            entry.name()
        )));
    }
    Ok(entry)
}

fn target_len(entry: &TreeEntry) -> usize {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => 32,
        EntryType::Gitlink => {
            1 + entry
                .gitlink_target()
                .expect("gitlink target")
                .as_bytes()
                .len()
        }
        EntryType::Spoollink => {
            let (spool, _) = entry.spoollink_target().expect("spoollink target");
            varint_len(spool.as_str().len()) + spool.as_str().len() + 32
        }
    }
}

fn decode_mode(value: u8) -> Result<FileMode> {
    FileMode::from_byte(value).ok_or_else(|| invalid(format!("invalid tree mode {value}")))
}

fn decode_kind(value: u8) -> Result<EntryType> {
    EntryType::from_byte(value).ok_or_else(|| invalid(format!("invalid tree kind {value}")))
}

fn git_format_tag(format: GitObjectFormat) -> u8 {
    match format {
        GitObjectFormat::Sha1 => 1,
        GitObjectFormat::Sha256 => 2,
    }
}

fn decode_git_format(value: u8) -> Result<GitObjectFormat> {
    match value {
        1 => Ok(GitObjectFormat::Sha1),
        2 => Ok(GitObjectFormat::Sha256),
        _ => Err(invalid(format!("invalid git object format {value}"))),
    }
}
