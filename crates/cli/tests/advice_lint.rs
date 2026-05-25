// SPDX-License-Identifier: Apache-2.0
//! Advice/error-envelope discipline lint.
//!
//! Error envelopes are a machine contract, not ad hoc command output.
//! This test keeps stderr JSON envelopes and human `Error:` / `Hint:`
//! labels centralized in `commands/error_envelope.rs`.

use std::{
    fs,
    path::{Path, PathBuf},
};

const ALLOWED_ENVELOPE_FILE: &str = "cli/commands/error_envelope.rs";
const ALLOWED_ADVICE_FILE: &str = "cli/commands/advice.rs";
const ALLOWED_HISTORY_TARGET_FILE: &str = "cli/commands/history_target.rs";
const ALLOWED_NEXT_ACTION_FILE: &str = "cli/commands/next_action.rs";

const RAW_RECOVERY_PHRASES: &[&str] = &[
    "State not found:",
    "network fetch support is not available",
    "network push support is not available",
    "network pull support is not available",
    "invalid Git remote name for Git-overlay repository",
    "repository has no HEAD; capture a state first",
    "Repository has no HEAD state - take a snapshot first",
    "Use one path.",
    "--principal-name is required",
    "--principal-email is required",
    "--annotation-kind is required for into-annotation",
    "--annotation-content is required for into-annotation",
    "--reason is required for dismiss",
    "--symbols expects 'file:symbol'",
    "has no recorded parent; pass --into",
];

const RAW_THREAD_NOT_FOUND_PHRASES: &[&str] = &[
    "Thread not found: {thread}",
    "Thread not found: {}",
    "Thread '{}' not found",
    "Target thread '{}' not found",
    "Thread '{}' not found after capture",
    "Thread '{}' not found after refresh",
];

#[test]
fn error_envelopes_stay_centralized() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    walk_rust_files(&src_dir, &mut |path| {
        let rel = path.strip_prefix(&src_dir).unwrap_or(path);
        if rel == Path::new(ALLOWED_ENVELOPE_FILE) {
            return;
        }
        let Ok(source) = fs::read_to_string(path) else {
            return;
        };
        for (line_index, line) in source.lines().enumerate() {
            if line.contains("eprintln!(\"Error:")
                || line.contains("eprintln!(\"Hint:")
                || line.contains("\"code\": \"parse_error\"")
                || line.contains("\"kind\": \"parse_error\"")
                || line.contains("\"code\": kind")
                || line.contains("\"kind\": kind")
            {
                violations.push(format!("{}:{}", rel.display(), line_index + 1));
            }
        }
    });

    assert!(
        violations.is_empty(),
        "error envelope rendering must stay centralized in {ALLOWED_ENVELOPE_FILE}; violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn known_recovery_phrases_stay_in_typed_advice() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    walk_rust_files(&src_dir, &mut |path| {
        let rel = path.strip_prefix(&src_dir).unwrap_or(path);
        let Ok(source) = fs::read_to_string(path) else {
            return;
        };
        for (line_index, line) in source.lines().enumerate() {
            for phrase in RAW_RECOVERY_PHRASES {
                if recovery_phrase_allowed(rel, phrase) {
                    continue;
                }
                if line.contains(phrase) {
                    violations.push(format!(
                        "{}:{} contains raw recovery phrase `{phrase}`",
                        rel.display(),
                        line_index + 1
                    ));
                }
            }
            for phrase in RAW_THREAD_NOT_FOUND_PHRASES {
                if recovery_phrase_allowed(rel, phrase) {
                    continue;
                }
                if line.contains(phrase) {
                    violations.push(format!(
                        "{}:{} contains raw missing-thread phrase `{phrase}`",
                        rel.display(),
                        line_index + 1
                    ));
                }
            }
        }
    });

    assert!(
        violations.is_empty(),
        "recovery phrases must be emitted through typed RecoveryAdvice constructors; violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn git_bridge_recovery_policy_stays_out_of_error_renderer() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let envelope = src_dir.join(ALLOWED_ENVELOPE_FILE);
    let source = fs::read_to_string(&envelope)
        .unwrap_or_else(|err| panic!("read {}: {err}", envelope.display()));

    assert!(
        source.contains("RecoveryAdvice::from_git_bridge_error"),
        "{ALLOWED_ENVELOPE_FILE} should delegate GitBridgeError policy to typed advice"
    );
    for forbidden in [
        "NonFastForwardRef",
        "GitHeddleThreadDiverged",
        "RemoteDiverged",
        "ShallowClone",
        "refs/notes/heddle",
        "git_overlay_remote_diverged",
        "git_overlay_mapping_conflict",
        "git_overlay_shallow_clone",
    ] {
        assert!(
            !source.contains(forbidden),
            "{ALLOWED_ENVELOPE_FILE} should render Git bridge recovery advice, not own policy `{forbidden}`"
        );
    }
}

