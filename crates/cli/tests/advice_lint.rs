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
