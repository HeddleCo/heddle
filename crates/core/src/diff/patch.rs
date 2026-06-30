// SPDX-License-Identifier: Apache-2.0
//! Patch-compatible rendering for diff reports.

use std::io::{self, Write};

use objects::object::FileMode;

use super::{DiffReport, FileChange, LineDiff};

pub fn write_diff_patch<W: Write>(output: &DiffReport, writer: &mut W) -> io::Result<()> {
    for change in &output.changes {
        // A symlink change carries its raw target bytes in `change.symlink`,
        // which on Unix need not be valid UTF-8. Render it byte-exact so a
        // non-UTF-8 link target round-trips through `git apply`; every other
        // change is UTF-8 text and is appended as its bytes.
        if change.symlink.is_some() {
            write_symlink_change(change, writer)?;
        } else {
            write_text_change(change, writer)?;
        }
    }
    Ok(())
}

pub fn render_diff_patch_bytes(output: &DiffReport) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    write_diff_patch(output, &mut buf).expect("writing diff patch to Vec cannot fail");
    buf
}

/// Lossy String view of the byte-exact patch (`render_diff_patch_bytes`),
/// for the JSON `.patch` field and String-based callers/tests. Only a
/// non-UTF-8 symlink target (Unix-only) differs from the byte render; JSON
/// strings cannot carry raw bytes, so a lossy view is the best a String
/// surface can do. The round-trip surface (`heddle diff --patch`) writes the
/// bytes directly via `render_diff_patch_bytes`, so its byte fidelity is
/// never reduced here.
pub fn render_diff_patch(output: &DiffReport) -> String {
    String::from_utf8_lossy(&render_diff_patch_bytes(output)).into_owned()
}

