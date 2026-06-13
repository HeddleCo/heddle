// SPDX-License-Identifier: Apache-2.0
//! Every `Commands` variant must have an explicit root command contract.
//!
//! Help tiers now come from the command contract table, so this test
//! enumerates every variant of `Commands` from `commands_main.rs` and
//! asserts the root verb name is present in that table. The runtime
//! `command_help_tier` lookup still has an advanced fallback for
//! unknown wrappers, but existing verbs must never rely on it.
//!
//! The check is text-based for the same reason
//! [`op_id_coverage`](super::op_id_coverage) is: avoids dragging
//! `syn` into the dev-deps and stays readable when something fails.

use std::{collections::BTreeSet, path::PathBuf};

use cli::cli::commands::{command_contract_removed_alias_root, command_contract_root_commands};

#[test]
fn every_commands_variant_has_explicit_root_contract() {
    let commands_rs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cli")
        .join("cli_args")
        .join("commands_main.rs");
    let source = std::fs::read_to_string(&commands_rs)
        .unwrap_or_else(|e| panic!("read {}: {e}", commands_rs.display()));

    let variants = enumerate_commands_variants(&source);
    assert!(
        !variants.is_empty(),
        "no Commands variants found in {} — has the enum shape changed?",
        commands_rs.display()
    );

    let contracted = command_contract_root_commands()
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    let mut missing = Vec::new();
    for variant in &variants {
        if cfg!(not(feature = "client"))
            && matches!(variant.as_str(), "Auth" | "Support" | "Presence")
        {
            continue;
        }
        let kebab = variant_to_verb(variant);
        if command_contract_removed_alias_root(&kebab) {
            continue;
        }
        if !contracted.contains(&kebab) {
            missing.push(format!("{variant} (verb name `{kebab}`)"));
        }
    }

    assert!(
        missing.is_empty(),
        "the following Commands variants don't have an explicit root \
         entry in the command contract table and would fall through to \
         runtime help/contract defaults:\n  {}\n\n\
         Add each root verb to CONTRACTS in \
         crates/cli/src/cli/commands/command_catalog.rs.",
        missing.join("\n  ")
    );
}

/// Walk the `pub enum Commands { ... }` block and collect every
/// top-level variant identifier. Variants are PascalCase Rust
/// identifiers; clap's auto-derive lowers them to kebab-case for the
/// CLI surface, which `variant_to_verb` mirrors.
fn enumerate_commands_variants(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let marker = "pub enum Commands";
    let start = source
        .find(marker)
        .expect("commands_main.rs must contain `pub enum Commands`");
    let brace_open = start
        + marker.len()
        + source[start + marker.len()..]
            .find('{')
            .expect("Commands enum must have an opening brace");
    let brace_close = match_close_brace(bytes, brace_open).expect("balanced enum braces");

    let mut variants = Vec::new();
    let mut i = brace_open + 1;
    while i < brace_close {
        // Skip whitespace and `#[...]` attributes. Attributes nest
        // arbitrarily but always start at a `#` at the line/expr top
        // level — `match_close_bracket` skips through their bodies.
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            // Line comment.
            while i < brace_close && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'#' && bytes.get(i + 1) == Some(&b'[') {
            // Skip the entire `#[...]` attribute, including line
            // breaks and nested brackets (rare but allowed).
            let attr_end = match_close_bracket(bytes, i + 1).unwrap_or(brace_close);
            i = attr_end + 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let name_start = i;
            while i < brace_close && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = std::str::from_utf8(&bytes[name_start..i]).unwrap_or("");
            // The first identifier on a non-attribute non-comment line
            // is the variant name. `pub` and `enum` aren't reachable
            // here because we entered the brace block. Skip Rust
            // keywords that can show up inside a tuple-variant payload
            // (e.g., `Box<str>`) — we only count identifiers when at
            // depth 0 within the enum body, which we track below.
            if !name.is_empty()
                && name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false)
                && depth_at(bytes, brace_open + 1, name_start) == 0
            {
                variants.push(name.to_string());
            }
            // Skip past whatever payload follows: `{...}`, `(...)`, or
            // nothing at all (unit variant). The next `,` or `}` ends
            // this variant.
            i = skip_to_variant_terminator(bytes, i, brace_close);
            continue;
        }
        i += 1;
    }
    variants
}

/// Track brace/paren depth between `start` and `pos`, ignoring strings
/// and chars. Returns the depth at `pos`. Used to decide whether a
/// PascalCase identifier we found is a top-level enum variant or a
/// type name nested inside a payload.
fn depth_at(bytes: &[u8], start: usize, pos: usize) -> i32 {
    let mut depth: i32 = 0;
    let mut i = start;
    while i < pos {
        match bytes[i] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth
}

fn skip_to_variant_terminator(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut depth: i32 = 0;
    let mut i = start;
    while i < end {
        match bytes[i] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b',' if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    end
}

fn match_close_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn match_close_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Convert a PascalCase variant name (`HarnessBridge`, `CherryPick`)
/// to the kebab-case verb name clap derives by default. Mirrors clap's
/// `rename_all = "kebab-case"` behaviour: insert `-` before every
/// non-leading uppercase letter, lowercase everything.
fn variant_to_verb(variant: &str) -> String {
    let mut out = String::with_capacity(variant.len() + 2);
    for (i, c) in variant.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
