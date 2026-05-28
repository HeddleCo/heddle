// SPDX-License-Identifier: Apache-2.0
//! `Next:`-envelope invariant for command error paths.
//!
//! Every CLI failure must hand the caller an actionable next step: the
//! agent-native contract (HeddleCo/heddle#252) is that a non-zero exit is
//! never a dead end. Concretely, for a representative error case of each
//! swept command this enforces, on the stderr envelope:
//!
//! 1. **Text mode** — a non-empty `Next: <command>` line.
//! 2. **JSON mode** — a non-empty `primary_command` (the machine form of
//!    `Next:`) AND a non-null *typed* action: either a concrete
//!    `primary_command_argv` array the agent can exec directly, or a
//!    `primary_command_template` object for actions with placeholders
//!    (e.g. `heddle remote add <name> <url>`, whose argv is necessarily
//!    null because the inputs aren't known yet).
//!
//! This is the error-path sibling of the `output_kind` invariant
//! (cli_integration/output_kind_invariant.rs): like that test it drives a
//! curated set of representative invocations rather than every verb, since
//! most error conditions need a hand-built fixture. The swept set is the
//! `init`/`status`/`verify`/`commit`/`merge`/`push`/`pull` + `bridge git`
//! subset whose codes `docs/exit-codes.md` documents; `SWEPT_COVERAGE`
//! guards that each is exercised here.

use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle_output};

/// Swept commands whose error envelopes this lint exercises. Mirrors the
/// `docs/exit-codes.md` Coverage list. A divergence between this and the
/// `CASES` table fails `swept_commands_have_envelope_coverage`.
const SWEPT_COVERAGE: &[&str] = &[
    "init",
    "status",
    "verify",
    "commit",
    "merge",
    "push",
    "pull",
    "bridge git import",
    "bridge git sync",
    "bridge git reconcile",
];

/// One representative error case: the swept command it covers, the argv
/// (after the leading `--output` toggle) and a fixture builder.
struct ErrorCase {
    /// Swept command(s) this case exercises, for `SWEPT_COVERAGE`.
    covers: &'static [&'static str],
    label: &'static str,
    argv: &'static [&'static str],
    fixture: fn() -> TempDir,
}

fn git(args: &[&str], dir: &Path) {
    git_hermetic(args, dir);
}

/// A directory that is not a Heddle repo — commands that need one fail
/// with the `repository_not_found` envelope.
fn bare_dir() -> TempDir {
    TempDir::new().expect("tempdir")
}

fn init_repo() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle_output(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        Some(temp.path()),
    )
    .expect("heddle init");
    temp
}

fn committed_repo() -> TempDir {
    let temp = init_repo();
    std::fs::write(temp.path().join("f.txt"), "base\n").expect("write f.txt");
    heddle_output(&["commit", "-m", "base"], Some(temp.path())).expect("heddle commit");
    temp
}

fn adopted_git_overlay() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git(&["init", "-q", "-b", "main", "."], dir);
    git(&["config", "user.email", "heddle@test.example"], dir);
    git(&["config", "user.name", "Heddle Test"], dir);
    std::fs::write(dir.join("a.txt"), "hello\n").expect("write a.txt");
    git(&["add", "a.txt"], dir);
    git(&["commit", "-qm", "init"], dir);
    heddle_output(&["adopt"], Some(dir)).expect("heddle adopt");
    heddle_output(&["bridge", "git", "import", "--ref", "main"], Some(dir))
        .expect("heddle bridge git import");
    temp
}