/// Render one non-symlink change as unified-diff text into `writer`. Symlink
/// changes never reach here — `write_diff_patch` routes them to
/// `write_symlink_change`, which preserves a non-UTF-8 target — so a symlink
/// target is never forced through `change.lines` (which a non-UTF-8 target
/// cannot populate) or `write_binary_change`.
fn write_text_change<W: Write>(change: &FileChange, writer: &mut W) -> io::Result<()> {
    let lines_ref = change.lines.as_deref();
    let has_hunk_body = lines_ref.is_some_and(|lines| lines.iter().any(|line| line.prefix != " "));
    let old_path = change.old_path.as_deref().unwrap_or(&change.path);
    let is_rename = change
        .old_path
        .as_deref()
        .is_some_and(|old| old != change.path);
    let is_added = change.kind == "added";
    let is_deleted = change.kind == "deleted";
    let is_modified = !is_rename && !is_added && !is_deleted;
    // A mode-only modify (chmod / exec-bit flip / type swap) has no
    // hunk body but is still a real change: git records it as
    // `old mode`/`new mode` extended headers and `git apply`
    // reproduces the permission change from those alone.
    let mode_changed = is_modified
        && matches!((change.old_mode, change.mode), (Some(old), Some(new)) if old != new);
    // `lines: None` is the binary / unreadable case — there is no
    // text body to render, so it never produces a patch regardless
    // of kind. `lines: Some(_)` (even empty) means we have a
    // readable text side.
    let has_text = change.lines.is_some();

    // A binary *content* change (add/delete/modify of a file heddle
    // cannot diff as text). heddle has no git binary delta to emit
    // (its blob hashes are not git SHAs), and silently dropping the
    // change would let `git apply` "succeed" while the binary content
    // stays stale — the false round-trip cid 3319484747 flagged. Emit
    // git's `Binary files … differ` marker with a *placeholder* index
    // line: that index line is what makes `git apply` recognize a
    // binary patch and refuse the *whole* patch ("without full index
    // line") instead of skipping the block. Without the index line git
    // treats the marker as an empty patch and silently ignores it. A
    // content-identical mode-only change is never `binary` (the diff
    // readers short-circuit it to an empty text body), so this only
    // fires on a real binary content change, never a chmod.
    if change.binary && !is_rename {
        write_binary_change(change, is_added, is_deleted, mode_changed, writer)?;
        return Ok(());
    }

    // Decide whether this change emits anything at all:
    // * renames always do (the extended headers carry the move even
    //   for identical content);
    // * add/delete do whenever there's a readable text side — the
    //   empty-file case renders header-only;
    // * a modify renders only when it has a real hunk body. A modify
    //   with no body and matching EOL is a no-op; the
    //   trailing-newline-only case is handled upstream in
    //   `unified_hunks`, which synthesizes a tail hunk so this
    //   branch sees `has_hunk_body == true`.
    let should_render = if is_rename {
        true
    } else if is_added || is_deleted {
        has_text
    } else {
        has_hunk_body || mode_changed
    };
    if !should_render {
        return Ok(());
    }

    if is_rename {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", old_path),
            quote_path_for_patch("b/", &change.path)
        )?;
        // A rename paired with a chmod/type change (`old.sh` renamed
        // to `new.sh` and made executable) carries both modes; emit
        // the `old mode`/`new mode` pair before `similarity index`,
        // matching `git diff`, so `git apply` reproduces the
        // permission change as well as the move.
        if let (Some(old), Some(new)) = (change.old_mode, change.mode)
            && old != new
        {
            writeln!(writer, "old mode {}", mode_str(change.old_mode))?;
            writeln!(writer, "new mode {}", mode_str(change.mode))?;
        }
        let pct = (change.similarity_score.unwrap_or(1.0).clamp(0.0, 1.0) * 100.0).round() as u32;
        writeln!(writer, "similarity index {pct}%")?;
        writeln!(writer, "rename from {}", quote_path_for_patch("", old_path))?;
        writeln!(
            writer,
            "rename to {}",
            quote_path_for_patch("", &change.path)
        )?;
        // Pure rename — extended headers alone suffice; emitting
        // `--- a/old / +++ b/new` without hunks would tell git to
        // apply an empty patch and warn about a stray header.
        if !has_hunk_body {
            return Ok(());
        }
    } else if is_added {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "new file mode {}", mode_str(change.mode))?;
    } else if is_deleted {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "deleted file mode {}", mode_str(change.mode))?;
    } else if mode_changed {
        // A modify whose mode changed (with or without a content
        // hunk). Emit the `diff --git` + `old mode`/`new mode`
        // header pair.
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "old mode {}", mode_str(change.old_mode))?;
        writeln!(writer, "new mode {}", mode_str(change.mode))?;
    } else {
        // A plain content modify. Emit the `diff --git` header so
        // every file stanza is self-delimiting. A bare `--- a/<path>`
        // is ambiguous: git's parser binds it to the *preceding*
        // `diff --git` stanza when one is still open — e.g. a
        // header-only empty-add (`diff --git ... / new file mode`) or
        // a mode-only change immediately above — and misreads this
        // file's `---` as the prior file's source side, corrupting the
        // patch ("expected /dev/null"). The explicit header closes the
        // prior stanza and opens this one. (cid 3319484717 ordering.)
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
    }

    // An empty-file add/delete (text side present but zero lines)
    // has no hunk body. git stops after the `new/deleted file mode`
    // header in that case and `git apply` still creates/unlinks the
    // path — emitting `--- /+++/@@` with no `@@` body would be a
    // malformed hunk, so we stop here too.
    if (is_added || is_deleted) && !has_hunk_body {
        return Ok(());
    }
    // A mode-only modify carries no content hunk: the `old mode`/
    // `new mode` header pair is the entire patch, so stop before the
    // `--- /+++` line-diff headers (which would be a malformed
    // empty hunk).
    if is_modified && !has_hunk_body {
        return Ok(());
    }

    if is_added {
        writer.write_all(b"--- /dev/null\n")?;
    } else {
        writeln!(writer, "--- {}", quote_path_for_patch("a/", old_path))?;
    }
    if is_deleted {
        writer.write_all(b"+++ /dev/null\n")?;
    } else {
        writeln!(writer, "+++ {}", quote_path_for_patch("b/", &change.path))?;
    }
    if let Some(lines) = lines_ref {
        write_patch_hunks(change, lines, writer)?;
    }
    Ok(())
}

