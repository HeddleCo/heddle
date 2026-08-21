// SPDX-License-Identifier: Apache-2.0
//! Incremental UTF-8 line scan matching `str::lines()` without owning strings.

#[derive(Clone, Copy, Debug)]
pub(super) struct LineOff {
    pub start: usize,
    pub len: usize,
}

/// Count `str::lines()` records without allocating.
pub(super) fn count_text_lines(bytes: &[u8]) -> Result<usize, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    Ok(text.lines().count())
}

/// Fill `out` with byte ranges for each `str::lines()` record.
///
/// Line bytes exclude the terminator. A trailing `\r` before `\n` is stripped,
/// matching [`str::lines`].
pub(super) fn fill_line_offsets(bytes: &[u8], out: &mut [LineOff]) -> Result<usize, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut count = 0usize;
    let mut byte_start = 0usize;
    for (idx, line) in text.lines().enumerate() {
        if idx >= out.len() {
            return Err(());
        }
        let start = byte_start;
        let len = line.len();
        out[idx] = LineOff { start, len };
        byte_start = next_line_start(bytes, start + len);
        count += 1;
    }
    Ok(count)
}

fn next_line_start(bytes: &[u8], after_content: usize) -> usize {
    let mut pos = after_content;
    if pos < bytes.len() && bytes[pos] == b'\r' {
        pos += 1;
    }
    if pos < bytes.len() && bytes[pos] == b'\n' {
        pos += 1;
    }
    pos
}

pub(super) fn line_bytes(source: &[u8], off: LineOff) -> &[u8] {
    &source[off.start..off.start + off.len]
}
