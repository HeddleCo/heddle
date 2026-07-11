// SPDX-License-Identifier: Apache-2.0
//! Pure clean message assembly (no FS deletion I/O).
//!
//! Owns human summary strings for `heddle clean` that can be decided from
//! removed-path facts alone. Directory emptiness checks and recursive delete
//! stay CLI-owned.

/// Empty-result human line (`removed_count == 0`).
pub fn clean_empty_message(dry_run: bool) -> &'static str {
    if dry_run {
        "Would remove: nothing to clean"
    } else {
        "Nothing to clean"
    }
}

/// Header when there is at least one path to list.
pub fn clean_paths_header(dry_run: bool) -> &'static str {
    if dry_run { "Would remove:" } else { "Removed:" }
}

/// One indented path line under the clean result header.
pub fn clean_path_line(path: &str) -> String {
    format!("  {path}")
}

/// Human text lines for clean output from removed paths + dry-run flag.
///
/// Empty `removed` yields a single summary line; otherwise a header plus one
/// indented path line per entry (order preserved).
pub fn clean_result_lines(removed: &[String], dry_run: bool) -> Vec<String> {
    if removed.is_empty() {
        return vec![clean_empty_message(dry_run).to_string()];
    }
    let mut lines = Vec::with_capacity(removed.len() + 1);
    lines.push(clean_paths_header(dry_run).to_string());
    for path in removed {
        lines.push(clean_path_line(path));
    }
    lines
}

/// Single multi-line string form of [`clean_result_lines`].
pub fn clean_result_text(removed: &[String], dry_run: bool) -> String {
    clean_result_lines(removed, dry_run).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_messages() {
        assert_eq!(clean_empty_message(true), "Would remove: nothing to clean");
        assert_eq!(clean_empty_message(false), "Nothing to clean");
        assert_eq!(
            clean_result_lines(&[], true),
            vec!["Would remove: nothing to clean".to_string()]
        );
        assert_eq!(
            clean_result_lines(&[], false),
            vec!["Nothing to clean".to_string()]
        );
    }

    #[test]
    fn dry_run_and_removed_lists() {
        let paths = vec!["a.txt".into(), "dir/b".into()];
        let dry = clean_result_lines(&paths, true);
        assert_eq!(dry[0], "Would remove:");
        assert_eq!(dry[1], "  a.txt");
        assert_eq!(dry[2], "  dir/b");

        let done = clean_result_text(&paths, false);
        assert!(done.starts_with("Removed:\n"));
        assert!(done.contains("  a.txt"));
        assert!(done.contains("  dir/b"));
        assert_eq!(clean_paths_header(false), "Removed:");
        assert_eq!(clean_path_line("x"), "  x");
    }
}