/// Render a symlink change (add / delete / target-edit / rename) byte-exact.
///
/// A symlink's git blob is its raw target bytes, which on Unix need not be
/// valid UTF-8 — so the hunk body is emitted straight from `change.symlink`
/// (the single byte-preserving symlink path) rather than `change.lines`,
/// which a non-UTF-8 target cannot populate. Marking such a change `binary`
/// (the old behaviour) emitted a placeholder-binary stanza that `git apply`
/// rejects for a `120000` entry; emitting the target as a text hunk is what
/// git itself does and round-trips. The extended headers mirror
/// `write_text_change`'s (add/delete/rename), and the mode is always
/// `120000` so a rename never needs an `old mode`/`new mode` pair unless the
/// two sides genuinely differ.
fn write_symlink_change<W: Write>(change: &FileChange, writer: &mut W) -> io::Result<()> {
    let Some(sym) = change.symlink.as_ref() else {
        return Ok(());
    };
    let old_path = change.old_path.as_deref().unwrap_or(&change.path);
    let is_rename = change
        .old_path
        .as_deref()
        .is_some_and(|old| old != change.path);
    let is_added = change.kind == "added";
    let is_deleted = change.kind == "deleted";

    if is_rename {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", old_path),
            quote_path_for_patch("b/", &change.path)
        )?;
        if let (Some(old), Some(new)) = (change.old_mode, change.mode)
            && old != new
        {
            writeln!(writer, "old mode {}", mode_str(change.old_mode))?;
            writeln!(writer, "new mode {}", mode_str(change.mode))?;
        }
        let pct = (change.similarity_score.unwrap_or(1.0).clamp(0.0, 1.0) * 100.0).round() as u32;
        writeln!(writer, "similarity index {pct}%")?;
        writeln!(writer, "rename from {}", quote_path_for_patch("", old_path))?;
        writeln!(
            writer,
            "rename to {}",
            quote_path_for_patch("", &change.path)
        )?;
        // Pure rename (identical target) — the extended headers alone carry
        // the move, exactly like a text rename with no hunk body.
        if sym.old == sym.new {
            return Ok(());
        }
        writeln!(writer, "--- {}", quote_path_for_patch("a/", old_path))?;
        writeln!(writer, "+++ {}", quote_path_for_patch("b/", &change.path))?;
    } else if is_added {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "new file mode {}", mode_str(change.mode))?;
        writer.write_all(b"--- /dev/null\n")?;
        writeln!(writer, "+++ {}", quote_path_for_patch("b/", &change.path))?;
    } else if is_deleted {
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "deleted file mode {}", mode_str(change.mode))?;
        writeln!(writer, "--- {}", quote_path_for_patch("a/", &change.path))?;
        writer.write_all(b"+++ /dev/null\n")?;
    } else {
        // A symlink target-edit. The mode is unchanged (`120000` → `120000`),
        // so no `old mode`/`new mode` block — just the file header. An
        // identical target would be a no-op and is never emitted by the diff
        // backends, but guard it so an accidental empty hunk can't form.
        if sym.old == sym.new {
            return Ok(());
        }
        writeln!(
            writer,
            "diff --git {} {}",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        )?;
        writeln!(writer, "--- {}", quote_path_for_patch("a/", &change.path))?;
        writeln!(writer, "+++ {}", quote_path_for_patch("b/", &change.path))?;
    }

    write_symlink_hunk(sym.old.as_deref(), sym.new.as_deref(), writer)?;
    Ok(())
}

