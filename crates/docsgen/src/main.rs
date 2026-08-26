// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::fmt;
use std::io;

use heddle_docsgen::{FULL_PATH, INDEX_PATH, REGEN_COMMAND, render, repository_root, stale_files};

fn main() -> Result<(), Error> {
    let Some(check) = parse_args()? else {
        return Ok(());
    };
    let repo_root = repository_root()?;
    let rendered = render(&repo_root)?;

    if check {
        let stale = stale_files(&repo_root, &rendered)?;
        if stale.is_empty() {
            println!("llms.txt / llms-full.txt are up to date");
            return Ok(());
        }
        return Err(Error::Stale(stale));
    }

    heddle_docsgen::write(&repo_root, &rendered)?;
    println!(
        "wrote {INDEX_PATH} ({} bytes) and {FULL_PATH} ({} bytes) from {} docs",
        rendered.index.len(),
        rendered.full.len(),
        rendered.source_count
    );
    Ok(())
}

fn parse_args() -> Result<Option<bool>, Error> {
    let mut check = false;
    for argument in env::args().skip(1) {
        match argument.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!(
                    "Generate docs/llms.txt and docs/llms-full.txt from docs/*.md.\n\nUsage: heddle-docsgen [--check]\n\nOptions:\n    --check  Exit non-zero if the committed files are stale\n    -h, --help  Print help"
                );
                return Ok(None);
            }
            _ => return Err(Error::Usage(argument)),
        }
    }
    Ok(Some(check))
}

enum Error {
    Io(io::Error),
    Stale(Vec<&'static str>),
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Stale(files) => write!(
                formatter,
                "stale (run {REGEN_COMMAND}): {}",
                files.join(", ")
            ),
            Self::Usage(argument) => write!(formatter, "unknown argument: {argument}"),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
