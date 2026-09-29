// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path};

fn scan_sources(dir: &Path, violations: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read crate source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            scan_sources(&path, violations);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let publication_module = path.ends_with("git_core.rs");
        let source = fs::read_to_string(&path).expect("read Rust source");
        let mut test_item_pending = false;
        let mut test_item_depth = 0_usize;
        let lines: Vec<_> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            // Test fixture writes are permitted. Resume scanning after each
            // test item, including when production items follow it.
            if test_item_depth > 0 {
                test_item_depth += line.matches('{').count();
                test_item_depth -= line.matches('}').count();
                continue;
            }
            if line.trim() == "#[cfg(test)]" {
                test_item_pending = true;
                continue;
            }
            if test_item_pending {
                if line.trim().starts_with("#[") {
                    continue;
                }
                test_item_depth = line.matches('{').count() - line.matches('}').count();
                test_item_pending = false;
                continue;
            }
            let raw_sley = [
                ".update_to(",
                ".delete_ref(",
                ".delete_symbolic_ref(",
                ".update_ref(",
                ".reference_matching(",
                ".reference_symbolic(",
                ".set_target(",
                ".set_target_id(",
                ".edit_reference(",
                "apply_ref_changes(",
                "apply_ref_batch(",
                "update_to(",
                "refs.transaction(",
                "store.transaction(",
                "references.transaction(",
                ".references().transaction(",
                "gix::refs::transaction",
                "sley_refs::transaction",
                "upsert_note_bytes_for(",
                "tx.update(",
                "create_branch(",
                "update_branch_checked_out_as_head(",
                "write_notes(",
            ]
            .iter()
            .any(|needle| line.contains(needle));
            let raw_git = line.contains("\"update-ref\"");
            let raw_reference = line.contains(".reference(") && !line.contains(".reference()");
            let dynamic_refspec = line
                .split_once("format!(\"+")
                .and_then(|(_, tail)| tail.split('"').next())
                .is_some_and(|format_string| format_string.contains(':'))
                || line.contains(".join(\":\")");
            let empty_fetch = line.contains("fetch(")
                && lines[index..lines.len().min(index + 5)].iter().any(|next| {
                    next.contains("&[]") || next.contains("vec![]") || next.contains("Vec::new()")
                });
            // Fetch refspecs are writes too. A forced transport update must
            // never target the branch or notes namespaces protected by the
            // publication guard. Staging and remote-tracking refs are safe.
            let protected_fetch = line.contains(":refs/heads/") || line.contains(":refs/notes/");
            if protected_fetch
                || empty_fetch
                || (!publication_module
                    && (raw_sley || raw_git || raw_reference || dynamic_refspec))
            {
                violations.push(format!("{}:{}: {line}", path.display(), index + 1));
            }
        }
    }
}

#[test]
fn git_ref_writes_stay_in_guarded_publication_module() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let mut violations = Vec::new();
    for entry in fs::read_dir(workspace.join("crates")).expect("read workspace crates") {
        let entry = entry.expect("read workspace crate");
        let source = entry.path().join("src");
        if source.is_dir() {
            scan_sources(&source, &mut violations);
        }
    }
    assert!(
        violations.is_empty(),
        "unguarded Git ref writes or protected fetch destinations:\n{}",
        violations.join("\n")
    );
}
