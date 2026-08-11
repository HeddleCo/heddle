// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use objects::object::{ContentHash, EntryType, FileMode, SpoolId, Tree, TreeEntry};
use serde::Serialize;
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use crate::compact_io::{Reader, Writer};

#[derive(Default, Serialize)]
pub struct TreeBreakdown {
    pub framing: u64,
    pub modes: u64,
    pub kinds: u64,
    pub names: u64,
    pub targets: u64,
    pub total: u64,
}

impl TreeBreakdown {
    pub fn add(&mut self, other: &Self) {
        self.framing += other.framing;
        self.modes += other.modes;
        self.kinds += other.kinds;
        self.names += other.names;
        self.targets += other.targets;
        self.total += other.total;
    }
}

pub fn encode_tree(tree: &Tree) -> Result<(Vec<u8>, TreeBreakdown)> {
    tree.validate()?;
    let mut output = Writer::new();
    let mut breakdown = TreeBreakdown::default();
    measured(&mut output, &mut breakdown.framing, |writer| {
        writer.put_u64(tree.len() as u64)
    });
    measured(&mut output, &mut breakdown.modes, |writer| {
        for entry in tree.entries() {
            writer.put_u8(entry.mode().to_byte());
        }
    });
    measured(&mut output, &mut breakdown.kinds, |writer| {
        for entry in tree.entries() {
            writer.put_u8(entry.entry_type().to_byte());
        }
    });
    measured(&mut output, &mut breakdown.names, |writer| {
        for entry in tree.entries() {
            writer.put_bytes(entry.name().as_bytes());
        }
    });
    measured(&mut output, &mut breakdown.targets, |writer| {
        for entry in tree.entries() {
            encode_target(writer, entry);
        }
    });
    let bytes = output.finish();
    breakdown.total = bytes.len() as u64;
    Ok((bytes, breakdown))
}

fn measured(output: &mut Writer, count: &mut u64, write: impl FnOnce(&mut Writer)) {
    let before = output.len();
    write(output);
    *count += (output.len() - before) as u64;
}

fn encode_target(output: &mut Writer, entry: &TreeEntry) {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            output.put_fixed(entry.require_content_hash().as_bytes());
        }
        EntryType::Gitlink => {
            let target = entry.gitlink_target().expect("gitlink target");
            output.put_u8(match target.format() {
                GitObjectFormat::Sha1 => 1,
                GitObjectFormat::Sha256 => 2,
            });
            output.put_fixed(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().expect("spoollink target");
            output.put_bytes(spool.as_str().as_bytes());
            output.put_fixed(state.as_bytes());
        }
    }
}

pub fn decode_tree(bytes: &[u8]) -> Result<Tree> {
    let mut input = Reader::new(bytes);
    let count = usize::try_from(input.get_u64()?)?;
    let modes = (0..count)
        .map(|_| decode_mode(input.get_u8()?))
        .collect::<Result<Vec<_>>>()?;
    let kinds = (0..count)
        .map(|_| decode_kind(input.get_u8()?))
        .collect::<Result<Vec<_>>>()?;
    let names = (0..count)
        .map(|_| String::from_utf8(input.get_bytes()?).context("tree name is not UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let entry = decode_entry(&mut input, names[index].clone(), kinds[index], modes[index])?;
        entries.push(entry);
    }
    input.finish()?;
    let tree = Tree::from_entries(entries);
    tree.validate()?;
    Ok(tree)
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
            let format = match input.get_u8()? {
                1 => GitObjectFormat::Sha1,
                2 => GitObjectFormat::Sha256,
                value => bail!("invalid git object format {value}"),
            };
            let oid_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            TreeEntry::gitlink(name, GitObjectId::from_raw(format, input.take(oid_len)?)?)?
        }
        EntryType::Spoollink => {
            let spool = String::from_utf8(input.get_bytes()?).context("spool id is not UTF-8")?;
            TreeEntry::spoollink(
                name,
                SpoolId::parse(spool)?,
                objects::object::StateId::from_bytes(input.get_fixed()?),
            )?
        }
    };
    if entry.mode() != mode {
        bail!(
            "tree kind/mode mismatch for {}: {:?}/{:?}",
            entry.name(),
            kind,
            mode
        );
    }
    Ok(entry)
}

fn decode_mode(value: u8) -> Result<FileMode> {
    FileMode::from_byte(value).with_context(|| format!("invalid tree mode {value}"))
}

fn decode_kind(value: u8) -> Result<EntryType> {
    EntryType::from_byte(value).with_context(|| format!("invalid tree kind {value}"))
}