/// Emit the unified-diff hunk for a symlink's target bytes. A symlink's git
/// blob has no trailing newline, so each side normally collapses to a single
/// line carrying the `\ No newline at end of file` marker; a target that
/// embeds a `\n` (pathological but representable) splits into multiple lines.
/// The `@@` header mirrors `unified_hunks`'s `@@ -s,c +s,c @@` shape (counts
/// always written, even `,1`), which `git apply` accepts.
fn write_symlink_hunk<W: Write>(
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    writer: &mut W,
) -> io::Result<()> {
    let old_lines = split_target_lines(old);
    let new_lines = split_target_lines(new);
    let old_count = old_lines.len();
    let new_count = new_lines.len();
    let old_start = if old_count == 0 { 0 } else { 1 };
    let new_start = if new_count == 0 { 0 } else { 1 };
    writeln!(
        writer,
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
    )?;
    let old_no_eol = !target_has_trailing_newline(old);
    let new_no_eol = !target_has_trailing_newline(new);
    for (idx, line) in old_lines.iter().enumerate() {
        writer.write_all(b"-")?;
        writer.write_all(line)?;
        writer.write_all(b"\n")?;
        if old_no_eol && idx + 1 == old_count {
            writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
        }
    }
    for (idx, line) in new_lines.iter().enumerate() {
        writer.write_all(b"+")?;
        writer.write_all(line)?;
        writer.write_all(b"\n")?;
        if new_no_eol && idx + 1 == new_count {
            writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
        }
    }
    Ok(())
}

/// Split a symlink target's raw bytes into unified-diff lines. An absent side
/// (`None`) or an empty blob yields no lines; a trailing `\n` is the line
/// terminator (dropped here, surfaced via `target_has_trailing_newline`)
/// rather than an extra empty line, matching how text blobs are line-counted.
fn split_target_lines(target: Option<&[u8]>) -> Vec<&[u8]> {
    let Some(bytes) = target else {
        return Vec::new();
    };
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
    if bytes.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

fn target_has_trailing_newline(target: Option<&[u8]>) -> bool {
    target.is_some_and(|bytes| bytes.ends_with(b"\n"))
}

/// Render a binary content change (add / delete / plain modify / modify
/// with a mode change) as git's `Binary files … differ` marker.
///
/// heddle cannot emit a git binary delta — its blob hashes are not git
/// SHAs — so the marker is the most faithful thing it can produce. The
/// catch (cid 3319484747): a bare `Binary files … differ` marker with no
/// `index` header is treated by `git apply` as an empty patch and
/// *silently skipped*, which would let the apply "succeed" while the
/// binary content stays stale. Emitting a *placeholder* `index
/// 0000000..0000000` line flips git into binary-patch mode, where it
/// refuses the whole patch ("cannot apply binary patch … without full
/// index line") rather than ignoring it. That refusal is the correct
/// outcome: heddle has no delta to apply, so the honest result is a hard
/// failure, never a false round-trip.
fn write_binary_change<W: Write>(
    change: &FileChange,
    is_added: bool,
    is_deleted: bool,
    mode_changed: bool,
    writer: &mut W,
) -> io::Result<()> {
    let path = &change.path;
    writeln!(
        writer,
        "diff --git {} {}",
        quote_path_for_patch("a/", path),
        quote_path_for_patch("b/", path)
    )?;
    if is_added {
        writeln!(writer, "new file mode {}", mode_str(change.mode))?;
        writer.write_all(b"index 0000000..0000000\n")?;
    } else if is_deleted {
        writeln!(writer, "deleted file mode {}", mode_str(change.mode))?;
        writer.write_all(b"index 0000000..0000000\n")?;
    } else if mode_changed {
        writeln!(writer, "old mode {}", mode_str(change.old_mode))?;
        writeln!(writer, "new mode {}", mode_str(change.mode))?;
        writer.write_all(b"index 0000000..0000000\n")?;
    } else {
        // Plain binary modify: git stamps the mode at the end of the
        // index line (`index <old>..<new> 100644`).
        writeln!(writer, "index 0000000..0000000 {}", mode_str(change.mode))?;
    }
    let (a, b) = if is_added {
        ("/dev/null".to_string(), quote_path_for_patch("b/", path))
    } else if is_deleted {
        (quote_path_for_patch("a/", path), "/dev/null".to_string())
    } else {
        (
            quote_path_for_patch("a/", path),
            quote_path_for_patch("b/", path),
        )
    };
    writeln!(writer, "Binary files {a} and {b} differ")?;
    Ok(())
}

/// Map a tracked file mode to the git unified-diff mode string. `None`
/// (mode not resolved) and the regular-file case both render `100644`.
fn mode_str(mode: Option<FileMode>) -> &'static str {
    match mode {
        Some(FileMode::Executable) => "100755",
        Some(FileMode::Symlink) => "120000",
        Some(FileMode::Normal) | None => "100644",
    }
}

