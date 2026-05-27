// SPDX-License-Identifier: Apache-2.0
//! Output formatting for diff command.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    process::{Command, Stdio},
};

use super::diff_types::{
    DiffOutput, FileChange, LineDiff, SemanticChangeEntry, should_render_modified_pair,
};
use crate::cli::style;

const PAGER_LINE_THRESHOLD: usize = 200;
const SIGNATURE_CHANGE_SEPARATOR: &str = "\u{1f}";

pub(crate) fn print_stat(output: &DiffOutput) {
    for change in &output.changes {
        match change.kind.as_str() {
            "added" => {
                // Status glyph is the carrier; colour only the +/M/-
                // prefix and let path text stay neutral so long path
                // lists scan as a column rather than a lightshow.
                println!(" {} {} | added", style::accent("+"), change.path);
            }
            "modified" => {
                println!(" {} {} | modified", style::warn("M"), change.path);
            }
            "deleted" => {
                println!(" {} {} | deleted", style::error("-"), change.path);
            }
            "renamed" => {
                let old_path = change.old_path.as_deref().unwrap_or("?");
                println!(
                    " {} {} -> {} | renamed",
                    style::accent("R"),
                    old_path,
                    change.path
                );
            }
            _ => {}
        }
    }

    if let Some(ref semantic) = output.semantic_changes {
        for change in semantic {
            if change.change_type == "file_renamed" {
                println!(
                    " {} -> {} | renamed",
                    change.from_path.as_deref().unwrap_or("?"),
                    change.to_path.as_deref().unwrap_or("?")
                );
            }
        }
    }

    println!();
    println!(
        " {} files changed, {} additions, {} modifications, {} deletions, {} renames",
        output.stats.files_changed,
        output.stats.additions,
        output.stats.modifications,
        output.stats.deletions,
        output.stats.renames
    );
}

/// Render the diff as standard unified-diff text — no gutter, no
/// inline-edit `~` lines, no ANSI styling. Output is suitable for
/// `patch(1)` and `git apply` round-trip. Each line is `prefix + content`
/// because the hunk-header `LineDiff` already encodes the second `@`
/// (prefix=`@`, content=`@ -a,b +c,d @@`), so concatenation yields the
/// canonical `@@ -a,b +c,d @@` shape.
///
/// Two cases require git's extended header block to round-trip:
///
/// * **Added files** get `diff --git ... / new file mode 100644 /
///   --- /dev/null`. Without it, `git apply` (and `patch -p1`) demand
///   that `b/<path>` already exist on the target side, which defeats
///   the whole point of an add hunk.
/// * **Renames** get `diff --git a/old b/new / similarity index N% /
///   rename from old / rename to new`. Pure renames (no edits) emit
///   the extended headers and stop; rename-with-edit appends the
///   usual `--- a/old / +++ b/new` + hunk body.
pub(crate) fn render_diff_patch(output: &DiffOutput) -> String {
    let mut buf = String::new();
    for change in &output.changes {
        let lines_ref = change.lines.as_deref();
        let has_hunk_body = lines_ref
            .is_some_and(|lines| lines.iter().any(|line| line.prefix != " "));
        let old_path = change.old_path.as_deref().unwrap_or(&change.path);
        let is_rename = change.old_path.as_deref().is_some_and(|old| old != change.path);
        let is_added = change.kind == "added";

        // Renames must emit headers even when there's no hunk body
        // (pure rename = identical content). Other kinds need at least
        // one `+`/`-` line; without one, `--- /+++/@@` would be a
        // header with no body, which `patch(1)` reads as malformed.
        if !is_rename && !has_hunk_body {
            continue;
        }

        if is_rename {
            buf.push_str(&format!("diff --git a/{old_path} b/{}\n", change.path));
            let pct = (change
                .similarity_score
                .unwrap_or(1.0)
                .clamp(0.0, 1.0)
                * 100.0)
                .round() as u32;
            buf.push_str(&format!("similarity index {pct}%\n"));
            buf.push_str(&format!("rename from {old_path}\n"));
            buf.push_str(&format!("rename to {}\n", change.path));
            // Pure rename — extended headers alone suffice; emitting
            // `--- a/old / +++ b/new` without hunks would tell git to
            // apply an empty patch and warn about a stray header.
            if !has_hunk_body {
                continue;
            }
        } else if is_added {
            // `new file mode 100644` is the git-canonical default for a
            // regular text blob. The probe doesn't carry the real mode
            // yet; matching git's default for a new regular file keeps
            // the round-trip clean. If/when executable-bit detection
            // lands, thread it through here.
            buf.push_str(&format!("diff --git a/{} b/{}\n", change.path, change.path));
            buf.push_str("new file mode 100644\n");
        }

        if is_added {
            buf.push_str("--- /dev/null\n");
        } else {
            buf.push_str(&format!("--- a/{old_path}\n"));
        }
        buf.push_str(&format!("+++ b/{}\n", change.path));
        if let Some(lines) = lines_ref {
            render_patch_hunks(change, lines, &mut buf);
        }
    }
    buf
}

