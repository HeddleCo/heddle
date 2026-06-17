// SPDX-License-Identifier: Apache-2.0
//! Output formatting for diff command.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    process::{Command, Stdio},
};

use objects::object::FileMode;

use super::{
    diff_compute::trim_added_decorations_for_display,
    diff_types::{
        DiffOutput, FileChange, LineDiff, SemanticChangeEntry, should_render_modified_pair,
    },
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
/// inline-edit `~` lines, no ANSI styling. Output targets a clean
/// `git apply` round-trip; `patch(1)` compatibility is best-effort (it
/// does not consume git's extended headers, so type changes and empty
/// add/delete hunks are git-apply-only). Each line is `prefix + content`
/// because the hunk-header `LineDiff` already encodes the second `@`
/// (prefix=`@`, content=`@ -a,b +c,d @@`), so concatenation yields the
/// canonical `@@ -a,b +c,d @@` shape.
///
/// Output is `Vec<u8>`, not `String`, because a symlink's target — which
/// git stores as the link's git blob and the patch carries verbatim in the
/// hunk body — is an arbitrary byte sequence on Unix and need not be valid
/// UTF-8. This is the single byte-preserving patch path; the lossy
/// `render_diff_patch` (used for the JSON `.patch` field) is derived from it.
///
/// Four cases require git's extended header block to round-trip:
///
/// * **Added files** get `diff --git ... / new file mode <mode> /
///   --- /dev/null`. Without it, `git apply` (and `patch -p1`) demand
///   that `b/<path>` already exist on the target side, which defeats
///   the whole point of an add hunk. `<mode>` reflects the real file
///   type (`100755` for an executable, `120000` for a symlink) so the
///   round-trip preserves it.
/// * **Deleted files** get `diff --git ... / deleted file mode <mode> /
///   +++ /dev/null`. `git apply --check` tolerates the bare `+++ b/<path>`
///   shape, but actually applying it leaves an empty file behind instead
///   of removing the path — the `+++ /dev/null` + deleted-mode header is
///   what tells git to unlink it.
/// * **Renames** get `diff --git a/old b/new / similarity index N% /
///   rename from old / rename to new`. Pure renames (no edits) emit
///   the extended headers and stop; rename-with-edit appends the
///   usual `--- a/old / +++ b/new` + hunk body.
/// * **Symlinks** (`120000`) carry their target bytes as the hunk body —
///   a symlink's git blob is exactly its raw target. Every symlink change
///   (add/delete/edit/rename) is rendered by `render_symlink_change`
///   straight from `change.symlink`, never through `change.lines` (which a
///   non-UTF-8 target cannot populate) and never as a binary marker (which
///   `git apply` rejects for a `120000` entry), so a non-UTF-8 link target
///   round-trips byte-for-byte.
///
/// Every rendered file also opens with a `diff --git a/<p> b/<p>` line,
/// including a plain content modify (which carries no extended-mode
/// block). The header is what delimits one file's stanza from the next:
/// a bare `--- a/<path>` is ambiguous, and git binds it to the preceding
/// `diff --git` stanza when one is still open (a header-only empty-add or
/// mode-only change just above), misattributing this file's hunk. The
/// explicit per-file header makes ordering irrelevant.
///
/// An empty-file add/delete is still a real patch: git emits the
/// extended header with no hunk body (and `git apply` creates/unlinks
/// the path from that alone), so we emit the header-only form rather
/// than skipping the change. A modify with no hunk body is only skipped
/// when it is a genuine no-op — a trailing-newline-only change carries a
/// synthesized tail hunk (see `unified_hunks`) and is rendered, and a
/// mode-only modify (chmod) emits a header-only `diff --git` +
/// `old mode`/`new mode` block so the permission change round-trips.
pub(crate) fn render_diff_patch_bytes(output: &DiffOutput) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    for change in &output.changes {
        // A symlink change carries its raw target bytes in `change.symlink`,
        // which on Unix need not be valid UTF-8. Render it byte-exact so a
        // non-UTF-8 link target round-trips through `git apply`; every other
        // change is UTF-8 text and is appended as its bytes.
        if change.symlink.is_some() {
            render_symlink_change(change, &mut buf);
        } else {
            let mut text = String::new();
            render_text_change(change, &mut text);
            buf.extend_from_slice(text.as_bytes());
        }
    }
    buf
}

