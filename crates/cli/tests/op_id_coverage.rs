// SPDX-License-Identifier: Apache-2.0
//! Op-id validation must be driven once from the command contract before
//! dispatch reaches individual command arms.
//!
//! The dedup contract is wire-only without it: an agent passing `--op-id`
//! needs replay/reservation before dispatch and a final format check in
//! the child process before any command body starts work. CI fails this
//! test if validation drifts back into per-arm calls.
//!
//! The check is intentionally text-based (no `syn` dep): the dispatch
//! match is parsed by a small balanced-brace scanner and grep-asserted
//! for the canonical helper name.

use std::path::PathBuf;

use cli::{
    cli::commands::{
        build_command_catalog, command_persists_op_id, command_uses_bootstrap_op_id_store,
        observe_only_root_commands,
    },
    operation_id::supports_local_op_id,
};

#[test]
fn op_id_validation_is_centralized_before_dispatch() {
    let main_rs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("main.rs");
    let source = std::fs::read_to_string(&main_rs)
        .unwrap_or_else(|e| panic!("read {}: {e}", main_rs.display()));

    let arms = extract_command_arms(&source);
    assert!(
        !arms.is_empty(),
        "no Commands::<Variant> arms found in {} — has the dispatch shape changed?",
        main_rs.display()
    );

    let resolver_call = "resolve_operation_id(&cli)?";
    let resolver_count = source.matches(resolver_call).count();
    assert_eq!(
        resolver_count, 1,
        "`{resolver_call}` must be called exactly once from the command-contract gate before dispatch"
    );

    let idempotency_gate = source
        .find("match run_local_idempotency_if_requested")
        .expect("main.rs must run the local op-id idempotency gate");
    let centralized_resolver = source
        .find(resolver_call)
        .expect("main.rs must centrally validate op-id before dispatch");
    let dispatch = source
        .find("let result = match &cli.command")
        .expect("main.rs must contain command dispatch");
    assert!(
        idempotency_gate < centralized_resolver && centralized_resolver < dispatch,
        "`{resolver_call}` must run after replay/reservation and before command dispatch"
    );

    let mut offenders = Vec::new();
    for arm in &arms {
        if arm.body.contains("resolve_operation_id(") {
            offenders.push(arm.variant.clone());
        }
    }
    assert!(
        offenders.is_empty(),
        "op-id validation must stay centralized; remove arm-level resolver calls from: {offenders:?}"
    );
}