const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// Walk the rendered hunks once and emit each line, splicing in the
/// `\ No newline at end of file` marker after the line that holds the
/// file's tail on a side whose source bytes lacked a trailing `\n`.
///
/// The diff backend strips line terminators, so per-line equality
/// collapses `hello` and `hello\n` into the same `LineDiff`. To match
/// `git diff`'s output (which `git apply --check` accepts), a context
/// line that sits on the no-newline side's tail has to be split into
/// a `-` + `+` pair, with the marker attached to the side that lacks
/// the terminator. The 4-case matrix is in `render_patch_hunks`'s
/// context-line branch.
fn render_patch_hunks(change: &FileChange, lines: &[LineDiff], buf: &mut String) {
    let old_no_eol = !change.eol.old_has_final_newline;
    let new_no_eol = !change.eol.new_has_final_newline;
    let old_tail_idx = if old_no_eol && change.eol.old_line_count > 0 {
        find_side_tail_idx(lines, Side::Old, change.eol.old_line_count)
    } else {
        None
    };
    let new_tail_idx = if new_no_eol && change.eol.new_line_count > 0 {
        find_side_tail_idx(lines, Side::New, change.eol.new_line_count)
    } else {
        None
    };

    for (idx, line) in lines.iter().enumerate() {
        let is_old_tail = Some(idx) == old_tail_idx;
        let is_new_tail = Some(idx) == new_tail_idx;
        let needs_old_marker = is_old_tail && old_no_eol;
        let needs_new_marker = is_new_tail && new_no_eol;

        if line.prefix == " " && (needs_old_marker || needs_new_marker) {
            if is_old_tail && is_new_tail && needs_old_marker && needs_new_marker {
                // Both sides' tail lands on this context line and both
                // lack a trailing newline — emit the line once, then
                // a single marker that applies to both sides.
                emit_line(buf, line);
                buf.push_str(NO_NEWLINE_MARKER);
            } else {
                // Mixed state: at least one side needs the marker and
                // the other shouldn't be tagged. Split the context
                // line into a `-content` / `+content` pair so each
                // side's marker (or its absence) is unambiguous.
                buf.push('-');
                buf.push_str(&line.content);
                buf.push('\n');
                if needs_old_marker {
                    buf.push_str(NO_NEWLINE_MARKER);
                }
                buf.push('+');
                buf.push_str(&line.content);
                buf.push('\n');
                if needs_new_marker {
                    buf.push_str(NO_NEWLINE_MARKER);
                }
            }
            continue;
        }

        emit_line(buf, line);
        if needs_old_marker && line.prefix == "-" {
            buf.push_str(NO_NEWLINE_MARKER);
        }
        if needs_new_marker && line.prefix == "+" {
            buf.push_str(NO_NEWLINE_MARKER);
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

fn find_side_tail_idx(lines: &[LineDiff], side: Side, target: usize) -> Option<usize> {
    lines.iter().enumerate().rev().find_map(|(idx, line)| {
        let (on_side, line_number) = match side {
            Side::Old => (line.prefix == "-" || line.prefix == " ", line.old_line),
            Side::New => (line.prefix == "+" || line.prefix == " ", line.new_line),
        };
        if on_side && line_number == Some(target) {
            Some(idx)
        } else {
            None
        }
    })
}

fn emit_line(buf: &mut String, line: &LineDiff) {
    buf.push_str(&line.prefix);
    buf.push_str(&line.content);
    buf.push('\n');
}

pub(crate) fn print_diff_patch(output: &DiffOutput) {
    let rendered = output
        .patch
        .clone()
        .unwrap_or_else(|| render_diff_patch(output));
    print!("{rendered}");
}

pub(crate) fn print_diff(output: &DiffOutput) {
    let mut rendered = String::new();
    for change in &output.changes {
        // File-header rows: `--- a/...` / `+++ b/...` are dim;
        // they're navigation, not data.
        let old_path = change.old_path.as_deref().unwrap_or(&change.path);
        rendered.push_str(&style::dim(&format!("--- a/{old_path}")));
        rendered.push('\n');
        rendered.push_str(&style::dim(&format!("+++ b/{}", change.path)));
        rendered.push('\n');
        if change.kind == "renamed" {
            rendered.push_str(&style::dim(&format!("rename from {old_path}")));
            rendered.push('\n');
            rendered.push_str(&style::dim(&format!("rename to {}", change.path)));
            rendered.push('\n');
        }

        if let Some(lines) = &change.lines {
            let mut index = 0;
            while index < lines.len() {
                let line = &lines[index];
                if line.prefix == "-"
                    && let Some(next) = lines.get(index + 1)
                    && next.prefix == "+"
                {
                    if style::color_enabled()
                        && should_render_modified_pair(&line.content, &next.content)
                    {
                        rendered.push_str(&paint_modified_pair(line, next));
                        rendered.push('\n');
                    } else {
                        rendered.push_str(&paint_line(line));
                        rendered.push('\n');
                        rendered.push_str(&paint_line(next));
                        rendered.push('\n');
                    }
                    index += 2;
                    continue;
                }

                rendered.push_str(&paint_line(line));
                rendered.push('\n');
                index += 1;
            }
        } else {
            let summary = if change.binary {
                format!("Binary file changed: {}", change.path)
            } else {
                format!("File changed; line diff unavailable: {}", change.path)
            };
            rendered.push_str(&style::dim(&summary));
            rendered.push('\n');
        }

        rendered.push('\n');
    }
    write_diff_text(&rendered);
}

fn paint_line(line: &LineDiff) -> String {
    let body = paint_body(&line.prefix, &line.content);
    format!("{}{}", number_gutter(line.old_line, line.new_line), body)
}

fn write_diff_text(rendered: &str) {
    if should_page(rendered)
        && let Ok(mut child) = pager_command().stdin(Stdio::piped()).spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(rendered.as_bytes());
        }
        let _ = child.wait();
        return;
    }

    print!("{rendered}");
}

fn should_page(rendered: &str) -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("HEDDLE_NO_PAGER").is_none()
        && rendered.lines().count() > PAGER_LINE_THRESHOLD
}