/// Lossy String view of the byte-exact patch (`render_diff_patch_bytes`),
/// for the JSON `.patch` field and String-based callers/tests. Only a
/// non-UTF-8 symlink target (Unix-only) differs from the byte render; JSON
/// strings cannot carry raw bytes, so a lossy view is the best a String
/// surface can do. The round-trip surface (`heddle diff --patch`) writes the
/// bytes directly via `render_diff_patch_bytes`, so its byte fidelity is
/// never reduced here.
pub(crate) fn render_diff_patch(output: &DiffOutput) -> String {
    String::from_utf8_lossy(&render_diff_patch_bytes(output)).into_owned()
}

/// Render one non-symlink change as unified-diff text into `buf`. Symlink
/// changes never reach here — `render_diff_patch_bytes` routes them to
/// `render_symlink_change`, which preserves a non-UTF-8 target — so a symlink
/// target is never forced through `change.lines` (which a non-UTF-8 target
/// cannot populate) or `render_binary_change`.
fn render_text_change(change: &FileChange, buf: &mut String) {
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
        render_binary_change(change, is_added, is_deleted, mode_changed, buf);
        return;
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
        return;
    }

    if is_rename {
        buf.push_str(&format!(
            "diff --git {} {}\n",
            quote_path_for_patch("a/", old_path),
            quote_path_for_patch("b/", &change.path)
        ));
        // A rename paired with a chmod/type change (`old.sh` renamed
        // to `new.sh` and made executable) carries both modes; emit
        // the `old mode`/`new mode` pair before `similarity index`,
        // matching `git diff`, so `git apply` reproduces the
        // permission change as well as the move.
        if let (Some(old), Some(new)) = (change.old_mode, change.mode)
            && old != new
        {
            buf.push_str(&format!("old mode {}\n", mode_str(change.old_mode)));
            buf.push_str(&format!("new mode {}\n", mode_str(change.mode)));
        }
        let pct = (change.similarity_score.unwrap_or(1.0).clamp(0.0, 1.0) * 100.0).round() as u32;
        buf.push_str(&format!("similarity index {pct}%\n"));
        buf.push_str(&format!(
            "rename from {}\n",
            quote_path_for_patch("", old_path)
        ));
        buf.push_str(&format!(
            "rename to {}\n",
            quote_path_for_patch("", &change.path)
        ));
        // Pure rename — extended headers alone suffice; emitting
        // `--- a/old / +++ b/new` without hunks would tell git to
        // apply an empty patch and warn about a stray header.
        if !has_hunk_body {
            return;
        }
    } else if is_added {
        buf.push_str(&format!(
            "diff --git {} {}\n",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        ));
        buf.push_str(&format!("new file mode {}\n", mode_str(change.mode)));
    } else if is_deleted {
        buf.push_str(&format!(
            "diff --git {} {}\n",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        ));
        buf.push_str(&format!("deleted file mode {}\n", mode_str(change.mode)));
    } else if mode_changed {
        // A modify whose mode changed (with or without a content
        // hunk). Emit the `diff --git` + `old mode`/`new mode`
        // header pair.
        buf.push_str(&format!(
            "diff --git {} {}\n",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        ));
        buf.push_str(&format!("old mode {}\n", mode_str(change.old_mode)));
        buf.push_str(&format!("new mode {}\n", mode_str(change.mode)));
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
        buf.push_str(&format!(
            "diff --git {} {}\n",
            quote_path_for_patch("a/", &change.path),
            quote_path_for_patch("b/", &change.path)
        ));
    }

    // An empty-file add/delete (text side present but zero lines)
    // has no hunk body. git stops after the `new/deleted file mode`
    // header in that case and `git apply` still creates/unlinks the
    // path — emitting `--- /+++/@@` with no `@@` body would be a
    // malformed hunk, so we stop here too.
    if (is_added || is_deleted) && !has_hunk_body {
        return;
    }
    // A mode-only modify carries no content hunk: the `old mode`/
    // `new mode` header pair is the entire patch, so stop before the
    // `--- /+++` line-diff headers (which would be a malformed
    // empty hunk).
    if is_modified && !has_hunk_body {
        return;
    }

    if is_added {
        buf.push_str("--- /dev/null\n");
    } else {
        buf.push_str(&format!("--- {}\n", quote_path_for_patch("a/", old_path)));
    }
    if is_deleted {
        buf.push_str("+++ /dev/null\n");
    } else {
        buf.push_str(&format!(
            "+++ {}\n",
            quote_path_for_patch("b/", &change.path)
        ));
    }
    if let Some(lines) = lines_ref {
        render_patch_hunks(change, lines, buf);
    }
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
/// `render_text_change`'s (add/delete/rename), and the mode is always
/// `120000` so a rename never needs an `old mode`/`new mode` pair unless the
/// two sides genuinely differ.
fn render_symlink_change(change: &FileChange, buf: &mut Vec<u8>) {
    let Some(sym) = change.symlink.as_ref() else {
        return;
    };
    let push = |buf: &mut Vec<u8>, text: &str| buf.extend_from_slice(text.as_bytes());
    let old_path = change.old_path.as_deref().unwrap_or(&change.path);
    let is_rename = change
        .old_path
        .as_deref()
        .is_some_and(|old| old != change.path);
    let is_added = change.kind == "added";
    let is_deleted = change.kind == "deleted";

    if is_rename {
        push(
            buf,
            &format!(
                "diff --git {} {}\n",
                quote_path_for_patch("a/", old_path),
                quote_path_for_patch("b/", &change.path)
            ),
        );
        if let (Some(old), Some(new)) = (change.old_mode, change.mode)
            && old != new
        {
            push(buf, &format!("old mode {}\n", mode_str(change.old_mode)));
            push(buf, &format!("new mode {}\n", mode_str(change.mode)));
        }
        let pct = (change.similarity_score.unwrap_or(1.0).clamp(0.0, 1.0) * 100.0).round() as u32;
        push(buf, &format!("similarity index {pct}%\n"));
        push(
            buf,
            &format!("rename from {}\n", quote_path_for_patch("", old_path)),
        );
        push(
            buf,
            &format!("rename to {}\n", quote_path_for_patch("", &change.path)),
        );
        // Pure rename (identical target) — the extended headers alone carry
        // the move, exactly like a text rename with no hunk body.
        if sym.old == sym.new {
            return;
        }
        push(
            buf,
            &format!("--- {}\n", quote_path_for_patch("a/", old_path)),
        );
        push(
            buf,
            &format!("+++ {}\n", quote_path_for_patch("b/", &change.path)),
        );
    } else if is_added {
        push(
            buf,
            &format!(
                "diff --git {} {}\n",
                quote_path_for_patch("a/", &change.path),
                quote_path_for_patch("b/", &change.path)
            ),
        );
        push(buf, &format!("new file mode {}\n", mode_str(change.mode)));
        push(buf, "--- /dev/null\n");
        push(
            buf,
            &format!("+++ {}\n", quote_path_for_patch("b/", &change.path)),
        );
    } else if is_deleted {
        push(
            buf,
            &format!(
                "diff --git {} {}\n",
                quote_path_for_patch("a/", &change.path),
                quote_path_for_patch("b/", &change.path)
            ),
        );
        push(
            buf,
            &format!("deleted file mode {}\n", mode_str(change.mode)),
        );
        push(
            buf,
            &format!("--- {}\n", quote_path_for_patch("a/", &change.path)),
        );
        push(buf, "+++ /dev/null\n");
    } else {
        // A symlink target-edit. The mode is unchanged (`120000` → `120000`),
        // so no `old mode`/`new mode` block — just the file header. An
        // identical target would be a no-op and is never emitted by the diff
        // backends, but guard it so an accidental empty hunk can't form.
        if sym.old == sym.new {
            return;
        }
        push(
            buf,
            &format!(
                "diff --git {} {}\n",
                quote_path_for_patch("a/", &change.path),
                quote_path_for_patch("b/", &change.path)
            ),
        );
        push(
            buf,
            &format!("--- {}\n", quote_path_for_patch("a/", &change.path)),
        );
        push(
            buf,
            &format!("+++ {}\n", quote_path_for_patch("b/", &change.path)),
        );
    }

    render_symlink_hunk(sym.old.as_deref(), sym.new.as_deref(), buf);
}

