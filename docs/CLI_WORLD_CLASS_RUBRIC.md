# Heddle CLI world-class rubric

> **Status:** audit rubric. This is intentionally strict. A command that is
> useful but inconsistent should score poorly until the inconsistency is fixed.

Heddle's first public release is a Git overlay, but the CLI must be designed as
the front door for a future standalone version-control system. In overlay mode,
`git` compatibility is a data/model contract, not a process dependency: the
installed `heddle` binary must not require a `git` executable to exist on
`PATH` for supported Git-overlay workflows.

This rubric grades the entire command-line surface: command tree, flags,
arguments, help, text output, JSON output, colors, exit codes, error envelopes,
interactive behavior, unexpected input, Git-overlay behavior, and automation
contracts.

## Sources

The audit standard is grounded in these sources:

- [Command Line Interface Guidelines](https://clig.dev/): human-first design,
  composability via stdout/stderr/exit codes, concise help, examples, error
  suggestions, output modes, robustness, recoverability, and deprecation.
- [GNU command-line interface standards](https://www.gnu.org/prep/standards/html_node/Command_002dLine-Interfaces.html):
  POSIX-style option parsing, long options, common option names, `--help`, and
  `--version`.
- [Microsoft System.CommandLine design guidance](https://learn.microsoft.com/en-us/dotnet/standard/commandline/design-guidance):
  CLI surface is an API, consistency matters because scripts depend on it, and
  command/subcommand grouping should be explicit.
- [PatternFly CLI handbook](https://www.patternfly.org/developer-resources/cli-handbook/):
  accessible color use, direct error language, parseable structured output,
  non-interactive safety, verb-oriented command design, and flag clarity.
- Heddle's own principles: [docs/PRINCIPLES.md](PRINCIPLES.md), especially
  trust, disposability, composability, restraint, and honesty.
- Heddle's JSON contract: [docs/json-schemas.md](json-schemas.md).

## Scoring model

Each command path is scored out of 100. Top-level audits also score global
behavior out of 100. A release candidate is world-class only if it passes every
hard gate and clears the weighted score thresholds.

| Grade | Score | Meaning |
|---|---:|---|
| A+ | 97-100 | Best-of-class. No meaningful defects found. |
| A | 93-96 | Release quality. Minor polish issues only. |
| B | 85-92 | Usable, but not excellent. Requires cleanup before flagship launch. |
| C | 75-84 | Functional, inconsistent, or under-tested. Not launch quality. |
| D | 60-74 | Significant UX/API risk. Must be redesigned or quarantined. |
| F | <60 | Fails the CLI contract. Remove, hide, or rebuild. |

Release thresholds:

- Everyday commands must score **A or better**: `init`, `status`, `start`,
  `capture`, `checkpoint`, `log`, `show`, `diff`, `merge`, `resolve`, `undo`,
  `thread`, `bridge`, `doctor`, `diagnose`, `help`, `version`.
- Advanced commands must score **B or better** unless hidden from curated help.
- Hidden/internal commands must still pass hard gates, but may be exempt from
  human-output polish if they are documented as machine-only.
- Global behavior must score **A+**. A world-class CLI cannot have inconsistent
  parse, output, exit-code, or dependency behavior.

## Hard gates

Any hard-gate failure caps the affected command at **F**, regardless of weighted
score. A global hard-gate failure blocks release.

1. **No `git` executable dependency in Git-overlay mode.** With `PATH` stripped
   of `git`, supported overlay workflows must still work: init/adopt, status,
   bridge import/status/sync/export where implemented, clone from local/bare
   repos where implemented, log/show/diff, checkpoint, merge, and fsck. Spawning
   `git` is allowed only in explicitly documented optional escape hatches.
2. **Correct exit status.** Success returns 0. User, environment, data, conflict,
   parse, and internal failures return non-zero. Dry runs that find work to do
   still return 0 unless documented otherwise.
3. **Stream discipline.** Primary command output goes to stdout. Errors,
   warnings, progress, tips, diagnostics, deprecation notices, and logs go to
   stderr. JSON stdout is never polluted by prose.
4. **Machine mode parses.** Every advertised JSON output parses as one complete
   JSON value, or JSONL where explicitly documented. JSON-mode failures emit the
   registered error envelope on stderr.
5. **No panic or backtrace by default.** Unexpected input, corrupt repo state,
   permission errors, read-only filesystems, missing remotes, broken config, and
   unsupported terminals produce controlled errors with hints.
6. **No silent data loss.** Commands that delete, overwrite, discard, rewrite,
   purge, force-sync, or move refs must require explicit force/confirmation or a
   documented dry-run/preview path.
7. **Help exists and is reachable.** `heddle help <command>`,
   `heddle <command> --help`, and `heddle <command> -h` work for every public
   command path.
8. **Schema drift fails.** Any command documented as JSON-producing must be
   registered in `heddle schemas` and covered by schema/doc drift tests.
9. **Non-interactive safety.** Commands used in scripts must not block on prompts
   unless explicitly interactive; interactive prompts must have a non-interactive
   equivalent.
10. **Accessibility baseline.** Color never carries the only meaning. Output is
    understandable with `NO_COLOR=1`, non-TTY stdout, and common screen-reader
    constraints.

## Weighted rubric

### 1. Command model and information architecture: 12 pts

- 3 pts: command name matches user intent and Heddle's domain model.
- 2 pts: top-level placement is justified; everyday commands stay curated and
  advanced/internal commands do not flood first-run help.
- 2 pts: subcommands are grouped by object or workflow consistently.
- 2 pts: aliases are sparse, documented, and not future-hostile.
- 1 pt: hidden/internal commands are actually hidden from user-facing help.
- 2 pts: command does not duplicate another command's job without a clear
  compositional reason.

Audit evidence: `heddle help`, `heddle help advanced`, command enum, docs, and
real invocation transcripts.

### 2. Arguments and flags: 10 pts

- 2 pts: positional arguments are few, ordered predictably, and named clearly.
- 2 pts: flags use standard names where applicable: `--output`, `--verbose`,
  `--quiet`, `--force`, `--dry-run`, `--all`, `--format`, `--message`.
- 1 pt: short flags are reserved for high-frequency use and have long forms.
- 1 pt: booleans behave like booleans; avoid `--flag=true` requirements.
- 1 pt: destructive flags are explicit and visually hard to pass accidentally.
- 1 pt: repeated/list flags behave consistently across commands.
- 1 pt: `--` pass-through behavior is documented and tested where commands run
  child processes.
- 1 pt: invalid, missing, ambiguous, and conflicting flags produce specific
  parse errors with examples or hints.

### 3. Help, docs, and discoverability: 10 pts

- 2 pts: command help starts with purpose, usage, arguments, and common flags.
- 2 pts: examples are real, runnable, and cover the common workflow.
- 1 pt: complex workflows have terminal docs and web/docs links.
- 1 pt: default/no-arg behavior teaches the next action without dumping the
  entire command tree.
- 1 pt: help text uses `heddle <verb>` in usage lines, not bare subcommands.
- 1 pt: docs do not mention stale flags or commands.
- 1 pt: deprecations give the replacement command/flag.
- 1 pt: `doctor` or equivalent can diagnose docs/schema drift.

### 4. Text output quality: 12 pts

- 2 pts: default text output answers "what happened?", "what state am I in?",
  and "what should I do next?" when applicable.
- 2 pts: output is concise by default, with detail behind `--verbose`,
  subcommands, or explicit formats.
- 1 pt: empty, clean, no-op, already-in-sync, and nothing-to-do states are named
  explicitly when ambiguity would waste operator time.
- 1 pt: labels use the same vocabulary across commands: state, thread, marker,
  checkpoint, blocker, warning, next step.
- 1 pt: tables align without truncating meaningful data; truncation preserves
  the signal, not the noise.
- 1 pt: progress output appears only for long operations and does not hide
  actionable failure logs.
- 1 pt: color is restrained, semantic, disabled for non-TTY or `NO_COLOR`, and
  never required to understand status.
- 1 pt: output width handles narrow terminals and long paths/names.
- 1 pt: `--quiet` or equivalent suppresses nonessential chatter.
- 1 pt: no debug/info logs appear by default.

### 5. Machine output and automation contract: 14 pts

- 2 pts: read-shaped commands support `--output json`; default auto mode chooses
  human text on TTY and JSON or stable plain output when piped as documented.
- 2 pts: JSON fields use stable names for the same concept across commands.
- 2 pts: optional fields are explicit `null`; empty collections are `[]` or `{}`.
- 1 pt: JSON is compact single-line unless the command is documented as a pretty
  inspection surface.
- 1 pt: JSON stdout is parseable under success and never receives warnings.
- 1 pt: JSON-mode stderr errors use `{kind,error,hint}` or the registered
  envelope shape.
- 1 pt: schemas exist for every JSON command and match runtime output.
- 1 pt: commands that stream use documented JSONL/event framing.
- 1 pt: output ordering is deterministic unless explicitly time/live ordered.
- 1 pt: stable identifiers are present for objects automation must track.
- 1 pt: text polish changes do not break machine contracts.

### 6. Error handling and recovery: 12 pts

- 2 pts: errors name the actual failure class, not just "failed".
- 2 pts: each common failure includes one primary next step.
- 1 pt: hints are runnable or clearly marked as examples.
- 1 pt: typo/unknown command errors suggest likely commands without executing
  state-changing guesses.
- 1 pt: validation catches impossible combinations before side effects.
- 1 pt: partial failures report what changed, what did not, and how to recover.
- 1 pt: transient failures are retryable/idempotent where possible.
- 1 pt: interrupted operations can be continued, aborted, or safely re-run.
- 1 pt: internal errors preserve bug-report context via `version --verbose` or
  equivalent.
- 1 pt: errors remain useful in text, JSON, non-TTY, and no-color modes.

### 7. Git-overlay and standalone trajectory: 10 pts

- 3 pts: overlay workflows use library/native implementations and pass with no
  `git` executable on `PATH`.
- 1 pt: overlay output never implies a Git operation succeeded when only Heddle
  state changed, or vice versa.
- 1 pt: import/export/sync distinguish walked commits, created states,
  no-ops, conflicts, and already-in-sync cases.
- 1 pt: divergent Git/Heddle state offers explicit directional recovery, not a
  generic sync that could lose data.
- 1 pt: command names and docs describe Heddle concepts first, Git bridge second.
- 1 pt: standalone/native mode has parallel terms and outputs where applicable.
- 1 pt: migration paths from overlay to native are visible and testable.
- 1 pt: all Git compatibility claims have fixtures against real repositories,
  including empty repos, branches, tags, merges, renames, binary blobs, large
  histories, weird paths, and corrupt inputs.

### 8. Safety, trust, and state integrity: 10 pts

- 2 pts: destructive operations have preview/dry-run and force semantics.
- 1 pt: operation state is durable enough for continue/abort/retry.
- 1 pt: oplog/undo records are written for meaningful mutations.
- 1 pt: actor/principal attribution is shown and serialized where relevant.
- 1 pt: confidence/trust metadata is not inflated by the UI.
- 1 pt: fsck/doctor surfaces integrity issues without requiring implementation
  knowledge.
- 1 pt: concurrent invocations avoid corrupting refs, indexes, object stores, or
  worktrees.
- 1 pt: read-only filesystem, full disk, permission denied, cross-device, and
  directory-not-empty errors have specific paths.
- 1 pt: config precedence is predictable and visible when it affects behavior.

### 9. Performance and responsiveness: 5 pts

- 1 pt: common commands return fast on large repos.
- 1 pt: long operations show progress on TTY and quiet structured output in
  automation.
- 1 pt: commands have reasonable timeouts for network/remote work.
- 1 pt: expensive scans are incremental, cached, or explicitly requested.
- 1 pt: performance budgets exist for everyday workflows and are enforced in CI.

### 10. Cross-platform and terminal behavior: 5 pts

- 1 pt: paths with spaces, unicode, shell metacharacters, and leading dashes are
  handled safely.
- 1 pt: behavior is tested on Linux and macOS at minimum; Windows expectations
  are explicit if unsupported.
- 1 pt: terminal width, TTY detection, paging, and color are stable.
- 1 pt: shell completion works for supported shells and does not expose hidden
  commands by accident.
- 1 pt: install/update/version behavior is predictable across package managers.

## Audit matrix

For every public command path, record:

| Field | Required evidence |
|---|---|
| Command path | Full path, aliases, hidden/public status |
| Purpose | One-sentence user/job statement |
| User class | human, agent, script, maintainer, support, internal |
| Lifecycle | read-only, mutating, destructive, long-running, interactive |
| Inputs | positionals, flags, env vars, config, stdin |
| Outputs | stdout text, stdout JSON, stderr, files, repo state |
| Exit codes | success, parse error, validation error, conflict, environment failure |
| Help | `help`, `--help`, examples, docs link |
| Error cases | at least five expected failures plus one unexpected/corrupt input |
| Safety | dry-run/preview/force/undo/continue/abort behavior |
| Git-free overlay | pass/fail with `git` absent from `PATH` |
| Accessibility | no-color, non-TTY, narrow terminal |
| Tests | integration, snapshot/schema, regression, property/fuzz where relevant |
| Score | points, hard-gate failures, grade, required fixes |

## Required test scenarios

Every everyday command should have automated coverage for:

- `--help`, `-h`, and `heddle help <command>`.
- text output on TTY-like mode.
- `--output json` success where the command has machine output.
- JSON-mode failure envelope for at least one representative failure.
- non-TTY stdout and stderr separation.
- `NO_COLOR=1`.
- `--verbose` and quiet/default log suppression.
- missing repo or wrong repo type.
- path with spaces.
- unknown/ambiguous IDs where IDs are accepted.
- dirty worktree, clean worktree, and no-op state where relevant.
- read-only filesystem or permission-denied failure for mutating commands.
- interrupted/continue/abort path for multi-step operations.
- `PATH` without `git` for all Git-overlay-supported behavior.

## Red flags

These should trigger redesign discussion, not small wording fixes:

- A command requires users to know whether Heddle currently stores something in
  Git or native storage.
- A command says "synced", "merged", "checkpointed", or "clean" when only part
  of that claim is true.
- A recovery hint points to a command that can fail in the exact state that
  produced the hint.
- Text output is beautiful but the JSON contract is missing or unstable.
- JSON contains convenient implementation leakage instead of a user-facing
  contract.
- A command mutates state before validating all obvious input errors.
- A hidden compatibility alias becomes easier to discover than the canonical
  command.
- A flag means different things on sibling commands.
- A command shells out to `git` for a supported overlay workflow.
- A command's default is optimized for demo output rather than repeated daily
  use.

## Release decision

Heddle is ready to present as a best-of-class OSS CLI only when:

1. All hard gates pass globally.
2. All everyday commands score A or better.
3. No advanced public command scores below B.
4. `docs/json-schemas.md`, `heddle schemas`, and runtime JSON agree.
5. `heddle doctor docs --all --json` and schema drift checks are clean.
6. The Git-overlay matrix passes with no `git` executable on `PATH`.
7. Every C-or-lower audit finding has an owner, issue, and release-blocking
   decision.

Until then, call it alpha/beta honestly and keep breaking the surface where the
surface is wrong.