fn pager_command() -> Command {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -R -M".to_string());
    let mut parts = pager.split_whitespace();
    let executable = parts.next().unwrap_or("less");
    let mut command = Command::new(executable);
    for arg in parts {
        command.arg(arg);
    }
    if executable == "less" && std::env::var_os("PAGER").is_some() {
        command.arg("-R").arg("-M");
    }
    command
}

fn paint_body(prefix: &str, content: &str) -> String {
    let combined = format!("{prefix}{content}");
    match prefix {
        "+" => style::accent(&combined),
        "-" => style::error(&combined),
        "@" => style::dim(&combined),
        _ => combined,
    }
}

fn number_gutter(old_line: Option<usize>, new_line: Option<usize>) -> String {
    match (old_line, new_line) {
        (None, None) => String::new(),
        _ => style::dim(&format!(
            "{:>4} {:>4} | ",
            old_line
                .map(format_line_number)
                .unwrap_or_else(|| " ".to_string()),
            new_line
                .map(format_line_number)
                .unwrap_or_else(|| " ".to_string()),
        )),
    }
}

fn format_line_number(line: usize) -> String {
    line.to_string()
}

fn paint_modified_pair(removed: &LineDiff, added: &LineDiff) -> String {
    format!(
        "{}{}",
        number_gutter(removed.old_line, added.new_line),
        paint_modified_body(&removed.content, &added.content),
    )
}

fn paint_modified_body(removed: &str, added: &str) -> String {
    let tokens = aligned_added_tokens(removed, added);
    let mut rendered = style::warn("~");
    for token in tokens {
        if token.changed {
            rendered.push_str(&style::accent(token.text));
        } else {
            rendered.push_str(&style::warn(token.text));
        }
    }
    rendered
}

#[derive(Debug, PartialEq, Eq)]
struct PaintedToken<'a> {
    text: &'a str,
    changed: bool,
}

