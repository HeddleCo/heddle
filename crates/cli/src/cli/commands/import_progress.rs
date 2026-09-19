// SPDX-License-Identifier: Apache-2.0
//! Shared human progress rendering for Git history import flows.
//!
//! This is a thin *consumer* of the generic [`Progress`](objects::Progress)
//! substrate: it owns the import-specific phrasing (the `[n/3]` phased steps and
//! the per-commit inspected/percent body) and drives a `Progress` handle. All
//! TTY concerns — `\r`-redraw, dim styling, the completion line, and the
//! ordinary JSON suppression policy — live in `progress_render`. The
//! per-commit throttle stays here because the body carries fields
//! (`states_created`) the generic snapshot does not model.

use ingest::ImportProgressEvent;
use objects::Progress;
use repo::Repository;

use crate::cli::{
    Cli,
    progress_render::{COMMIT_TICK_INTERVAL, TerminalSink, finish_line, progress_for},
    should_output_json, style,
};

pub(crate) struct ImportProgress {
    /// The generic progress handle. Finite import commands can explicitly keep
    /// it active for JSON because their structured result remains on stdout.
    progress: Progress,
    current: usize,
    total: usize,
}

impl ImportProgress {
    pub(crate) fn start(cli: &Cli, repo: &Repository, scope: &str, source_label: &str) -> Self {
        Self::start_with_progress(cli, repo, scope, source_label, progress_for(cli, repo))
    }

    pub(crate) fn start_for_finite_import(
        cli: &Cli,
        repo: &Repository,
        scope: &str,
        source_label: &str,
    ) -> Self {
        Self::start_with_progress(
            cli,
            repo,
            scope,
            source_label,
            Progress::with_sink(Box::new(TerminalSink::new())),
        )
    }

    fn start_with_progress(
        cli: &Cli,
        repo: &Repository,
        scope: &str,
        source_label: &str,
        progress: Progress,
    ) -> Self {
        if !should_output_json(cli, Some(repo.config())) {
            println!(
                "{} {} from {}",
                style::dim("Importing Git history:"),
                scope,
                style::dim(source_label)
            );
        }
        let this = Self {
            progress,
            current: 0,
            total: 3,
        };
        this.progress.set_total(this.total);
        this.step("scanning refs");
        this
    }

    pub(crate) fn advance(&mut self, label: &str) {
        self.current += 1;
        self.step(label);
    }

    pub(crate) fn begin_commit_import(&mut self) {
        self.advance("importing commits");
    }

    pub(crate) fn checking_notes(&self) {
        self.step("checking Heddle notes");
    }

    pub(crate) fn ordering_commits(&self) {
        self.step("ordering commits");
    }

    pub(crate) fn begin_ref_write(&mut self) {
        self.advance("writing refs");
    }

    /// Live per-commit counter for the import phase. When active, progress uses
    /// stderr and never leaks into machine-readable stdout (#550). Throttling
    /// on the commit count happens here — a throttled
    /// tick becomes a `set_phase` on the substrate, which the `TerminalSink`
    /// paints (it always repaints on a phase-string change).
    pub(crate) fn commit_tick(&mut self, event: ImportProgressEvent) {
        if !self.progress.is_active() {
            return;
        }
        if !should_render_commit_progress(event) {
            return;
        }
        let label = if event.total_commits == 0 {
            "counting commits"
        } else {
            "importing commits"
        };
        let line = format!(
            "[{}/{}] {label}... {}",
            self.current + 1,
            self.total,
            format_commit_progress(event),
        );
        self.progress.set_phase(line);
    }

    pub(crate) fn finish(&mut self) {
        if !self.progress.is_active() {
            return;
        }
        self.current = self.total;
        finish_line(&self.progress, "[done] imported Git history");
    }

    /// Paint a phased `[n/total] label...` step. `set_phase` forces a repaint on
    /// the phase-string change, matching the historic always-render step.
    fn step(&self, label: &str) {
        if !self.progress.is_active() {
            return;
        }
        let next = self.current + 1;
        self.progress
            .set_phase(format!("[{next}/{}] {label}...", self.total));
    }
}

fn should_render_commit_progress(event: ImportProgressEvent) -> bool {
    event.commits_imported == 0
        || event.commits_imported == event.total_commits
        || event.commits_imported.is_multiple_of(COMMIT_TICK_INTERVAL)
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

    #[test]
    fn import_commit_progress_is_throttled_but_keeps_edges() {
        assert!(should_render_commit_progress(ImportProgressEvent {
            commits_imported: 0,
            total_commits: 128,
            states_created: 0,
        }));
        assert!(should_render_commit_progress(ImportProgressEvent {
            commits_imported: 64,
            total_commits: 128,
            states_created: 40,
        }));
        assert!(should_render_commit_progress(ImportProgressEvent {
            commits_imported: 128,
            total_commits: 128,
            states_created: 80,
        }));
        assert!(!should_render_commit_progress(ImportProgressEvent {
            commits_imported: 63,
            total_commits: 128,
            states_created: 39,
        }));
    }
}
