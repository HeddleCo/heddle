// SPDX-License-Identifier: Apache-2.0
//! Preflight checks before running the merge algorithm.

const BINARY_SNIFF_LEN: usize = 8192;

pub(super) fn any_binary(base: &[u8], ours: &[u8], theirs: &[u8]) -> bool {
    is_binary(base) || is_binary(ours) || is_binary(theirs)
}

fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(BINARY_SNIFF_LEN)];
    head.contains(&0)
}
