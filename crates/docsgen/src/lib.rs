// SPDX-License-Identifier: Apache-2.0

//! Rendering and freshness checks for Heddle's agent-facing documentation corpus.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const INDEX_PATH: &str = "docs/llms.txt";
pub const FULL_PATH: &str = "docs/llms-full.txt";
pub const REGEN_COMMAND: &str = "cargo run -p heddle-docsgen";

const PROJECT_NAME: &str = "Heddle";
const BLURB: &str = "Agent-native version control. Heddle is built around saved states and isolated threads with signed provenance, and exposes a stable JSON machine contract (`--output json`) so agents can drive it without parsing prose. Git compatibility is an interop/output layer, not the core model.";
const INTRO: &str = "This index covers Heddle's top-level reference docs. Start an agent at `heddle help model` for the mental model and `heddle help --output json` for the machine-readable command catalog; the docs below carry the durable contracts (exit codes, JSON schemas, stability guarantees, auth model).";
const SEPARATOR: &str = "========================================================================";
const REFERENCE_DIRS: &[(&str, &str)] = &[
    ("adr/", "Architecture Decision Records."),
    (
        "agents/",
        "Per-repo agent operating config (issue tracker, domain docs).",
    ),
    ("design/", "Design notes and proposals."),
    (
        "contributor-guide/",
        "Contributor onboarding and workflow guides.",
    ),
    ("perf/", "Performance investigations and profiles."),
    ("benchmarks/", "Benchmark harness notes and results."),
    ("spikes/", "Exploratory spikes and prototypes."),
    ("program/", "Program-level planning docs."),
];

#[derive(Debug, Eq, PartialEq)]
pub struct RenderedCorpus {
    pub index: String,
    pub full: String,
    pub source_count: usize,
}

pub fn repository_root() -> io::Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("docs generator crate is not under the repository root"))
}

pub fn render(repo_root: &Path) -> io::Result<RenderedCorpus> {
    let files = corpus_files(repo_root)?;
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no docs/*.md found",
        ));
    }

    let mut docs = Vec::with_capacity(files.len());
    for path in files {
        let content = fs::read_to_string(&path)?;
        docs.push(SourceDoc {
            name: file_name(&path)?.to_owned(),
            content,
        });
    }

    Ok(RenderedCorpus {
        index: build_index(&docs),
        full: build_full(&docs),
        source_count: docs.len(),
    })
}

pub fn stale_files(repo_root: &Path, rendered: &RenderedCorpus) -> io::Result<Vec<&'static str>> {
    let mut stale = Vec::new();
    for (relative_path, expected) in [
        (INDEX_PATH, rendered.index.as_str()),
        (FULL_PATH, rendered.full.as_str()),
    ] {
        match fs::read_to_string(repo_root.join(relative_path)) {
            Ok(current) if current == expected => {}
            Ok(_) => stale.push(relative_path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => stale.push(relative_path),
            Err(error) => return Err(error),
        }
    }
    Ok(stale)
}

pub fn write(repo_root: &Path, rendered: &RenderedCorpus) -> io::Result<()> {
    fs::write(repo_root.join(INDEX_PATH), &rendered.index)?;
    fs::write(repo_root.join(FULL_PATH), &rendered.full)
}

struct SourceDoc {
    name: String,
    content: String,
}

fn corpus_files(repo_root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(repo_root.join("docs"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn file_name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("documentation path is not valid UTF-8: {}", path.display()),
            )
        })
}

fn build_index(files: &[SourceDoc]) -> String {
    let mut out = vec![
        format!("# {PROJECT_NAME}"),
        String::new(),
        format!("> {BLURB}"),
        String::new(),
        INTRO.to_owned(),
        String::new(),
        "## Docs".to_owned(),
        String::new(),
    ];
    for file in files {
        let (title, description) = title_and_description(&file.name, &file.content);
        let suffix = if description.is_empty() {
            String::new()
        } else {
            format!(": {description}")
        };
        out.push(format!("- [{title}]({}){suffix}", file.name));
    }
    out.extend([
        String::new(),
        "## Reference directories".to_owned(),
        String::new(),
        "Not inlined into `llms-full.txt`; browse in-repo under `docs/`.".to_owned(),
        String::new(),
    ]);
    for (label, description) in REFERENCE_DIRS {
        out.push(format!("- [{label}]({label}): {description}"));
    }
    out.push(String::new());
    out.join("\n")
}

fn build_full(files: &[SourceDoc]) -> String {
    let mut out = vec![
        format!("# {PROJECT_NAME} documentation — full corpus"),
        String::new(),
        format!(
            "Generated by {REGEN_COMMAND} from docs/*.md. Do not edit by hand; edit the source docs and regenerate."
        ),
        String::new(),
    ];
    for file in files {
        out.extend([
            SEPARATOR.to_owned(),
            format!("Source: docs/{}", file.name),
            SEPARATOR.to_owned(),
            String::new(),
            file.content.trim_end_matches('\n').to_owned(),
            String::new(),
        ]);
    }
    out.join("\n")
}

fn title_and_description(file_name: &str, markdown: &str) -> (String, String) {
    let mut title = file_name
        .strip_suffix(".md")
        .unwrap_or(file_name)
        .to_owned();
    let mut description = String::new();
    let mut seen_title = false;

    for raw in markdown.lines() {
        let line = raw.trim();
        if !seen_title {
            if let Some(heading) = line.strip_prefix("# ") {
                title = clean(heading);
                seen_title = true;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let candidate = clean(line.trim_start_matches(['>', ' ']).trim());
        if !candidate.is_empty() {
            description = truncate_description(&candidate);
            break;
        }
    }

    (title, description)
}

fn clean(text: &str) -> String {
    let without_links = strip_markdown_links(text);
    without_links
        .chars()
        .filter(|character| !matches!(character, '*' | '_' | '`'))
        .collect::<String>()
        .trim()
        .to_owned()
}

fn strip_markdown_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(open_offset) = text[cursor..].find('[') {
        let open = cursor + open_offset;
        out.push_str(&text[cursor..open]);
        let label_start = open + 1;
        let Some(close_offset) = text[label_start..].find(']') else {
            out.push_str(&text[open..]);
            return out;
        };
        let close = label_start + close_offset;
        let url_start = close + 2;
        if close == label_start || !text[close..].starts_with("](") {
            out.push('[');
            cursor = label_start;
            continue;
        }
        let Some(url_end_offset) = text[url_start..].find(')') else {
            out.push('[');
            cursor = label_start;
            continue;
        };
        if url_end_offset == 0 {
            out.push('[');
            cursor = label_start;
            continue;
        }
        out.push_str(&text[label_start..close]);
        cursor = url_start + url_end_offset + 1;
    }
    out.push_str(&text[cursor..]);
    out
}

fn truncate_description(description: &str) -> String {
    if description.chars().count() <= 160 {
        return description.to_owned();
    }
    let end = description
        .char_indices()
        .nth(157)
        .map_or(description.len(), |(index, _)| index);
    format!("{}...", description[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_corpus_is_fresh() {
        let repo_root = repository_root().expect("repository root should resolve");
        let rendered = render(&repo_root).expect("documentation corpus should render");
        let stale =
            stale_files(&repo_root, &rendered).expect("committed corpus should be readable");

        assert!(
            stale.is_empty(),
            "stale (run {REGEN_COMMAND}): {}",
            stale.join(", ")
        );
    }
}
