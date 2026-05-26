// SPDX-License-Identifier: Apache-2.0
//! Shared human progress rendering for Git history import flows.

use std::io::{self, IsTerminal, Write};

use repo::Repository;

use crate::cli::{Cli, should_output_json, style};

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

    /// Print a sub-line beneath the current step. Useful when a phase
    /// has a long-running operation users would otherwise read as a
    /// hang — e.g. a network fetch during the hydrate step.
    pub(crate) fn note(&self, message: &str) {
        if !self.enabled {
            return;
        }
        // Drop any in-flight \r-line first so the note doesn't overwrite it.
        if io::stdout().is_terminal() {
            println!();
        }
        println!("    {}", style::dim(&format!("· {message}")));
    }

    pub(crate) fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        self.current = self.total;
        if io::stdout().is_terminal() {
            print!("\r{}\n", style::accent("[done] imported Git history"));
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
                "\r{}",
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
