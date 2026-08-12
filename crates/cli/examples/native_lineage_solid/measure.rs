// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::lineage::RenameRecord;

pub fn compress_frame(frames_dir: &Path, index: usize, bytes: &[u8]) -> Result<u64> {
    let raw_path = frames_dir.join("current.raw");
    let compressed_path = frames_dir.join(format!("{index:05}.zst"));
    fs::write(&raw_path, bytes)?;
    let status = Command::new("/usr/bin/zstd")
        .env_clear()
        .args(["-19", "--long=27", "-q", "-f"])
        .arg(&raw_path)
        .arg("-o")
        .arg(&compressed_path)
        .status()
        .context("run /usr/bin/zstd -19 --long=27")?;
    if !status.success() {
        bail!("zstd failed for frame {} with {status}", raw_path.display());
    }
    fs::remove_file(&raw_path)?;
    Ok(fs::metadata(compressed_path)?.len())
}

pub fn write_renames(path: &Path, renames: &[RenameRecord]) -> Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(output, "child_state\tparent_state\tkind\tfrom\tto")?;
    for rename in renames {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}",
            rename.child.to_string_full(),
            rename.parent.to_string_full(),
            if rename.exact { "exact" } else { "similarity" },
            rename.from,
            rename.to
        )?;
    }
    Ok(())
}