fn aligned_added_tokens<'a>(removed: &str, added: &'a str) -> Vec<PaintedToken<'a>> {
    let old_tokens = tokenize_inline(removed);
    let new_tokens = tokenize_inline(added);

    let mut prefix_len = 0usize;
    while prefix_len < old_tokens.len()
        && prefix_len < new_tokens.len()
        && old_tokens[prefix_len] == new_tokens[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;
    while suffix_len < old_tokens.len().saturating_sub(prefix_len)
        && suffix_len < new_tokens.len().saturating_sub(prefix_len)
        && old_tokens[old_tokens.len() - 1 - suffix_len]
            == new_tokens[new_tokens.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    let old_middle = &old_tokens[prefix_len..old_tokens.len().saturating_sub(suffix_len)];
    let new_middle = &new_tokens[prefix_len..new_tokens.len().saturating_sub(suffix_len)];
    let old_len = old_middle.len();
    let new_len = new_middle.len();
    let mut aligned = vec![false; new_tokens.len()];
    for slot in aligned.iter_mut().take(prefix_len) {
        *slot = true;
    }
    for slot in aligned.iter_mut().rev().take(suffix_len) {
        *slot = true;
    }

    let mut table = vec![vec![0usize; new_len + 1]; old_len + 1];

    for old_index in (0..old_len).rev() {
        for new_index in (0..new_len).rev() {
            table[old_index][new_index] = if old_middle[old_index] == new_middle[new_index] {
                table[old_index + 1][new_index + 1] + 1
            } else {
                table[old_index + 1][new_index].max(table[old_index][new_index + 1])
            };
        }
    }

    let (mut old_index, mut new_index) = (0usize, 0usize);
    while old_index < old_len && new_index < new_len {
        if old_middle[old_index] == new_middle[new_index] {
            aligned[prefix_len + new_index] = true;
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }

    new_tokens
        .into_iter()
        .enumerate()
        .map(|(index, text)| PaintedToken {
            text,
            changed: !aligned[index],
        })
        .collect()
}

fn tokenize_inline(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut previous_kind: Option<TokenKind> = None;

    for (index, ch) in s.char_indices() {
        let kind = TokenKind::for_char(ch);
        if kind == TokenKind::Punctuation {
            if start < index {
                tokens.push(&s[start..index]);
            }
            let end = index + ch.len_utf8();
            tokens.push(&s[index..end]);
            start = end;
            previous_kind = None;
            continue;
        }
        if let Some(previous) = previous_kind
            && previous != kind
        {
            tokens.push(&s[start..index]);
            start = index;
        }
        previous_kind = Some(kind);
    }

    if start < s.len() {
        tokens.push(&s[start..]);
    }
    tokens
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Whitespace,
    Punctuation,
}

impl TokenKind {
    fn for_char(ch: char) -> Self {
        if ch.is_alphanumeric() || ch == '_' {
            Self::Word
        } else if ch.is_whitespace() {
            Self::Whitespace
        } else {
            Self::Punctuation
        }
    }
}

pub(crate) fn print_context(output: &DiffOutput) {
    if let Some(guidance) = &output.broader_guidance
        && !guidance.is_empty()
    {
        println!("Broader Guidance:");
        println!("-----------------");
        for annotation in guidance {
            println!(
                "  [{}] {} ({} rev{})",
                annotation.kind,
                annotation.content,
                annotation.revision_count,
                if annotation.revision_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        println!();
    }

    if let Some(entries) = &output.context {
        let mut printed_header = false;
        for entry in entries {
            if entry.annotations.is_empty() {
                continue;
            }
            if !printed_header {
                println!("Applicable Context:");
                println!("-------------------");
                printed_header = true;
            }
            println!("{}", entry.path);
            for annotation in &entry.annotations {
                println!(
                    "  [{}] {} ({} rev{})",
                    annotation.kind,
                    annotation.content,
                    annotation.revision_count,
                    if annotation.revision_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
            println!();
        }
    }
}

pub(crate) fn print_semantic_changes(changes: &[SemanticChangeEntry]) {
    if changes.is_empty() {
        return;
    }

    println!("{}", style::bold("Semantic Changes:"));
    println!("{}", style::dim("----------------"));

    let grouped = group_semantic_changes(changes);
    for file in grouped.files.values() {
        println!("{}", style::dim(&file.path));
        for (label, items) in &file.groups {
            println!("  {}:", paint_semantic_label(label));
            for item in items {
                for line in paint_semantic_item_lines(label, item) {
                    println!("    {line}");
                }
            }
        }
        println!();
    }

    if !grouped.dependencies.is_empty() {
        println!("{}", style::bold("Dependencies:"));
        for (label, items) in &grouped.dependencies {
            println!("  {}:", paint_semantic_label(label));
            for item in items {
                println!("    {} {}", style::accent("-"), item);
            }
        }
        println!();
    }

    if !grouped.other.is_empty() {
        println!("{}", style::bold("Other:"));
        for item in &grouped.other {
            println!("  {} {item}", style::accent("-"));
        }
        println!();
    }
}

fn paint_semantic_label(label: &str) -> String {
    match label {
        "Function deleted" | "Removed" => style::error(label),
        "Function modified" | "Signature changed" => style::warn(label),
        "Function added" | "Function extracted" | "Function renamed" | "Function moved"
        | "Added" => style::accent(label),
        _ => style::bold(label),
    }
}

fn paint_semantic_item(label: &str, item: &str) -> String {
    match label {
        "Function extracted" => paint_extracted_item(item),
        _ => item.to_string(),
    }
}

fn paint_semantic_item_lines(label: &str, item: &str) -> Vec<String> {
    if label == "Signature changed" {
        return paint_signature_change_item_lines(item);
    }
    vec![format!(
        "{} {}",
        style::accent("-"),
        paint_semantic_item(label, item)
    )]
}

fn paint_extracted_item(item: &str) -> String {
    let Some((name, source)) = item.split_once(" from ") else {
        return style::accent(item);
    };
    format!(
        "{} {} {}",
        style::accent(name),
        style::dim("from"),
        style::warn(source)
    )
}

fn paint_signature_change_item_lines(item: &str) -> Vec<String> {
    let Some((old, new)) = item.split_once(SIGNATURE_CHANGE_SEPARATOR) else {
        return vec![format!("{} {item}", style::accent("-"))];
    };
    paint_signature_change_lines(old, new)
}

#[cfg(test)]
fn signature_change_display_segments(item: &str) -> Vec<(&str, bool)> {
    let Some((old, new)) = item.split_once(SIGNATURE_CHANGE_SEPARATOR) else {
        return vec![(item, false)];
    };
    aligned_added_tokens(old, new)
        .into_iter()
        .map(|token| (token.text, token.changed))
        .collect()
}

fn paint_signature_change_lines(old: &str, new: &str) -> Vec<String> {
    if !old.contains('\n') && !new.contains('\n') {
        return vec![paint_signature_change_line(old, new)];
    }

    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    signature_line_diff(&old_lines, &new_lines)
        .into_iter()
        .map(paint_signature_line_diff)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignatureLineDiff<'a> {
    Context(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

fn signature_line_diff<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
) -> Vec<SignatureLineDiff<'a>> {
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let mut table = vec![vec![0usize; new_len + 1]; old_len + 1];

    for old_index in (0..old_len).rev() {
        for new_index in (0..new_len).rev() {
            table[old_index][new_index] = if old_lines[old_index] == new_lines[new_index] {
                table[old_index + 1][new_index + 1] + 1
            } else {
                table[old_index + 1][new_index].max(table[old_index][new_index + 1])
            };
        }
    }

    let mut diff = Vec::new();
    let (mut old_index, mut new_index) = (0usize, 0usize);
    while old_index < old_len && new_index < new_len {
        if old_lines[old_index] == new_lines[new_index] {
            diff.push(SignatureLineDiff::Context(new_lines[new_index]));
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            diff.push(SignatureLineDiff::Removed(old_lines[old_index]));
            old_index += 1;
        } else {
            diff.push(SignatureLineDiff::Added(new_lines[new_index]));
            new_index += 1;
        }
    }
    while old_index < old_len {
        diff.push(SignatureLineDiff::Removed(old_lines[old_index]));
        old_index += 1;
    }
    while new_index < new_len {
        diff.push(SignatureLineDiff::Added(new_lines[new_index]));
        new_index += 1;
    }
    diff
}

fn paint_signature_line_diff(line: SignatureLineDiff<'_>) -> String {
    match line {
        SignatureLineDiff::Context(line) => format!("{} {}", style::warn("~"), style::warn(line)),
        SignatureLineDiff::Added(line) => format!("{} {}", style::accent("+"), style::accent(line)),
        SignatureLineDiff::Removed(line) => format!("{} {}", style::error("-"), style::error(line)),
    }
}

fn paint_signature_change_line(old: &str, new: &str) -> String {
    let tokens = aligned_added_tokens(old, new);
    let mut rendered = style::warn("~ ");
    for token in tokens {
        if token.changed {
            rendered.push_str(&style::accent(token.text));
        } else {
            rendered.push_str(&style::warn(token.text));
        }
    }
    rendered
}

#[derive(Default)]
struct SemanticGroups {
    files: BTreeMap<String, FileSemanticGroups>,
    dependencies: Vec<(&'static str, Vec<String>)>,
    other: Vec<String>,
}

struct FileSemanticGroups {
    path: String,
    groups: Vec<(&'static str, Vec<String>)>,
}

impl FileSemanticGroups {
    fn new(path: String) -> Self {
        Self {
            path,
            groups: Vec::new(),
        }
    }

    fn push(&mut self, label: &'static str, item: String) {
        if let Some((_, items)) = self
            .groups
            .iter_mut()
            .find(|(existing, _)| *existing == label)
        {
            items.push(item);
        } else {
            self.groups.push((label, vec![item]));
        }
    }
}

fn group_semantic_changes(changes: &[SemanticChangeEntry]) -> SemanticGroups {
    let mut grouped = SemanticGroups::default();
    for change in changes {
        let kind = change.change_type.as_str();
        match kind {
            "file_added" => push_file_change(&mut grouped, change, "File", "added"),
            "file_deleted" => push_file_change(&mut grouped, change, "File", "deleted"),
            kind if kind.starts_with("file_modified") => {
                push_file_change(&mut grouped, change, "File", "modified")
            }
            "file_renamed" => push_file_rename(&mut grouped, change),
            "function_added" => push_function_change(&mut grouped, change, "Function added"),
            "function_extracted" => push_function_extracted(&mut grouped, change),
            "function_deleted" => push_function_change(&mut grouped, change, "Function deleted"),
            "function_renamed" => push_function_rename(&mut grouped, change),
            "function_modified" => push_function_change(&mut grouped, change, "Function modified"),
            "function_moved" => push_function_change(&mut grouped, change, "Function moved"),
            "signature_changed" => push_signature_change(&mut grouped, change),
            "dependency_added" => push_dependency_change(&mut grouped, "Added", change),
            "dependency_removed" => push_dependency_change(&mut grouped, "Removed", change),
            _ => grouped.other.push(change.description.clone()),
        }
    }
    grouped
}

fn push_file_change(
    grouped: &mut SemanticGroups,
    change: &SemanticChangeEntry,
    label: &'static str,
    item: &str,
) {
    let path = semantic_path(change);
    grouped
        .files
        .entry(path.clone())
        .or_insert_with(|| FileSemanticGroups::new(path))
        .push(label, item.to_string());
}

fn push_file_rename(grouped: &mut SemanticGroups, change: &SemanticChangeEntry) {
    let to_path = semantic_path(change);
    let item = change
        .from_path
        .as_ref()
        .map(|from| format!("{from} -> {to_path}"))
        .unwrap_or_else(|| change.description.clone());
    grouped
        .files
        .entry(to_path.clone())
        .or_insert_with(|| FileSemanticGroups::new(to_path))
        .push("File", item);
}

fn push_function_change(
    grouped: &mut SemanticGroups,
    change: &SemanticChangeEntry,
    label: &'static str,
) {
    let path = semantic_path(change);
    let item = change
        .new_name
        .as_deref()
        .or(change.old_name.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| change.description.clone());
    grouped
        .files
        .entry(path.clone())
        .or_insert_with(|| FileSemanticGroups::new(path))
        .push(label, item);
}

fn push_function_extracted(grouped: &mut SemanticGroups, change: &SemanticChangeEntry) {
    let path = semantic_path(change);
    let item = match (&change.new_name, &change.old_name) {
        (Some(name), Some(source)) => {
            let source = match change.from_path.as_deref() {
                Some(source_path) if source_path != path => format!("{source} ({source_path})"),
                _ => source.clone(),
            };
            format!("{name} from {source}")
        }
        (Some(name), None) => name.clone(),
        _ => change.description.clone(),
    };
    grouped
        .files
        .entry(path.clone())
        .or_insert_with(|| FileSemanticGroups::new(path))
        .push("Function extracted", item);
}

fn push_function_rename(grouped: &mut SemanticGroups, change: &SemanticChangeEntry) {
    let path = semantic_path(change);
    let item = match (&change.old_name, &change.new_name) {
        (Some(old), Some(new)) => format!("{old}{SIGNATURE_CHANGE_SEPARATOR}{new}"),
        _ => change.description.clone(),
    };
    grouped
        .files
        .entry(path.clone())
        .or_insert_with(|| FileSemanticGroups::new(path))
        .push("Function renamed", item);
}

fn push_signature_change(grouped: &mut SemanticGroups, change: &SemanticChangeEntry) {
    let path = semantic_path(change);
    let item = match (&change.old_name, &change.new_name) {
        (Some(old), Some(new)) => format!("{old}{SIGNATURE_CHANGE_SEPARATOR}{new}"),
        _ => change.description.clone(),
    };
    grouped
        .files
        .entry(path.clone())
        .or_insert_with(|| FileSemanticGroups::new(path))
        .push("Signature changed", item);
}

fn push_dependency_change(
    grouped: &mut SemanticGroups,
    label: &'static str,
    change: &SemanticChangeEntry,
) {
    if let Some((_, items)) = grouped
        .dependencies
        .iter_mut()
        .find(|(existing, _)| *existing == label)
    {
        items.push(change.description.clone());
    } else {
        grouped
            .dependencies
            .push((label, vec![change.description.clone()]));
    }
}

fn semantic_path(change: &SemanticChangeEntry) -> String {
    change
        .path
        .as_ref()
        .or(change.to_path.as_ref())
        .or(change.from_path.as_ref())
        .cloned()
        .unwrap_or_else(|| "(unknown path)".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        SIGNATURE_CHANGE_SEPARATOR, aligned_added_tokens, group_semantic_changes, paint_line,
        paint_signature_change_item_lines, render_diff_patch,
        signature_change_display_segments,
    };
    use crate::cli::commands::diff::diff_types::{
        DiffOutput, FileChange, FileEolState, LineDiff, SemanticChangeEntry, change_line_counts,
        should_render_modified_pair,
    };

    fn modified_change_with_eol(
        path: &str,
        lines: Vec<LineDiff>,
        eol: FileEolState,
    ) -> FileChange {
        FileChange {
            path: path.to_string(),
            kind: "modified".to_string(),
            lines: Some(lines),
            eol,
            ..Default::default()
        }
    }

    fn diff_output_with(changes: Vec<FileChange>) -> DiffOutput {
        DiffOutput::new(None, None, changes, None, None, None)
    }

    /// `lines: None` is the binary / unreadable case. The patch
    /// renderer must skip it without emitting a header (otherwise
    /// downstream tools would see `--- a/foo` `+++ b/foo` followed by
    /// nothing, which `patch(1)` reads as a malformed hunk).
    #[test]
    fn render_diff_patch_skips_change_with_no_lines() {
        let change = FileChange {
            path: "binary.bin".to_string(),
            kind: "modified".to_string(),
            binary: true,
            lines: None,
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.is_empty(),
            "binary-only change must emit nothing: {rendered:?}"
        );
    }

    /// A change whose `lines` vector is present but empty must also
    /// be skipped — the file path is known but there's no hunk body
    /// to render. Mixed batches (one renderable, one empty) must keep
    /// rendering the renderable change.
    #[test]
    fn render_diff_patch_skips_change_with_empty_lines() {
        let empty = FileChange {
            path: "empty.txt".to_string(),
            kind: "modified".to_string(),
            lines: Some(Vec::new()),
            ..Default::default()
        };
        let real = modified_change_with_eol(
            "real.txt",
            vec![
                LineDiff::with_lines("@", "@ -1,1 +1,1 @@", None, None),
                LineDiff::with_lines("-", "old", Some(1), None),
                LineDiff::with_lines("+", "new", None, Some(1)),
            ],
            FileEolState::default(),
        );
        let rendered = render_diff_patch(&diff_output_with(vec![empty, real]));
        assert!(
            !rendered.contains("empty.txt"),
            "skipped change must not emit a header: {rendered}"
        );
        assert!(
            rendered.contains("--- a/real.txt"),
            "renderable change must still be emitted: {rendered}"
        );
    }

    /// When both sides lack a trailing newline AND their tails land on
    /// the same context line, the renderer emits the line once and a
    /// single `\ No newline at end of file` marker that documents both
    /// sides' state. `git diff` does the same — two markers in a row
    /// would be a corruption.
    #[test]
    fn render_diff_patch_collapses_both_side_no_eol_marker_on_shared_tail() {
        // `more` is the tail for both sides; the change is on the line
        // above (hello -> world). Both blobs end without `\n`.
        let lines = vec![
            LineDiff::with_lines("@", "@ -1,2 +1,2 @@", None, None),
            LineDiff::with_lines("-", "hello", Some(1), None),
            LineDiff::with_lines("+", "world", None, Some(1)),
            LineDiff::with_lines(" ", "more", Some(2), Some(2)),
        ];
        let eol = FileEolState {
            old_has_final_newline: false,
            new_has_final_newline: false,
            old_line_count: 2,
            new_line_count: 2,
        };
        let change = modified_change_with_eol("tail.txt", lines, eol);
        let rendered = render_diff_patch(&diff_output_with(vec![change]));

        let marker_count = rendered.matches("\\ No newline at end of file").count();
        assert_eq!(
            marker_count, 1,
            "shared-tail double-no-eol must emit exactly one marker, got:\n{rendered}"
        );
        // The context line must NOT be split into `-more`/`+more` —
        // that's the wrong branch and would confuse `git apply` about
        // whether the line is being modified.
        assert!(
            !rendered.contains("-more\n"),
            "context tail must not be split when both sides agree:\n{rendered}"
        );
        assert!(
            !rendered.contains("+more\n"),
            "context tail must not be split when both sides agree:\n{rendered}"
        );
        assert!(
            rendered.contains(" more\n\\ No newline at end of file\n"),
            "marker must sit immediately after the shared context line:\n{rendered}"
        );
    }

    /// When only the OLD side lacks a trailing newline and its tail is
    /// a context line, the renderer must split that line into a
    /// `-content` (with the marker after it) + `+content` pair so the
    /// patch unambiguously documents that the OLD file ends without
    /// `\n` while the NEW file ends with one.
    #[test]
    fn render_diff_patch_splits_context_tail_when_only_old_lacks_newline() {
        // Diff for OLD `hello` (no eol) -> NEW `hello\nmore\n`:
        // ` hello` is the old tail; `+more` is the trailing addition.
        let lines = vec![
            LineDiff::with_lines("@", "@ -1,1 +1,2 @@", None, None),
            LineDiff::with_lines(" ", "hello", Some(1), Some(1)),
            LineDiff::with_lines("+", "more", None, Some(2)),
        ];
        let eol = FileEolState {
            old_has_final_newline: false,
            new_has_final_newline: true,
            old_line_count: 1,
            new_line_count: 2,
        };
        let change = modified_change_with_eol("old.txt", lines, eol);
        let rendered = render_diff_patch(&diff_output_with(vec![change]));

        assert!(
            rendered.contains("-hello\n\\ No newline at end of file\n+hello\n"),
            "OLD-side context-tail split must emit `-hello` + marker + `+hello`:\n{rendered}"
        );
        // Only the OLD side carries a marker — the NEW side ends with
        // `\n` so its tail line must NOT be followed by a marker.
        let marker_count = rendered.matches("\\ No newline at end of file").count();
        assert_eq!(
            marker_count, 1,
            "exactly one marker expected (OLD side only):\n{rendered}"
        );
    }

    /// Mirror of the OLD-only case: when only the NEW side lacks a
    /// trailing newline and its tail is a shared context line, the
    /// split emits `-content` + `+content` + marker so the patch
    /// states "the file ends without `\n` after applying".
    #[test]
    fn render_diff_patch_splits_context_tail_when_only_new_lacks_newline() {
        // Diff for OLD `hello\nmore\n` -> NEW `hello` (no eol):
        // ` hello` is the new tail; `-more` is the removal.
        let lines = vec![
            LineDiff::with_lines("@", "@ -1,2 +1,1 @@", None, None),
            LineDiff::with_lines(" ", "hello", Some(1), Some(1)),
            LineDiff::with_lines("-", "more", Some(2), None),
        ];
        let eol = FileEolState {
            old_has_final_newline: true,
            new_has_final_newline: false,
            old_line_count: 2,
            new_line_count: 1,
        };
        let change = modified_change_with_eol("new.txt", lines, eol);
        let rendered = render_diff_patch(&diff_output_with(vec![change]));

        assert!(
            rendered.contains("-hello\n+hello\n\\ No newline at end of file\n"),
            "NEW-side context-tail split must emit `-hello` + `+hello` + marker:\n{rendered}"
        );
        let marker_count = rendered.matches("\\ No newline at end of file").count();
        assert_eq!(
            marker_count, 1,
            "exactly one marker expected (NEW side only):\n{rendered}"
        );
    }

    /// When the tail is a `-` (deletion) on the OLD side and the OLD
    /// blob lacked a trailing newline, the marker goes right after the
    /// `-line` — same as `git diff` for a delete-the-last-line patch
    /// against a no-eol source. The `+` branch is the mirror.
    #[test]
    fn render_diff_patch_marker_after_minus_line_when_old_tail_is_deletion() {
        // OLD has 2 lines (no eol on `tail`), NEW has 1 line (`only`,
        // with eol). The diff is two replacements; the second `-tail`
        // is the OLD tail.
        let lines = vec![
            LineDiff::with_lines("@", "@ -1,2 +1,1 @@", None, None),
            LineDiff::with_lines("-", "only", Some(1), None),
            LineDiff::with_lines("-", "tail", Some(2), None),
            LineDiff::with_lines("+", "only", None, Some(1)),
        ];
        let eol = FileEolState {
            old_has_final_newline: false,
            new_has_final_newline: true,
            old_line_count: 2,
            new_line_count: 1,
        };
        let change = modified_change_with_eol("del.txt", lines, eol);
        let rendered = render_diff_patch(&diff_output_with(vec![change]));

        assert!(
            rendered.contains("-tail\n\\ No newline at end of file\n"),
            "marker must follow the OLD tail deletion line:\n{rendered}"
        );
    }

    #[test]
    fn modified_pair_compacts_only_when_lines_share_context() {
        assert!(should_render_modified_pair(
            "    let value = 41;",
            "    let value = 42;"
        ));
        assert!(should_render_modified_pair(
            "    object::{Blob, ContentHash, EntryType, FileMode, Tree, TreeEntry},",
            "    object::{Blob, ContentHash, EntryType, FileMode, SemanticChange, Tree, TreeEntry},"
        ));
    }

    #[test]
    fn unrelated_adjacent_delete_add_lines_do_not_compact() {
        assert!(!should_render_modified_pair(
            "        return get_blob_recursive(store, &subtree, &parts[1..]);",
            "fn put_blob(store: &InMemoryStore, content: &str) -> ContentHash {"
        ));
        assert!(!should_render_modified_pair("    Ok(None)", "fn put_tree("));
    }

    #[test]
    fn modified_pair_aligns_insertions_around_existing_tokens() {
        let tokens = aligned_added_tokens(
            "    collections::HashMap,",
            "    collections::{HashMap, HashSet},",
        );
        let mut rendered = String::new();
        let mut in_changed_span = false;
        for token in tokens {
            if token.changed && !in_changed_span {
                rendered.push('[');
                in_changed_span = true;
            } else if !token.changed && in_changed_span {
                rendered.push(']');
                in_changed_span = false;
            }
            rendered.push_str(token.text);
        }
        if in_changed_span {
            rendered.push(']');
        }

        assert_eq!(rendered, "    collections::[{]HashMap[, HashSet}],");
    }

    #[test]
    fn line_renderer_shows_old_and_new_line_numbers() {
        let line = LineDiff::with_lines(" ", "let value = 42;", Some(7), Some(8));

        let rendered = paint_line(&line);
        assert!(rendered.contains("   7    8 | "));
        assert!(rendered.ends_with(" let value = 42;"));
    }

    #[test]
    fn stat_counts_pure_insertions_as_additions() {
        let lines = vec![
            LineDiff::with_lines("@", "@ -1,1 +1,2 @@", None, None),
            LineDiff::with_lines(" ", "base", Some(1), Some(1)),
            LineDiff::with_lines("+", "added", None, Some(2)),
        ];

        let counts = change_line_counts(Some(&lines));
        assert_eq!(counts.added, 1);
        assert_eq!(counts.modified, 0);
        assert_eq!(counts.deleted, 0);
    }

    #[test]
    fn semantic_changes_group_by_file_then_type() {
        let changes = vec![
            semantic_entry(
                "function_extracted",
                "src/lib.rs",
                Some("render_diff"),
                Some("is_blank_or_visual_decoration"),
            ),
            semantic_entry(
                "function_extracted",
                "src/lib.rs",
                None,
                Some("is_visual_decoration_line"),
            ),
            semantic_entry("function_deleted", "src/lib.rs", Some("old_helper"), None),
        ];

        let grouped = group_semantic_changes(&changes);
        let file = grouped.files.get("src/lib.rs").unwrap();

        assert_eq!(file.groups[0].0, "Function extracted");
        assert_eq!(
            file.groups[0].1,
            vec![
                "is_blank_or_visual_decoration from render_diff".to_string(),
                "is_visual_decoration_line".to_string()
            ]
        );
        assert_eq!(file.groups[1].0, "Function deleted");
        assert_eq!(file.groups[1].1, vec!["old_helper".to_string()]);
    }

    #[test]
    fn semantic_changes_show_cross_file_extraction_source() {
        let mut change = semantic_entry(
            "function_extracted",
            "src/new.rs",
            Some("render_diff"),
            Some("is_blank_or_visual_decoration"),
        );
        change.from_path = Some("src/old.rs".to_string());

        let grouped = group_semantic_changes(&[change]);
        let file = grouped.files.get("src/new.rs").unwrap();

        assert_eq!(
            file.groups[0].1,
            vec!["is_blank_or_visual_decoration from render_diff (src/old.rs)".to_string()]
        );
    }

    #[test]
    fn semantic_signature_change_segments_changed_signature_once() {
        let item = format!(
            "fn parse(input: &str) -> Result<()>{SIGNATURE_CHANGE_SEPARATOR}fn parse(input: &str, mode: Mode) -> Result<()>"
        );
        let segments = signature_change_display_segments(&item);
        let mut rendered = String::new();
        let mut in_changed_span = false;
        for (text, changed) in segments {
            if changed && !in_changed_span {
                rendered.push('[');
                in_changed_span = true;
            } else if !changed && in_changed_span {
                rendered.push(']');
                in_changed_span = false;
            }
            rendered.push_str(text);
        }
        if in_changed_span {
            rendered.push(']');
        }

        assert_eq!(
            rendered,
            "fn parse(input: &str[, mode: Mode]) -> Result<()>"
        );
    }

    #[test]
    fn semantic_multiline_signature_change_marks_inserted_lines() {
        let item = format!(
            "cmd_diff (\n    cli: &Cli,\n    show_context: bool,\n){SIGNATURE_CHANGE_SEPARATOR}cmd_diff (\n    cli: &Cli,\n    unified: usize,\n    show_context: bool,\n)"
        );

        let rendered = paint_signature_change_item_lines(&item)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "~ cmd_diff (",
                "~     cli: &Cli,",
                "+     unified: usize,",
                "~     show_context: bool,",
                "~ )",
            ]
        );
    }

    #[test]
    fn semantic_multiline_signature_change_preserves_removed_lines() {
        let item = format!(
            "get_blob_recursive <S: ObjectStore + ?Sized> (\n    store: &S,\n    tree: &Tree,\n    parts: &[&str],\n){SIGNATURE_CHANGE_SEPARATOR}get_blob_recursive (\n        &self,\n        tree: &Tree,\n        parts: &[&str],\n    )"
        );

        let rendered = paint_signature_change_item_lines(&item)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "- get_blob_recursive <S: ObjectStore + ?Sized> (",
                "-     store: &S,",
                "-     tree: &Tree,",
                "-     parts: &[&str],",
                "- )",
                "+ get_blob_recursive (",
                "+         &self,",
                "+         tree: &Tree,",
                "+         parts: &[&str],",
                "+     )",
            ]
        );
    }

    #[test]
    fn semantic_signature_group_uses_internal_separator_for_rendering() {
        let changes = vec![semantic_entry(
            "signature_changed",
            "src/lib.rs",
            Some("fn run(a: A)"),
            Some("fn run(a: A, b: B)"),
        )];

        let grouped = group_semantic_changes(&changes);
        let file = grouped.files.get("src/lib.rs").unwrap();

        assert_eq!(file.groups[0].0, "Signature changed");
        assert_eq!(
            file.groups[0].1,
            vec![format!(
                "fn run(a: A){SIGNATURE_CHANGE_SEPARATOR}fn run(a: A, b: B)"
            )]
        );
    }

    #[test]
    fn semantic_changes_keep_dependencies_out_of_file_groups() {
        let mut dependency = semantic_entry("dependency_added", "Cargo.toml", None, None);
        dependency.description = "Dependency added: serde@1".to_string();

        let grouped = group_semantic_changes(&[dependency]);

        assert!(grouped.files.is_empty());
        assert_eq!(grouped.dependencies[0].0, "Added");
        assert_eq!(
            grouped.dependencies[0].1,
            vec!["Dependency added: serde@1".to_string()]
        );
    }

    fn semantic_entry(
        change_type: &str,
        path: &str,
        old_name: Option<&str>,
        new_name: Option<&str>,
    ) -> SemanticChangeEntry {
        SemanticChangeEntry {
            change_type: change_type.to_string(),
            description: format!("{change_type}: {path}"),
            path: Some(path.to_string()),
            from_path: None,
            to_path: None,
            old_name: old_name.map(ToString::to_string),
            new_name: new_name.map(ToString::to_string),
            importance: None,
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut stripped = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for ch in chars.by_ref() {
                    if ch == 'm' {
                        break;
                    }
                }
            } else {
                stripped.push(ch);
            }
        }
        stripped
    }
}