/// Emit the unified-diff hunk for a symlink's target bytes. A symlink's git
/// blob has no trailing newline, so each side normally collapses to a single
/// line carrying the `\ No newline at end of file` marker; a target that
/// embeds a `\n` (pathological but representable) splits into multiple lines.
/// The `@@` header mirrors `unified_hunks`'s `@@ -s,c +s,c @@` shape (counts
/// always written, even `,1`), which `git apply` accepts.
fn render_symlink_hunk(old: Option<&[u8]>, new: Option<&[u8]>, buf: &mut Vec<u8>) {
    let old_lines = split_target_lines(old);
    let new_lines = split_target_lines(new);
    let old_count = old_lines.len();
    let new_count = new_lines.len();
    let old_start = if old_count == 0 { 0 } else { 1 };
    let new_start = if new_count == 0 { 0 } else { 1 };
    buf.extend_from_slice(
        format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n").as_bytes(),
    );
    let old_no_eol = !target_has_trailing_newline(old);
    let new_no_eol = !target_has_trailing_newline(new);
    for (idx, line) in old_lines.iter().enumerate() {
        buf.push(b'-');
        buf.extend_from_slice(line);
        buf.push(b'\n');
        if old_no_eol && idx + 1 == old_count {
            buf.extend_from_slice(NO_NEWLINE_MARKER.as_bytes());
        }
    }
    for (idx, line) in new_lines.iter().enumerate() {
        buf.push(b'+');
        buf.extend_from_slice(line);
        buf.push(b'\n');
        if new_no_eol && idx + 1 == new_count {
            buf.extend_from_slice(NO_NEWLINE_MARKER.as_bytes());
        }
    }
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
fn render_binary_change(
    change: &FileChange,
    is_added: bool,
    is_deleted: bool,
    mode_changed: bool,
    buf: &mut String,
) {
    let path = &change.path;
    buf.push_str(&format!(
        "diff --git {} {}\n",
        quote_path_for_patch("a/", path),
        quote_path_for_patch("b/", path)
    ));
    if is_added {
        buf.push_str(&format!("new file mode {}\n", mode_str(change.mode)));
        buf.push_str("index 0000000..0000000\n");
    } else if is_deleted {
        buf.push_str(&format!("deleted file mode {}\n", mode_str(change.mode)));
        buf.push_str("index 0000000..0000000\n");
    } else if mode_changed {
        buf.push_str(&format!("old mode {}\n", mode_str(change.old_mode)));
        buf.push_str(&format!("new mode {}\n", mode_str(change.mode)));
        buf.push_str("index 0000000..0000000\n");
    } else {
        // Plain binary modify: git stamps the mode at the end of the
        // index line (`index <old>..<new> 100644`).
        buf.push_str(&format!(
            "index 0000000..0000000 {}\n",
            mode_str(change.mode)
        ));
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
    buf.push_str(&format!("Binary files {a} and {b} differ\n"));
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
    // Write the raw patch BYTES, not `output.patch` (a lossy String): a
    // symlink's target can be non-UTF-8, and the round-trip surface
    // (`heddle diff --patch | git apply`) must carry those bytes verbatim.
    // `output.patch` exists only to feed the JSON `.patch` field, where bytes
    // can't live; rendering bytes fresh here keeps stdout byte-exact.
    let rendered = render_diff_patch_bytes(output);
    let _ = std::io::stdout().write_all(&rendered);
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
            // Decoration trimming is a pretty-display-only nicety; the
            // canonical `change.lines` stays untrimmed for the
            // `--patch`/JSON path (cid 3320364905). Apply it here, for
            // human rendering, against a local copy.
            let lines = trim_added_decorations_for_display(lines);
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
    use objects::object::FileMode;

    use super::{
        SIGNATURE_CHANGE_SEPARATOR, aligned_added_tokens, group_semantic_changes, paint_line,
        paint_signature_change_item_lines, quote_path_for_patch, render_diff_patch,
        render_diff_patch_bytes, signature_change_display_segments,
    };
    use crate::cli::commands::diff::diff_types::{
        DiffOutput, FileChange, FileEolState, LineDiff, SemanticChangeEntry, change_line_counts,
        should_render_modified_pair,
    };

    fn modified_change_with_eol(path: &str, lines: Vec<LineDiff>, eol: FileEolState) -> FileChange {
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

    #[cfg(unix)]
    fn hermetic_git_command(dir: &std::path::Path, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        command
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Heddle Test")
            .env("GIT_AUTHOR_EMAIL", "heddle@example.com")
            .env("GIT_COMMITTER_NAME", "Heddle Test")
            .env("GIT_COMMITTER_EMAIL", "heddle@example.com");
        command
    }

    #[cfg(unix)]
    fn hermetic_git(dir: &std::path::Path, args: &[&str]) {
        let status = hermetic_git_command(dir, args)
            .status()
            .unwrap_or_else(|err| panic!("git {args:?} should spawn: {err}"));
        assert!(status.success(), "git {args:?} should succeed");
    }

    #[cfg(unix)]
    fn pipe_git_apply(dir: &std::path::Path, args: &[&str], patch: &[u8]) -> std::process::Output {
        use std::{io::Write, process::Stdio};

        let mut child = hermetic_git_command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("git {args:?} should spawn: {err}"));
        child.stdin.as_mut().unwrap().write_all(patch).unwrap();
        child
            .wait_with_output()
            .unwrap_or_else(|err| panic!("git {args:?} should finish: {err}"))
    }

    #[cfg(unix)]
    #[test]
    fn render_diff_patch_bytes_applies_non_utf8_symlink_target_byte_exactly() {
        use std::os::unix::ffi::OsStrExt;

        use crate::cli::commands::diff::diff_types::SymlinkChange;

        let target = b"target-\xff\xfe";
        let change = FileChange {
            path: "linky".to_string(),
            kind: "added".to_string(),
            mode: Some(FileMode::Symlink),
            symlink: Some(SymlinkChange {
                old: None,
                new: Some(target.to_vec()),
            }),
            ..Default::default()
        };
        let patch = render_diff_patch_bytes(&diff_output_with(vec![change]));
        assert!(
            patch.windows(target.len()).any(|window| window == target),
            "patch must carry the raw non-UTF-8 target bytes:\n{}",
            String::from_utf8_lossy(&patch)
        );

        let scratch = tempfile::TempDir::new().unwrap();
        hermetic_git(scratch.path(), &["init", "-q"]);
        hermetic_git(scratch.path(), &["checkout", "-q", "-b", "main"]);

        let check = pipe_git_apply(scratch.path(), &["apply", "--check"], &patch);
        assert!(
            check.status.success(),
            "git apply --check rejected patch;\nstderr={}\npatch=\n{}",
            String::from_utf8_lossy(&check.stderr),
            String::from_utf8_lossy(&patch)
        );
        let applied = pipe_git_apply(scratch.path(), &["apply"], &patch);
        assert!(
            applied.status.success(),
            "git apply rejected patch;\nstderr={}\npatch=\n{}",
            String::from_utf8_lossy(&applied.stderr),
            String::from_utf8_lossy(&patch)
        );

        let applied_target = std::fs::read_link(scratch.path().join("linky")).unwrap();
        assert_eq!(
            applied_target.as_os_str().as_bytes(),
            target,
            "applied symlink target must be byte-exact"
        );
    }

    /// A mode-only modify (exec-bit flip, no content change) must render
    /// as a header-only `diff --git` + `old mode`/`new mode` block with
    /// no `@@` hunk. Regressing this drops the chmod from the patch so
    /// `git apply` can't reproduce the permission change (cid 3318629228).
    #[test]
    fn render_diff_patch_emits_mode_only_header_for_chmod() {
        let change = FileChange {
            path: "run.sh".to_string(),
            kind: "modified".to_string(),
            lines: Some(Vec::new()),
            old_mode: Some(FileMode::Normal),
            mode: Some(FileMode::Executable),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.contains("diff --git a/run.sh b/run.sh"),
            "chmod-only must emit the `diff --git` header:\n{rendered}"
        );
        assert!(
            rendered.contains("old mode 100644") && rendered.contains("new mode 100755"),
            "chmod-only must emit `old mode`/`new mode`:\n{rendered}"
        );
        assert!(
            !rendered.contains("@@") && !rendered.contains("--- a/"),
            "chmod-only is header-only — no hunk body:\n{rendered}"
        );
    }

    /// A modify that changes BOTH content and mode emits the mode-header
    /// pair AND the usual `--- /+++` line-diff body.
    #[test]
    fn render_diff_patch_emits_mode_headers_with_content_hunk() {
        let change = FileChange {
            path: "run.sh".to_string(),
            kind: "modified".to_string(),
            lines: Some(vec![
                LineDiff::with_lines("@", "@ -1,1 +1,1 @@", None, None),
                LineDiff::with_lines("-", "echo old", Some(1), None),
                LineDiff::with_lines("+", "echo new", None, Some(1)),
            ]),
            old_mode: Some(FileMode::Normal),
            mode: Some(FileMode::Executable),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.contains("old mode 100644") && rendered.contains("new mode 100755"),
            "content+mode change must still emit the mode headers:\n{rendered}"
        );
        assert!(
            rendered.contains("--- a/run.sh")
                && rendered.contains("+++ b/run.sh")
                && rendered.contains("+echo new"),
            "content+mode change must still emit the line-diff body:\n{rendered}"
        );
    }

    /// An unchanged mode on a modify with no hunk body is a genuine
    /// no-op and must emit nothing — guards against the mode branch
    /// firing when `old_mode == mode`.
    #[test]
    fn render_diff_patch_skips_modify_with_same_mode_and_no_body() {
        let change = FileChange {
            path: "run.sh".to_string(),
            kind: "modified".to_string(),
            lines: Some(Vec::new()),
            old_mode: Some(FileMode::Normal),
            mode: Some(FileMode::Normal),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.is_empty(),
            "no-op modify (same mode, no body) must emit nothing:\n{rendered}"
        );
    }

    /// A binary content modify (`binary: true`, `lines: None`) must emit
    /// git's `Binary files … differ` marker with a *placeholder* index
    /// line. Silently dropping it would let `git apply` "succeed" while
    /// the binary content stayed stale (cid 3319484747); the index line
    /// flips git into binary-patch mode so it refuses the whole patch
    /// instead of skipping the block.
    #[test]
    fn render_diff_patch_binary_modify_emits_marker_with_index() {
        let change = FileChange {
            path: "binary.bin".to_string(),
            kind: "modified".to_string(),
            binary: true,
            lines: None,
            mode: Some(FileMode::Normal),
            old_mode: Some(FileMode::Normal),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.contains("diff --git a/binary.bin b/binary.bin"),
            "binary modify must emit a diff header:\n{rendered}"
        );
        assert!(
            rendered.contains("index 0000000..0000000 100644"),
            "binary modify must emit a placeholder index line:\n{rendered}"
        );
        assert!(
            rendered.contains("Binary files a/binary.bin and b/binary.bin differ"),
            "binary modify must emit the binary marker:\n{rendered}"
        );
        assert!(
            !rendered.contains("--- a/binary.bin"),
            "binary modify must not emit a text hunk header:\n{rendered}"
        );
    }

    /// A binary modify whose mode *also* changed emits the
    /// `old mode`/`new mode` pair (so the chmod is recorded) followed by
    /// the placeholder index + binary marker — never a mode-only chmod
    /// patch that git apply would accept while leaving stale binary
    /// content (cid 3319484747).
    #[test]
    fn render_diff_patch_binary_modify_with_mode_change_keeps_marker() {
        let change = FileChange {
            path: "binary.bin".to_string(),
            kind: "modified".to_string(),
            binary: true,
            lines: None,
            old_mode: Some(FileMode::Normal),
            mode: Some(FileMode::Executable),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![change]));
        assert!(
            rendered.contains("old mode 100644") && rendered.contains("new mode 100755"),
            "binary+mode change must still record the chmod:\n{rendered}"
        );
        assert!(
            rendered.contains("index 0000000..0000000"),
            "binary+mode change must emit the placeholder index line:\n{rendered}"
        );
        assert!(
            rendered.contains("Binary files a/binary.bin and b/binary.bin differ"),
            "binary+mode change must still emit the binary marker:\n{rendered}"
        );
    }

    /// A binary add emits `new file mode` + placeholder index + marker;
    /// a binary delete mirrors it with `deleted file mode`.
    #[test]
    fn render_diff_patch_binary_add_and_delete_emit_markers() {
        let added = FileChange {
            path: "added.bin".to_string(),
            kind: "added".to_string(),
            binary: true,
            lines: None,
            mode: Some(FileMode::Normal),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![added]));
        assert!(
            rendered.contains("new file mode 100644")
                && rendered.contains("index 0000000..0000000")
                && rendered.contains("Binary files /dev/null and b/added.bin differ"),
            "binary add marker:\n{rendered}"
        );

        let deleted = FileChange {
            path: "gone.bin".to_string(),
            kind: "deleted".to_string(),
            binary: true,
            lines: None,
            mode: Some(FileMode::Normal),
            ..Default::default()
        };
        let rendered = render_diff_patch(&diff_output_with(vec![deleted]));
        assert!(
            rendered.contains("deleted file mode 100644")
                && rendered.contains("index 0000000..0000000")
                && rendered.contains("Binary files a/gone.bin and /dev/null differ"),
            "binary delete marker:\n{rendered}"
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

    /// Pin git's C-style path quoting byte-for-byte. The conformance
    /// harness round-trips the common classes through real `git apply`;
    /// this covers the exact escape spellings (including the `\a \b \v \f
    /// \r` controls and octal fallback) the integration cells don't reach.
    #[test]
    fn quote_path_matches_git_c_style() {
        // Simple paths — and spaces, which git leaves bare — emit unquoted.
        assert_eq!(quote_path_for_patch("a/", "src/main.rs"), "a/src/main.rs");
        assert_eq!(
            quote_path_for_patch("a/", "with space.txt"),
            "a/with space.txt"
        );
        // Tab/newline/quote/backslash force quoting; the prefix is escaped
        // inside the quotes, matching git's `quote_two`.
        assert_eq!(quote_path_for_patch("a/", "tab\there"), "\"a/tab\\there\"");
        assert_eq!(
            quote_path_for_patch("b/", "line\nbreak"),
            "\"b/line\\nbreak\""
        );
        assert_eq!(quote_path_for_patch("a/", "quo\"te"), "\"a/quo\\\"te\"");
        assert_eq!(
            quote_path_for_patch("a/", "back\\slash"),
            "\"a/back\\\\slash\""
        );
        // Non-ASCII (UTF-8 é = 0xC3 0xA9) → per-byte octal.
        assert_eq!(quote_path_for_patch("a/", "café"), "\"a/caf\\303\\251\"");
        // `rename from`/`rename to` quote the bare path (empty prefix).
        assert_eq!(quote_path_for_patch("", "x\ty"), "\"x\\ty\"");
        // The remaining named C-escapes plus a low control byte (octal).
        assert_eq!(
            quote_path_for_patch("", "\u{07}\u{08}\u{0b}\u{0c}\r\u{01}"),
            "\"\\a\\b\\v\\f\\r\\001\""
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
