// SPDX-License-Identifier: Apache-2.0
//! Pure change-monitor human output assembly (no FS / serve I/O).
//!
//! Owns human lines for `heddle maintenance monitor` from report fields.
//! Monitor inspection, path collection, and local helper serve stay CLI-owned.

/// `Backend: {backend}`.
pub fn monitor_backend_line(backend: &str) -> String {
    format!("Backend: {backend}")
}

/// `Status: {status}`.
pub fn monitor_status_line(status: &str) -> String {
    format!("Status: {status}")
}

/// `Reason: {reason}` when a reason is present.
pub fn monitor_reason_line(reason: &str) -> String {
    format!("Reason: {reason}")
}

/// `Changed paths: {count}`.
pub fn monitor_changed_paths_count_line(count: usize) -> String {
    format!("Changed paths: {count}")
}

/// Indented path line under the changed-paths section.
pub fn monitor_path_line(path: &str) -> String {
    format!("  {path}")
}

/// Ordered human lines for monitor inspect output.
///
/// Includes reason only when `reason` is `Some`. Path lines are included
/// only when `include_paths` is true (order preserved).
pub fn monitor_human_lines(
    backend: &str,
    status: &str,
    reason: Option<&str>,
    changed_path_count: usize,
    paths: &[String],
    include_paths: bool,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(
        3 + usize::from(reason.is_some()) + if include_paths { paths.len() } else { 0 },
    );
    lines.push(monitor_backend_line(backend));
    lines.push(monitor_status_line(status));
    if let Some(reason) = reason {
        lines.push(monitor_reason_line(reason));
    }
    lines.push(monitor_changed_paths_count_line(changed_path_count));
    if include_paths {
        for path in paths {
            lines.push(monitor_path_line(path));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn individual_lines() {
        assert_eq!(monitor_backend_line("fs"), "Backend: fs");
        assert_eq!(monitor_status_line("ready"), "Status: ready");
        assert_eq!(monitor_reason_line("ok"), "Reason: ok");
        assert_eq!(monitor_changed_paths_count_line(2), "Changed paths: 2");
        assert_eq!(monitor_path_line("a.txt"), "  a.txt");
    }

    #[test]
    fn human_lines_without_paths() {
        let lines = monitor_human_lines("fs", "ready", None, 0, &[], false);
        assert_eq!(
            lines,
            vec![
                "Backend: fs".to_string(),
                "Status: ready".to_string(),
                "Changed paths: 0".to_string(),
            ]
        );
    }

    #[test]
    fn human_lines_with_reason_and_paths() {
        let paths = vec!["a".into(), "b".into()];
        let lines = monitor_human_lines("fs", "dirty", Some("mtime"), 2, &paths, true);
        assert_eq!(lines[0], "Backend: fs");
        assert_eq!(lines[1], "Status: dirty");
        assert_eq!(lines[2], "Reason: mtime");
        assert_eq!(lines[3], "Changed paths: 2");
        assert_eq!(lines[4], "  a");
        assert_eq!(lines[5], "  b");
    }
}