#[test]
fn remote_recovery_policy_uses_typed_advice_constructors() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for (file, forbidden) in [
        (
            "cli/commands/remote/mod.rs",
            &[
                "remote_transport_mismatch_advice",
                "remote_not_configured_advice",
                "git_tracking_refresh_failed_advice",
                "network_push_failed_advice",
            ][..],
        ),
        (
            "cli/commands/remote/remote_ops.rs",
            &[
                "local_lazy_pull_advice",
                "network_pull_failed_advice",
                "remote_not_found_advice",
            ][..],
        ),
        (
            "cli/commands/fetch.rs",
            &["fetch_remote_required_advice"][..],
        ),
        (
            "cli/commands/clone.rs",
            &["network_clone_failed_advice"][..],
        ),
    ] {
        let path = src_dir.join(file);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for symbol in forbidden {
            if source.contains(symbol) {
                violations.push(format!("{file} contains `{symbol}`"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "remote recovery policy should live on RecoveryAdvice constructors:\n{}",
        violations.join("\n")
    );
}

#[test]
fn verification_blocked_outputs_use_shared_action_policy() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for file in [
        "cli/commands/operator_loop.rs",
        "cli/commands/thread_shaping.rs",
        "cli/commands/merge/mod.rs",
        "cli/commands/ready_cmd.rs",
        "cli/commands/workflow.rs",
        "cli/commands/rebase/mod.rs",
    ] {
        let path = src_dir.join(file);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        if source.contains("trust.recommended_action.is_empty()") {
            violations.push(format!(
                "{file} reimplements repository verification recommended-action fallback"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "repository-verification blocked outputs should use repository_verification_primary_command or OperatorCommandOutput::blocked_by_repository_verification:\n{}",
        violations.join("\n")
    );
}

#[test]
fn next_action_priority_lives_in_shared_selector() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    walk_rust_files(&src_dir, &mut |path| {
        let rel = path.strip_prefix(&src_dir).unwrap_or(path);
        if rel == Path::new(ALLOWED_NEXT_ACTION_FILE) {
            return;
        }
        let Ok(source) = fs::read_to_string(path) else {
            return;
        };
        for (line_index, line) in source.lines().enumerate() {
            for fragment in [
                "remote_tracking.behind > 0",
                "heddle bridge git import --ref {}",
                "thread_action.filter(|action| !action.trim().is_empty())",
            ] {
                if line.contains(fragment) {
                    violations.push(format!(
                        "{}:{} reimplements next-action priority fragment `{fragment}`",
                        rel.display(),
                        line_index + 1
                    ));
                }
            }
        }
    });

    assert!(
        violations.is_empty(),
        "next-action priority should be selected through {ALLOWED_NEXT_ACTION_FILE}; violations:\n{}",
        violations.join("\n")
    );
}

fn recovery_phrase_allowed(rel: &Path, phrase: &str) -> bool {
    rel == Path::new(ALLOWED_ADVICE_FILE)
        || rel == Path::new(ALLOWED_ENVELOPE_FILE)
        || (phrase == "State not found:" && rel == Path::new(ALLOWED_HISTORY_TARGET_FILE))
}

fn walk_rust_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk_rust_files(&path, visit);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}
