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
