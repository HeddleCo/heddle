// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Default, Serialize)]
pub struct GitTypeBytes {
    pub objects: u64,
    pub bytes: u64,
}

#[derive(Serialize)]
pub struct GitBaseline {
    pub repository: String,
    pub revision: String,
    pub commits: GitTypeBytes,
    pub trees: GitTypeBytes,
    pub blobs: GitTypeBytes,
    pub tags: GitTypeBytes,
    pub total_objects: u64,
    pub pack_bytes: u64,
    pub idx_bytes: u64,
}

pub fn measure_git(git_dir: &Path) -> Result<GitBaseline> {
    let output = Command::new("git")
        .env_clear()
        .args([
            "--git-dir",
            &git_dir.to_string_lossy(),
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype) %(objectsize)",
        ])
        .output()
        .context("enumerate Git objects")?;
    if !output.status.success() {
        bail!("git cat-file failed with {}", output.status);
    }
    let mut baseline = GitBaseline {
        repository: git_dir.display().to_string(),
        revision: git_stdout(git_dir, &["rev-parse", "HEAD"])?
            .trim()
            .to_string(),
        commits: GitTypeBytes::default(),
        trees: GitTypeBytes::default(),
        blobs: GitTypeBytes::default(),
        tags: GitTypeBytes::default(),
        total_objects: 0,
        pack_bytes: extension_bytes(&git_dir.join("objects/pack"), "pack")?,
        idx_bytes: extension_bytes(&git_dir.join("objects/pack"), "idx")?,
    };
    for line in String::from_utf8(output.stdout)?.lines() {
        let (kind, size) = line
            .split_once(' ')
            .with_context(|| format!("invalid git cat-file row {line:?}"))?;
        let size = size.parse::<u64>()?;
        let bucket = match kind {
            "commit" => &mut baseline.commits,
            "tree" => &mut baseline.trees,
            "blob" => &mut baseline.blobs,
            "tag" => &mut baseline.tags,
            value => bail!("unexpected Git object type {value}"),
        };
        bucket.objects += 1;
        bucket.bytes += size;
        baseline.total_objects += 1;
    }
    Ok(baseline)
}

fn git_stdout(git_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .env_clear()
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!("git {} failed with {}", args.join(" "), output.status);
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn extension_bytes(directory: &Path, extension: &str) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|value| value == extension)
        {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}