/// `init` / `status` / `verify` / `merge` / `bridge git import|sync` exit
/// `0` on their documented happy paths, so this lint covers them through
/// adjacent error conditions: an unparseable invocation (`init`), a
/// missing-state lookup (`status`/`verify`/`merge` share the
/// `state_not_found` / refusal envelopes), etc. The goal is envelope
/// shape, not per-verb exhaustiveness.
fn cases() -> Vec<ErrorCase> {
    vec![
        ErrorCase {
            covers: &["init"],
            label: "init into an existing repo",
            argv: &[
                "init",
                "--principal-name",
                "Heddle Test",
                "--principal-email",
                "heddle@test.example",
            ],
            fixture: init_repo,
        },
        ErrorCase {
            covers: &["status", "verify"],
            label: "verify outside a repository",
            argv: &["verify"],
            fixture: bare_dir,
        },
        ErrorCase {
            covers: &["commit"],
            label: "commit with nothing staged",
            argv: &["commit", "-m", "again"],
            fixture: committed_repo,
        },
        ErrorCase {
            covers: &["merge"],
            label: "merge a nonexistent thread",
            argv: &["merge", "does-not-exist"],
            fixture: committed_repo,
        },
        ErrorCase {
            covers: &["push"],
            label: "push with no remote configured",
            argv: &["push"],
            fixture: init_repo,
        },
        ErrorCase {
            covers: &["pull"],
            label: "pull with no remote configured",
            argv: &["pull"],
            fixture: init_repo,
        },
        ErrorCase {
            covers: &["bridge git import"],
            label: "bridge git import of a missing ref",
            argv: &["bridge", "git", "import", "--ref", "no-such-branch"],
            fixture: adopted_git_overlay,
        },
        ErrorCase {
            // `sync` has its own handler (bridge.rs `GitCommands::Sync`)
            // that builds the error envelope independently of reconcile —
            // exercise it directly so a regression that drops Sync's `Next:`
            // fields fails the lint. A `--path` at a nonexistent source
            // reaches the handler (export runs, then the import half fails).
            covers: &["bridge git sync"],
            label: "bridge git sync against a missing source",
            argv: &["bridge", "git", "sync", "--path", "/heddle/no/such/source"],
            fixture: adopted_git_overlay,
        },
        ErrorCase {
            covers: &["bridge git reconcile"],
            label: "bridge git reconcile without a --prefer side",
            argv: &["bridge", "git", "reconcile", "--ref", "main"],
            fixture: adopted_git_overlay,
        },
    ]
}

/// The first non-empty line of `stderr` parsed as the JSON envelope.
fn parse_json_envelope(stderr: &str, label: &str) -> Value {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| panic!("[{label}] empty stderr; expected a JSON envelope"));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("[{label}] stderr is not a JSON envelope: {err}\n  line: {line}"))
}

#[test]
fn error_envelopes_carry_actionable_next_step() {
    let mut failures = Vec::new();

    for case in cases() {
        let fixture = (case.fixture)();
        let dir = fixture.path();

        // --- JSON mode -------------------------------------------------
        let mut json_argv = vec!["--output", "json"];
        json_argv.extend_from_slice(case.argv);
        let json_out = heddle_output(&json_argv, Some(dir))
            .unwrap_or_else(|err| panic!("[{}] spawn {json_argv:?}: {err}", case.label));
        if json_out.status.success() {
            failures.push(format!(
                "[{}] expected a non-zero exit to exercise the error envelope, got success",
                case.label
            ));
            continue;
        }
        let stderr = String::from_utf8_lossy(&json_out.stderr);
        let envelope = parse_json_envelope(&stderr, case.label);

        let primary_command = envelope.get("primary_command").and_then(Value::as_str);
        if primary_command.is_none_or(|cmd| cmd.trim().is_empty()) {
            failures.push(format!(
                "[{}] JSON envelope has empty/missing `primary_command` (the `Next:` step): {envelope}",
                case.label
            ));
        }

        // A typed action is either a concrete argv array or, for actions
        // with placeholders, a template object. Exactly one is non-null.
        let argv_ok = envelope
            .get("primary_command_argv")
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty());
        let template_ok = envelope
            .get("primary_command_template")
            .is_some_and(Value::is_object);
        if !argv_ok && !template_ok {
            failures.push(format!(
                "[{}] JSON envelope exposes no typed recommended action: \
                 `primary_command_argv` must be a non-empty array OR \
                 `primary_command_template` a non-null object: {envelope}",
                case.label
            ));
        }

        // --- Text mode -------------------------------------------------
        let text_out = heddle_output(case.argv, Some(dir))
            .unwrap_or_else(|err| panic!("[{}] spawn {:?}: {err}", case.label, case.argv));
        let text_stderr = String::from_utf8_lossy(&text_out.stderr);
        let next_line = text_stderr
            .lines()
            .find_map(|line| line.trim().strip_prefix("Next:"))
            .map(str::trim);
        if next_line.is_none_or(|next| next.is_empty()) {
            failures.push(format!(
                "[{}] text-mode stderr is missing a non-empty `Next:` line:\n{text_stderr}",
                case.label
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Error envelopes must surface an actionable next step (HeddleCo/heddle#252):\n  - {}",
        failures.join("\n  - ")
    );
}

#[test]
fn swept_commands_have_envelope_coverage() {
    let covered: std::collections::BTreeSet<&str> =
        cases().iter().flat_map(|case| case.covers.iter().copied()).collect();
    let missing: Vec<&str> = SWEPT_COVERAGE
        .iter()
        .copied()
        .filter(|cmd| !covered.contains(cmd))
        .collect();
    assert!(
        missing.is_empty(),
        "Every swept command must have a representative error case in `CASES`; missing: {missing:?}"
    );
}
