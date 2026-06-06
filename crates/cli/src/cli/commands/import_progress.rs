// SPDX-License-Identifier: Apache-2.0
//! Shared human progress rendering for Git history import flows.

use std::io::{self, IsTerminal, Write};

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

    /// Live per-commit counter for the import phase. Renders only to a TTY
    /// (throttled); under `--json`/agent output or a piped stdout it is a
    /// no-op so no control codes leak into machine-readable output (#550).
    pub(crate) fn commit_tick(&mut self, count: usize) {
        if !self.enabled || !io::stdout().is_terminal() {
            return;
        }
        if !count.is_multiple_of(COMMIT_TICK_INTERVAL) {
            return;
        }
        print!(
            "\r{}\x1b[K",
            style::dim(&format!(
                "[{}/{}] importing commits… {count}",
                self.current + 1,
                self.total
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