/// Quote a patch-header path the way `git diff` does (C-style quoting,
/// `core.quotePath` defaults to true). A path containing a tab, newline,
/// double-quote, backslash, control byte, or non-ASCII byte is wrapped in
/// double quotes with the bytes escaped; a "simple" path is emitted bare.
///
/// `prefix` is the in-quote prefix git stamps on `diff --git`/`--- `/`+++ `
/// headers (`a/`, `b/`) — git puts the prefix *inside* the quotes
/// (`"a/tab\there"`), so it is escaped alongside the path. `rename from`/
/// `rename to` pass an empty prefix (git quotes the bare path there).
///
/// Verified byte-for-byte against `git diff` for tab, newline, quote,
/// backslash, and non-ASCII (UTF-8 → per-byte octal) paths.
fn quote_path_for_patch(prefix: &str, path: &str) -> String {
    if !needs_c_quoting(prefix) && !needs_c_quoting(path) {
        return format!("{prefix}{path}");
    }
    let mut out = String::with_capacity(prefix.len() + path.len() + 2);
    out.push('"');
    push_c_quoted(&mut out, prefix);
    push_c_quoted(&mut out, path);
    out.push('"');
    out
}

fn needs_c_quoting(s: &str) -> bool {
    s.bytes().any(byte_needs_escape)
}

/// git escapes any byte below 0x20, the DEL byte and everything above it
/// (0x7f..=0xff — `core.quotePath` octal-escapes non-ASCII), plus the two
/// in-quote metacharacters `"` and `\`.
fn byte_needs_escape(byte: u8) -> bool {
    matches!(byte, b'"' | b'\\') || !(0x20..0x7f).contains(&byte)
}

fn push_c_quoted(out: &mut String, s: &str) {
    for byte in s.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            0x0d => out.push_str("\\r"),
            0x20..=0x7e => out.push(byte as char),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
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
/// the terminator. The 4-case matrix is in `write_patch_hunks`'s
/// context-line branch.
fn write_patch_hunks<W: Write>(
    change: &FileChange,
    lines: &[LineDiff],
    writer: &mut W,
) -> io::Result<()> {
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
                write_patch_line(writer, line)?;
                writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
            } else {
                // Mixed state: at least one side needs the marker and
                // the other shouldn't be tagged. Split the context
                // line into a `-content` / `+content` pair so each
                // side's marker (or its absence) is unambiguous.
                writer.write_all(b"-")?;
                writer.write_all(line.content.as_bytes())?;
                writer.write_all(b"\n")?;
                if needs_old_marker {
                    writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
                }
                writer.write_all(b"+")?;
                writer.write_all(line.content.as_bytes())?;
                writer.write_all(b"\n")?;
                if needs_new_marker {
                    writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
                }
            }
            continue;
        }

        write_patch_line(writer, line)?;
        if needs_old_marker && line.prefix == "-" {
            writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
        }
        if needs_new_marker && line.prefix == "+" {
            writer.write_all(NO_NEWLINE_MARKER.as_bytes())?;
        }
    }
    Ok(())
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

fn write_patch_line<W: Write>(writer: &mut W, line: &LineDiff) -> io::Result<()> {
    writer.write_all(line.prefix.as_bytes())?;
    writer.write_all(line.content.as_bytes())?;
    writer.write_all(b"\n")
}
