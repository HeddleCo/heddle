// SPDX-License-Identifier: Apache-2.0
//! Renderer split formalization (A8).
//!
//! The CLI is already structure-first: every verb builds a
//! `#[derive(Serialize)]` output struct, then routes through
//! `should_output_json` to either `serde_json::to_writer` or a
//! hand-written text renderer. This module codifies that pattern as a
//! trait, plus an `emit` helper, so future verbs can't drift back to
//! `println!` at call sites.
//!
//! Adding a new verb: define `struct FooOutput { ... }` deriving
//! `Serialize`, `impl RenderOutput for FooOutput { fn render_text(...) }`,
//! then call `emit(&cli, repo.config(), &output)` from the handler.

use anyhow::Result;
use serde::Serialize;

use crate::cli::{cli_args::Cli, should_output_json};

/// Treat the harness "unknown" placeholder and empty/whitespace strings
/// as absent so renderers don't surface them as literal text. Mirrors
/// the discipline in `snapshot::clean_attribution_value` — the harness
/// writes "unknown" when it can't identify provider/model from
/// argv/env, and rendering that literally as `anthropic/unknown` is
/// worse than just showing the meaningful side.
pub fn real_or_none(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(value)
    }
}

/// Format an `actor` payload (`provider`, `model`) into a one-line
/// display. Suppresses the literal "unknown" placeholder. Returns
/// `None` when neither side carries a real value — callers should
/// suppress the `Actor:` line entirely in that case.
pub fn actor_display(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    let provider = provider.and_then(real_or_none);
    let model = model.and_then(real_or_none);
    match (provider, model) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(m)) => Some(m.to_string()),
        (None, None) => None,
    }
}

/// Format a truncated one-line preview of an ordered string list for
/// inclusion in a status / advice / blocker message. Used by every
/// verb that would otherwise dump a 50+ item csv onto a single line:
/// branch lists in `status`/`log`/`show`/`diagnose`, heavy-impact path
/// lists in `status`/`snapshot`/`thread`/`merge`, and the
/// `Heavy-impact change:` blocker built in `repo::thread_advice`.
///
/// Keeps the first three names and tags the rest as `… +N more`. The
/// full list still lives in every JSON form (`--output json` plus the
/// verb-specific structured surfaces).
pub fn preview_list(items: &[String], total: usize) -> String {
    const PREVIEW: usize = 3;
    let visible: Vec<&str> = items.iter().take(PREVIEW).map(String::as_str).collect();
    let suffix = if total > visible.len() {
        format!(", … +{} more", total - visible.len())
    } else {
        String::new()
    };
    format!("{}{suffix}", visible.join(", "))
}

/// POSIX-shell-quote a path for inclusion in a copy-pasteable command.
///
/// Returns the bare path when it's a safe identifier; otherwise wraps it
/// in single quotes (escaping any embedded single quote via the standard
/// `'\''` trick). Keeps the common case (`cd /tmp/scratch`) clean while
/// staying correct for spaces, parens, `$`, etc.
pub fn shell_quote(path: &str) -> String {
    let safe = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'));
    if safe {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// Optional knobs the text renderer respects. New options append at the
/// tail; defaults stay backwards-compatible.
#[derive(Clone, Debug, Default)]
pub struct RenderOpts {
    /// Caller hint to render a compact one-line view (e.g. `log --oneline`).
    pub short: bool,
    /// Suppress ANSI colour. Resolved by `cli::style` from the global
    /// CLI flag and env, but text renderers may want to consult it
    /// directly when emitting low-level escapes.
    pub no_color: bool,
    /// Optional row cap. `None` means "render everything".
    pub limit: Option<usize>,
}

/// Contract every CLI output type implements. The `Serialize` super-trait
/// is what powers `--json`; `render_text` is the human view. The same
/// underlying value powers both — there is no separate "text-mode" code
/// path that could drift from JSON.
pub trait RenderOutput: Serialize {
    fn render_text<W: std::io::Write>(&self, w: &mut W, opts: RenderOpts) -> std::io::Result<()>;
}

/// Resolve the format decision (JSON vs text) and emit accordingly.
///
/// Centralises the `should_output_json → branch → write` idiom from the
/// existing structure-first verbs. Handlers should construct a typed
/// output value and call this; never `println!` directly.
pub fn emit<T: RenderOutput>(cli: &Cli, cfg: Option<&repo::RepoConfig>, out: &T) -> Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if should_output_json(cli, cfg) {
        serde_json::to_writer(&mut handle, out)?;
        // Trailing newline so terminal renderers don't visually run into
        // the next prompt. JSON consumers strip whitespace anyway.
        use std::io::Write;
        let _ = handle.write_all(b"\n");
    } else {
        out.render_text(&mut handle, RenderOpts::default())?;
    }
    Ok(())
}

/// Same as [`emit`] but lets the caller pass non-default render options
/// (e.g. `RenderOpts { short: true, .. }` for `log --oneline`).
pub fn emit_with_opts<T: RenderOutput>(
    cli: &Cli,
    cfg: Option<&repo::RepoConfig>,
    out: &T,
    opts: RenderOpts,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if should_output_json(cli, cfg) {
        serde_json::to_writer(&mut handle, out)?;
        use std::io::Write;
        let _ = handle.write_all(b"\n");
    } else {
        out.render_text(&mut handle, opts)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn safe_paths_are_returned_unquoted() {
        assert_eq!(shell_quote("/tmp/scratch"), "/tmp/scratch");
        assert_eq!(
            shell_quote("/home/user/.heddle-threads/my-thread/root"),
            "/home/user/.heddle-threads/my-thread/root"
        );
        assert_eq!(
            shell_quote("relative/path-1.2_3+x"),
            "relative/path-1.2_3+x"
        );
    }

    #[test]
    fn paths_with_spaces_are_single_quoted() {
        assert_eq!(shell_quote("/tmp/scratch dir"), "'/tmp/scratch dir'");
        assert_eq!(
            shell_quote("/Users/luke/My Repo/.thread"),
            "'/Users/luke/My Repo/.thread'"
        );
    }

    #[test]
    fn metacharacters_are_single_quoted() {
        assert_eq!(shell_quote("/tmp/$HOME"), "'/tmp/$HOME'");
        assert_eq!(shell_quote("/tmp/(paren)"), "'/tmp/(paren)'");
        assert_eq!(shell_quote("/tmp/a;b"), "'/tmp/a;b'");
        assert_eq!(shell_quote("/tmp/a&b"), "'/tmp/a&b'");
        assert_eq!(shell_quote("/tmp/a*b"), "'/tmp/a*b'");
    }

    #[test]
    fn embedded_single_quote_is_escaped() {
        assert_eq!(shell_quote("/tmp/o'brien"), "'/tmp/o'\\''brien'");
    }

    #[test]
    fn empty_path_is_quoted() {
        assert_eq!(shell_quote(""), "''");
    }
}
