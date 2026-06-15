// SPDX-License-Identifier: Apache-2.0
//! Shared human progress rendering for Git history import flows.

use std::io::{self, IsTerminal, Write};

use ingest::ImportProgressEvent;
use repo::Repository;

use crate::cli::{Cli, should_output_json, style};

/// Redraw the live commit counter at most once per this many commits, so a
/// large import doesn't spend its time flushing the terminal.
const COMMIT_TICK_INTERVAL: usize = 64;

pub(crate) struct ImportProgress {
    enabled: bool,
    current: usize,
    total: usize,
}

impl ImportProgress {
    pub(crate) fn start(cli: &Cli, repo: &Repository, scope: &str, source_label: &str) -> Self {
        let enabled = !should_output_json(cli, Some(repo.config()));
        if enabled {
            println!(
                "{} {} from {}",
                style::dim("Importing Git history:"),
                scope,
                style::dim(source_label)
            );
        }
        let progress = Self {
            enabled,
            current: 0,
            total: 3,
        };
        progress.step("scanning refs");
        progress
    }

    pub(crate) fn advance(&mut self, label: &str) {
        self.current += 1;
        self.step(label);
    }

    pub(crate) fn detail(&self, label: &str) {
        self.step(label);
    }

    /// Live per-commit counter for the import phase. Renders only to a TTY
    /// (throttled); under `--json`/agent output or a piped stdout it is a
    /// no-op so no control codes leak into machine-readable output (#550).
    pub(crate) fn commit_tick(&mut self, event: ImportProgressEvent) {
        if !self.enabled || !io::stdout().is_terminal() {
            return;
        }
        let should_render = event.commits_imported == 0
            || event.commits_imported == event.total_commits
            || event.commits_imported.is_multiple_of(COMMIT_TICK_INTERVAL);
        if !should_render {
            return;
        }
        let label = if event.total_commits == 0 {
            "counting commits"
        } else {
            "importing commits"
        };
        print!(
            "\r{}\x1b[K",
            style::dim(&format!(
                "[{}/{}] {label}… {}",
                self.current + 1,
                self.total,
                format_commit_progress(event),
            ))
        );
        io::stdout().flush().ok();
    }

    pub(crate) fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        self.current = self.total;
        if io::stdout().is_terminal() {
            print!("\r\x1b[K{}\n", style::accent("[done] imported Git history"));
            io::stdout().flush().ok();
        } else {
            println!("{}", style::accent("[done] imported Git history"));
        }
    }

    fn step(&self, label: &str) {
        if !self.enabled {
            return;
        }
        let next = self.current + 1;
        if io::stdout().is_terminal() {
            print!(
                "\r\x1b[K{}",
                style::dim(&format!("[{next}/{}] {label}...", self.total))
            );
            io::stdout().flush().ok();
        } else {
            println!(
                "{}",
                style::dim(&format!("[{next}/{}] {label}", self.total))
            );
        }
    }
}

fn format_commit_progress(event: ImportProgressEvent) -> String {
    let inspected = if event.total_commits == 0 {
        format!(
            "{} inspected, counting reachable commits",
            event.commits_imported
        )
    } else {
        format!(
            "{}/{} inspected ({}%)",
            event.commits_imported,
            event.total_commits,
            event.commits_imported.saturating_mul(100) / event.total_commits
        )
    };
    format!("{inspected}, {} new states", event.states_created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_commit_progress_includes_total_percent_and_new_states() {
        let rendered = format_commit_progress(ImportProgressEvent {
            commits_imported: 64,
            total_commits: 128,
            states_created: 40,
        });

        assert_eq!(rendered, "64/128 inspected (50%), 40 new states");
    }

    #[test]
    fn import_commit_progress_handles_unknown_total_counting_phase() {
        let rendered = format_commit_progress(ImportProgressEvent {
            commits_imported: 0,
            total_commits: 0,
            states_created: 0,
        });

        assert_eq!(
            rendered,
            "0 inspected, counting reachable commits, 0 new states"
        );
    }
}
