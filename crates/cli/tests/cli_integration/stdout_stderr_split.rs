// SPDX-License-Identifier: Apache-2.0
//! Stdout/stderr split contract for `--output json`.
//!
//! Agents rely on stdout being machine output (or empty on failure) and
//! stderr being everything else: diagnostics, error envelopes, progress.
//! Mixing the two breaks scripted callers — a stray `println!` on the
//! happy path leaks a diagnostic into the JSON stream and the parser
//! barfs.
//!
//! This module asserts the contract for every command in the catalog
//! that declares `supports_json: true`. The test is structured so each
//! command runs in isolation: a fresh `TempDir`, `heddle init`, then
//! the command under test with `--output json`. Failures are expected
//! and fine — what we care about is *where* the failure output lands.

use serde_json::Value;
use tempfile::TempDir;

use super::*;

/// Commands that block, fan out, or otherwise can't terminate quickly
/// in a sandboxed run. The contract still applies to them; we just
/// can't drive them here.
const SKIP: &[&str] = &[
    "daemon serve",
    "agent serve",
    "monitor",
    "watch",
    "shell",
    "discuss",
    "review",
    // `run` invokes external child processes that need their own
    // sandboxing setup.
    "run",
    // `try` runs a child Heddle in a worktree it sets up — too
    // expensive for a per-command lint sweep.
    "try",
    // Hosted commands need network credentials.
    "auth",
    "support",
    "support grant",
    "support list",
    "support revoke",
    "presence",
    "presence publish",
];

#[test]
fn json_mode_keeps_stdout_clean_on_every_catalog_command() {
    let catalog = cli::cli::commands::build_command_catalog();
    let mut checked = 0_usize;
    let mut violations: Vec<String> = Vec::new();

    for entry in &catalog.commands {
        if !entry.supports_json || entry.has_subcommands {
            continue;
        }
        if SKIP.contains(&entry.display.as_str()) {
            continue;
        }

        let temp = TempDir::new().expect("scratch temp dir");
        // Ignore init failure — the per-command run below will produce an
        // error envelope on stderr that we still want to check.
        let _ = heddle(&["init"], Some(temp.path()));

        let mut argv: Vec<&str> = vec!["--output", "json"];
        for segment in &entry.path {
            argv.push(segment.as_str());
        }

        let output = match heddle_output(&argv, Some(temp.path())) {
            Ok(o) => o,
            Err(err) => {
                violations.push(format!("{}: failed to invoke: {err}", entry.display));
                continue;
            }
        };

        checked += 1;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout_trimmed = stdout.trim();
        if stdout_trimmed.is_empty() {
            continue;
        }

        // Either one well-formed JSON document, or NDJSON (one JSON
        // value per line) for streaming commands.
        let parses_as_single = serde_json::from_str::<Value>(stdout_trimmed).is_ok();
        let parses_as_ndjson = !parses_as_single
            && stdout_trimmed
                .lines()
                .filter(|line| !line.trim().is_empty())
                .all(|line| serde_json::from_str::<Value>(line.trim()).is_ok());

        if !parses_as_single && !parses_as_ndjson {
            violations.push(format!(
                "{}: stdout under --output json is neither one JSON \
                 document nor NDJSON.\nfirst 400 chars: {}",
                entry.display,
                &stdout.chars().take(400).collect::<String>()
            ));
        }
    }

    assert!(
        checked > 20,
        "expected to sweep at least 20 json-capable commands; got {checked}. \
         Catalog may have shifted; review SKIP list."
    );

    assert!(
        violations.is_empty(),
        "JSON-mode stdout/stderr split violated by {} command(s):\n\n{}",
        violations.len(),
        violations.join("\n\n")
    );
}
