// SPDX-License-Identifier: Apache-2.0
//! Pure markdown invocation extraction for `heddle doctor docs`.
//!
//! Owns tokenization and sample lifting from markdown text. Clap command
//! resolution, RecoveryAdvice, filesystem walks, and catalog checks stay
//! CLI-owned.

use std::path::Path;

/// One `heddle …` invocation extracted from markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsInvocation {
    /// 1-based line number in the source buffer.
    pub line: usize,
    /// Display form of the invocation (includes the `heddle` prefix).
    pub raw: String,
    /// Tokens after the `heddle` prefix (verb, subverbs, flags, values).
    pub tokens: Vec<String>,
}

/// Pull `heddle <…>` invocations out of either inline backtick code
/// (`` `heddle …` ``) or fenced code blocks. Non-backticked prose is ignored.
pub fn extract_invocations(text: &str) -> Vec<DocsInvocation> {
    let mut result = Vec::new();
    let mut in_fence = false;
    let mut planned_fence = false;
    let mut skip_next_planned_line = false;
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_fence {
                in_fence = false;
                planned_fence = false;
            } else {
                in_fence = true;
                planned_fence = is_planned_docs_marker(trimmed) || skip_next_planned_line;
                skip_next_planned_line = false;
            }
            continue;
        }
        if is_planned_docs_marker(line) {
            skip_next_planned_line = true;
            continue;
        }
        if skip_next_planned_line {
            if trimmed.is_empty() {
                continue;
            }
            skip_next_planned_line = false;
            continue;
        }
        if planned_fence {
            continue;
        }
        if in_fence {
            // Inside a code fence: scan whole line for `heddle …`
            // tokens, stopping at end of line.
            let lower = line.trim_start();
            // Strip a leading shell prompt or comment marker.
            let cleaned = strip_shell_prefix(lower);
            if let Some(rest) = cleaned.strip_prefix("heddle ")
                && let Some(tokens) = tokenize(rest)
            {
                result.push(DocsInvocation {
                    line: line_no,
                    raw: format!("heddle {}", rest.trim_end()),
                    tokens,
                });
            }
        } else {
            // Outside a fence: pull out backticked snippets that begin
            // with `heddle `.
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'`' {
                    let start = i + 1;
                    let mut end = start;
                    while end < bytes.len() && bytes[end] != b'`' {
                        end += 1;
                    }
                    if end <= bytes.len() {
                        let snippet = &line[start..end];
                        let cleaned = strip_shell_prefix(snippet);
                        if let Some(rest) = cleaned.strip_prefix("heddle ")
                            && let Some(tokens) = tokenize(rest)
                        {
                            result.push(DocsInvocation {
                                line: line_no,
                                raw: format!("heddle {}", rest.trim_end()),
                                tokens,
                            });
                        }
                        i = end + 1;
                        continue;
                    }
                }
                i += 1;
            }
        }
    }
    result
}

/// Explicit opt-out for planned or illustrative command surfaces.
///
/// Place `<!-- doctor-docs:planned -->` immediately before a markdown
/// line or fence, or include `doctor-docs:planned` in the fence info
/// string.
pub fn is_planned_docs_marker(line: &str) -> bool {
    line.contains("doctor-docs:planned") || line.contains("doctor-docs: planned")
}

/// Strip a leading shell prompt (`$ `) or comment marker (`# `).
pub fn strip_shell_prefix(s: &str) -> &str {
    let s = s.trim_start();
    s.strip_prefix("$ ")
        .or_else(|| s.strip_prefix("# "))
        .unwrap_or(s)
}

/// Best-effort word-splitter for verbs, subverbs, and `--flag` / `--flag=value`.
/// Anything inside `<…>` is treated as a placeholder and left intact as a token.
pub fn tokenize(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            // Stop on shell control characters — these wreck token
            // boundaries and signal "this is a longer pipeline" we
            // probably can't reason about cleanly. `<…>` and `>…<`
            // are NOT in this set: docs routinely use `<name>` and
            // `<dir>` as placeholders, and those need to remain
            // intact so the per-token placeholder check can skip
            // them.
            '|' | '&' | ';' if !in_single && !in_double => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                break;
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Heuristic: paths, dotted slugs, or quoted strings are values; bare
/// identifiers are likely subcommand names.
pub fn looks_like_value(tok: &str) -> bool {
    tok.contains('.') || tok.contains('/') || tok.starts_with('"')
}

/// Repo-relative display path for issue reporting.
pub fn display_path(repo_root: &Path, file: &Path) -> String {
    file.strip_prefix(repo_root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn tokenize_splits_flags_and_preserves_placeholders() {
        let tokens = tokenize("start <name> --path <dir> --workspace ephemeral").unwrap();
        assert_eq!(
            tokens,
            vec![
                "start",
                "<name>",
                "--path",
                "<dir>",
                "--workspace",
                "ephemeral"
            ]
        );
    }

    #[test]
    fn tokenize_stops_at_shell_control() {
        let tokens = tokenize("status --output json | jq .").unwrap();
        assert_eq!(tokens, vec!["status", "--output", "json"]);
    }

    #[test]
    fn tokenize_respects_quotes() {
        let tokens = tokenize("context set -m \"hello world\"").unwrap();
        assert_eq!(tokens, vec!["context", "set", "-m", "hello world"]);
    }

    #[test]
    fn strip_shell_prefix_variants() {
        assert_eq!(strip_shell_prefix("$ heddle status"), "heddle status");
        assert_eq!(strip_shell_prefix("# heddle status"), "heddle status");
        assert_eq!(strip_shell_prefix("  heddle status"), "heddle status");
    }

    #[test]
    fn planned_marker_detection() {
        assert!(is_planned_docs_marker("<!-- doctor-docs:planned -->"));
        assert!(is_planned_docs_marker("```sh doctor-docs: planned"));
        assert!(!is_planned_docs_marker("```bash"));
    }

    #[test]
    fn extract_inline_and_fenced_invocations() {
        let text = "\
Use `heddle status --output json` here.

```bash
$ heddle start probe --path /tmp
```
";
        let inv = extract_invocations(text);
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0].tokens[0], "status");
        assert_eq!(inv[0].line, 1);
        assert_eq!(inv[1].tokens[0], "start");
        assert!(inv[1].raw.starts_with("heddle start"));
    }

    #[test]
    fn planned_marker_skips_next_inline_line() {
        let text = "\
<!-- doctor-docs:planned -->
`heddle frobnicate --foo`
`heddle status`
";
        let inv = extract_invocations(text);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].tokens[0], "status");
    }

    #[test]
    fn planned_fence_info_skips_block() {
        let text = "\
```sh doctor-docs:planned
heddle frobnicate --foo
```
`heddle status --output json`
";
        let inv = extract_invocations(text);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].tokens[0], "status");
    }

    #[test]
    fn ignores_non_backticked_prose() {
        let inv = extract_invocations("when using heddle status without backticks");
        assert!(inv.is_empty());
    }

    #[test]
    fn looks_like_value_heuristics() {
        assert!(looks_like_value("src/lib.rs"));
        assert!(looks_like_value("pkg.mod"));
        assert!(looks_like_value("\"quoted\""));
        assert!(!looks_like_value("marker"));
    }

    #[test]
    fn display_path_strips_repo_root() {
        let root = PathBuf::from("/repo");
        let file = PathBuf::from("/repo/docs/guide.md");
        assert_eq!(display_path(&root, &file), "docs/guide.md");
        assert_eq!(
            display_path(&root, Path::new("/elsewhere/x.md")),
            "/elsewhere/x.md"
        );
    }
}
