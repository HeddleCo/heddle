// SPDX-License-Identifier: Apache-2.0
//! Shared `--dry-run` plan model for the mutating "scary" verbs
//! (`push`, `land`, `ready`).
//!
//! A dry run reports what a command *would* do — refs that would move,
//! conflicts / non-fast-forward risks it would hit, policy/verify
//! verdicts, and a thread-integration preview — without performing any
//! mutation and without contacting the server for anything beyond
//! read/plan. The declared mutation surface is pulled from the command
//! catalog (`heddle help --output json`) so the plan cites the single
//! source of truth rather than re-deriving side effects here.

use serde::Serialize;

use super::command_catalog::{CommandSideEffectFlags, command_side_effect_flags};
use crate::cli::{Cli, render::write_json_stdout, should_output_json, style};

/// Stable `output_kind` discriminator for every dry-run plan.
pub const DRY_RUN_PLAN_OUTPUT_KIND: &str = "dry_run_plan";

/// A single ref that the real command would move, with the tip it would
/// move *from* (when locally observable) and *to*.
#[derive(Debug, Clone, Serialize)]
pub struct RefUpdatePreview {
    /// Human ref label, e.g. a thread name or git ref.
    pub name: String,
    /// Current tip, when the command can read it without mutating or a
    /// server round-trip. `None` when it lives on the remote and dry-run
    /// declines to fetch it.
    pub from: Option<String>,
    /// Tip the ref would point at after the command.
    pub to: Option<String>,
    /// True when applying this update would require `--force` (non
    /// fast-forward / rewrite).
    pub requires_force: bool,
}

/// A policy or verification verdict surfaced in the preview.
#[derive(Debug, Clone, Serialize)]
pub struct DryRunVerdict {
    /// Verdict source, e.g. `"verification"`, `"integration-policy"`.
    pub source: String,
    /// `"pass"`, `"blocked"`, `"warn"`, or a command-specific status.
    pub status: String,
    /// Human-readable one-line detail.
    pub detail: String,
}

/// Thread-integration preview shared by `land` and `ready`.
#[derive(Debug, Clone, Serialize)]
pub struct IntegrationPreview {
    pub thread: String,
    pub target: Option<String>,
    pub merge_relation: String,
    pub freshness: String,
    pub conflict_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    pub changed_path_count: usize,
    /// State the thread would transition to (e.g. `"ready"`, `"blocked"`),
    /// when the command decides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub would_transition_to: Option<String>,
    /// True when the command would first capture outstanding worktree
    /// changes before integrating.
    pub would_capture_dirty: bool,
}

/// The complete plan a `--dry-run` invocation reports.
#[derive(Debug, Clone, Serialize)]
pub struct DryRunPlan {
    pub output_kind: &'static str,
    /// Command the plan describes: `"push"`, `"land"`, `"ready"`.
    pub command: String,
    /// One-line human summary.
    pub summary: String,
    /// Declared mutation surface from the command catalog.
    pub side_effects: CommandSideEffectFlags,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ref_updates: Vec<RefUpdatePreview>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub integrations: Vec<IntegrationPreview>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<DryRunVerdict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Free-form notes, e.g. what dry-run intentionally skipped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Always `false` — a dry run performs no mutation. Present in the
    /// wire contract so consumers can assert it.
    pub performed_mutation: bool,
}

impl DryRunPlan {
    /// Start a plan for `command`, seeding the declared side-effect
    /// surface from the catalog.
    pub fn new(command: &str, summary: impl Into<String>) -> Self {
        let side_effects = command_side_effect_flags(command).unwrap_or(CommandSideEffectFlags {
            may_move_ref: false,
            destructive_requires_force: false,
            network_io: false,
            writes_heddle_refs: false,
            writes_git_refs: false,
        });
        Self {
            output_kind: DRY_RUN_PLAN_OUTPUT_KIND,
            command: command.to_string(),
            summary: summary.into(),
            side_effects,
            ref_updates: Vec::new(),
            integrations: Vec::new(),
            verdicts: Vec::new(),
            blockers: Vec::new(),
            notes: Vec::new(),
            performed_mutation: false,
        }
    }

    pub fn note(&mut self, note: impl Into<String>) -> &mut Self {
        self.notes.push(note.into());
        self
    }

    /// Emit the plan as text or JSON depending on the CLI output mode.
    /// `config` is the repo config when available (drives JSON default).
    pub fn emit(&self, cli: &Cli, config: Option<&repo::Config>) -> anyhow::Result<()> {
        if should_output_json(cli, config) {
            write_json_stdout(self)?;
        } else {
            self.render_text();
        }
        Ok(())
    }

    fn render_text(&self) {
        println!(
            "{} {}",
            style::working_marker(),
            style::bold(&format!("dry run: {}", self.summary))
        );
        println!(
            "{}",
            style::dim("No changes were made. This is a preview of what would happen.")
        );

        let se = &self.side_effects;
        let mut surface = Vec::new();
        if se.may_move_ref {
            surface.push("moves refs");
        }
        if se.writes_heddle_refs {
            surface.push("writes Heddle refs");
        }
        if se.writes_git_refs {
            surface.push("writes Git refs");
        }
        if se.network_io {
            surface.push("network I/O");
        }
        if se.destructive_requires_force {
            surface.push("destructive (needs --force)");
        }
        if !surface.is_empty() {
            println!("  {}", style::field("would", &surface.join(", ")));
        }

        for update in &self.ref_updates {
            let from = update.from.as_deref().unwrap_or("(remote — not fetched)");
            let to = update.to.as_deref().unwrap_or("(unresolved)");
            let force = if update.requires_force {
                style::warn(" [requires --force]")
            } else {
                String::new()
            };
            println!("  ref {}: {} -> {}{}", update.name, from, to, force);
        }

        for integration in &self.integrations {
            let target = integration.target.as_deref().unwrap_or("(no target)");
            println!(
                "  integrate {} -> {} ({}, {})",
                integration.thread, target, integration.merge_relation, integration.freshness
            );
            if integration.conflict_count > 0 {
                println!(
                    "    {}",
                    style::warn(&format!(
                        "{} conflict(s): {}",
                        integration.conflict_count,
                        integration.conflicts.join(", ")
                    ))
                );
            }
            if integration.would_capture_dirty {
                println!(
                    "    {}",
                    style::dim("would first capture outstanding worktree changes")
                );
            }
            if let Some(next) = &integration.would_transition_to {
                println!("    thread would become: {}", style::thread_state(next));
            }
        }

        for verdict in &self.verdicts {
            let marker = match verdict.status.as_str() {
                "pass" | "clean" | "ready" | "verified" => style::ok_marker(),
                "blocked" | "failed" => style::error_marker(),
                _ => style::warn_marker(),
            };
            println!(
                "  {} {}: {}",
                marker,
                style::bold(&verdict.source),
                verdict.detail
            );
        }

        if !self.blockers.is_empty() {
            println!("  {}", style::warn("blockers:"));
            for blocker in &self.blockers {
                println!("    - {blocker}");
            }
        }

        for note in &self.notes {
            println!("  {}", style::dim(note));
        }
    }
}