#[test]
fn command_contract_table_drives_op_id_and_read_only_classification() {
    let catalog = build_command_catalog();
    let root_entries = catalog
        .commands
        .iter()
        .filter(|entry| entry.path.len() == 1)
        .collect::<Vec<_>>();
    assert!(
        !root_entries.is_empty(),
        "command catalog returned no root entries"
    );

    for entry in root_entries {
        let root = entry.path[0].as_str();
        assert_eq!(
            entry.supports_op_id,
            supports_local_op_id(root),
            "op-id runtime support for `{root}` must come from the command contract table"
        );
        assert_eq!(
            entry.persists_op_id,
            command_persists_op_id(root),
            "op-id persistence for `{root}` must come from the command contract table"
        );
        assert_eq!(
            entry.op_id_store_scope == "bootstrap",
            command_uses_bootstrap_op_id_store(root),
            "op-id store scope for `{root}` must come from the command contract table"
        );
        if entry.persists_op_id {
            assert!(
                entry.supports_op_id,
                "`{root}` cannot persist op-id state unless it supports op-id replay"
            );
        }
        if observe_only_root_commands().contains(&root) {
            assert!(
                entry.observe_only,
                "`{root}` is read-only in op-id coverage but not observe_only in command catalog"
            );
            assert!(
                !entry.mutates,
                "`{root}` is read-only in op-id coverage but mutates in command catalog"
            );
        }
    }

    for read_only in [
        "thread list",
        "thread show",
        "status",
        "bridge git status",
        "hook list",
        "remote list",
        "context get",
        "review show",
        "agent list",
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == read_only)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{read_only}`"));
        assert!(
            entry.observe_only,
            "`{read_only}` must be observe-only in the command contract table"
        );
        assert!(
            !entry.supports_op_id,
            "`{read_only}` must not reserve local op-id slots"
        );
        assert_eq!(
            entry.op_id_store_scope, "none",
            "`{read_only}` must not use an op-id store"
        );
        assert!(
            !supports_local_op_id(read_only),
            "runtime op-id support for `{read_only}` must come from the exact command contract"
        );
    }

    for mutating in [
        "init",
        "adopt",
        "clone",
        "thread switch",
        "thread drop",
        "bridge git export",
        "bridge git import",
        "context set",
        "review sign",
        "agent capture",
    ] {
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == mutating)
            .unwrap_or_else(|| panic!("missing command catalog entry for `{mutating}`"));
        assert!(
            entry.mutates,
            "`{mutating}` must be mutating in the command contract table"
        );
        assert!(
            supports_local_op_id(mutating),
            "runtime op-id support for `{mutating}` must come from the exact command contract"
        );
        let expected_scope = if entry.may_initialize {
            "bootstrap"
        } else {
            "repository"
        };
        assert_eq!(
            entry.op_id_store_scope, expected_scope,
            "`{mutating}` op-id store scope must come from the exact command contract"
        );
    }
}

#[derive(Debug)]
struct Arm {
    variant: String,
    body: String,
}

/// Extract every `Commands::<Variant>` arm from the dispatch match in
/// main.rs. Tolerates `cfg(...)` attributes between arms and arms whose
/// body is either an expression (`=> cmd_foo(...),`) or a block
/// (`=> { ... }`).
///
/// We scope to the dispatch match (`match &cli.command { ... }`) by
/// finding it explicitly — this avoids picking up the arms in the
/// `command_name` helper at the bottom of main.rs, which return string
/// literals and never call `resolve_operation_id`.
fn extract_command_arms(source: &str) -> Vec<Arm> {
    let bytes = source.as_bytes();
    let mut arms = Vec::new();

    // Locate `match &cli.command {`, then walk inside its braces.
    let dispatch_marker = "match &cli.command";
    let dispatch_start = source
        .find(dispatch_marker)
        .expect("main.rs must contain a `match &cli.command` dispatch");
    let dispatch_open_brace = dispatch_start
        + dispatch_marker.len()
        + source[dispatch_start + dispatch_marker.len()..]
            .find('{')
            .expect("dispatch match must have an opening brace");
    let dispatch_close = match_close_brace(bytes, dispatch_open_brace)
        .expect("dispatch match must have a balanced closing brace");
    let scope_end = dispatch_close;

    let needle = "Commands::";
    let mut cursor = dispatch_open_brace + 1;
    while cursor < scope_end {
        let Some(rel) = source[cursor..scope_end].find(needle) else {
            break;
        };
        let start = cursor + rel;
        cursor = start + needle.len();

        if cfg_attr_before_arm_is_disabled(source, start) {
            continue;
        }

        // Word-boundary check: skip when preceded by an identifier char
        // (e.g. `ContextCommands::`, `ActorCommands::`, `SessionCommands::`).
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }

        let variant = read_variant_name(&source[cursor..]);
        if variant.is_empty() {
            continue;
        }

        // Walk to the `=>` token — bail if we don't find one before the
        // next match arm or a closing brace at depth 0 from `cursor`.
        let after_variant = cursor + variant.len();
        let Some(arrow_off) = find_match_arrow(&source[after_variant..]) else {
            continue;
        };
        let arrow = after_variant + arrow_off;
        let body_start = arrow + 2;

        // Body is either a block `{ ... }` or an expression terminated by
        // a comma at depth 0 (or end of file).
        let body_text =
            if let Some(brace_off) = first_non_whitespace_is_brace(&source[body_start..]) {
                // Block arm — find matching close brace.
                let block_open = body_start + brace_off;
                let close = match_close_brace(bytes, block_open).unwrap_or(bytes.len());
                source[block_open..=close.min(bytes.len() - 1)].to_string()
            } else {
                // Expression arm — read until depth-0 comma.
                let end = expr_arm_end(&source[body_start..]);
                source[body_start..body_start + end].to_string()
            };

        arms.push(Arm {
            variant: variant.to_string(),
            body: body_text,
        });
    }
    arms
}

fn cfg_attr_before_arm_is_disabled(source: &str, arm_start: usize) -> bool {
    for line in source[..arm_start].lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("#[") {
            return false;
        }
        if trimmed.contains("cfg(feature = \"client\")") && !cfg!(feature = "client") {
            return true;
        }
        if trimmed.contains("cfg(feature = \"git-overlay\")") && !cfg!(feature = "git-overlay") {
            return true;
        }
        if trimmed.contains("cfg(feature = \"semantic\")") && !cfg!(feature = "semantic") {
            return true;
        }
    }
    false
}

fn read_variant_name(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Find the next `=>` that's clearly part of this arm's pattern. We
/// ignore `=>` that appears nested inside parens/braces (paranoid
/// defense; clap arms typically don't have them).
fn find_match_arrow(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth_paren: i32 = 0;
    let mut depth_brace: i32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'{' => depth_brace += 1,
            b'}' => {
                if depth_brace == 0 {
                    return None;
                }
                depth_brace -= 1;
            }
            b'=' if bytes[i + 1] == b'>' && depth_paren == 0 && depth_brace == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn first_non_whitespace_is_brace(s: &str) -> Option<usize> {
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            continue;
        }
        return if c == '{' { Some(i) } else { None };
    }
    None
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

fn expr_arm_end(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth_paren: i32 = 0;
    let mut depth_brace: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b',' if depth_paren == 0 && depth_brace == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}
