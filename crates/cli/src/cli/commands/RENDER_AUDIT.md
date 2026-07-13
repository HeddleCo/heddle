# Render audit

Classification of every CLI verb's renderer against the rule:
**`println!`/`print!` may only appear inside functions named `render_*`
or `write_*`** (or under a `#[cfg(test)]` block). `eprintln!` is
allowed everywhere — warnings/tips ride on stderr by design.

The classification is the audit's first cut, sourced from a quick read
of each file. Treat the table as a punch list, not a contract.

| Status        | Meaning                                                              |
|---------------|----------------------------------------------------------------------|
| `compliant`   | Builds a `*Output` struct and renders via `render_*` / `write_*`.    |
| `partial`     | Has a structured-output path but `println!` still leaks into the body of the handler. |
| `text-only`   | No structured output yet. Refactor target.                           |

## Status by file

| File                                                          | Status     | Notes                                              |
|---------------------------------------------------------------|------------|----------------------------------------------------|
| `agent_presence.rs`                                          | partial    | JSON path exists; render path uses `println!`.     |
| `agent.rs`                                                    | partial    |                                                    |
| `auth_cmd.rs`                                                 | text-only  | Auth flow needs structured output.                 |
| `bisect.rs`                                                   | partial    |                                                    |
| `blame.rs`                                                    | partial    |                                                    |
| `bridge.rs`                                                   | partial    | Remaining sites are inside the Git projection subcommand dispatcher and the import-progress reporter — still to chip. |
| `checkpoint.rs`                                               | partial    |                                                    |
| `clean.rs`                                                    | text-only  |                                                    |
| `clone.rs`                                                    | partial    |                                                    |
| `collapse.rs`                                                 | partial    |                                                    |
| `compare/compare_output.rs`                                   | compliant  | All output goes through `render_*`.                |
| `conflict.rs`                                                 | partial    |                                                    |
| `context/context_mutate.rs`                                   | partial    |                                                    |
| `context/context_query.rs`                                    | partial    |                                                    |
| `daemon/cmd.rs`                                               | partial    |                                                    |
| `doctor.rs`                                                   | partial    |                                                    |
| `diff/diff_output.rs`                                         | compliant  | Canonical example.                                 |
| `discuss.rs`                                                  | partial    |                                                    |
| `fetch.rs`                                                    | partial    |                                                    |
| `fork.rs`                                                     | compliant  | Body routes through `render_fork`.                 |
| `fsck.rs`                                                     | partial    |                                                    |
| `gc.rs`                                                       | text-only  |                                                    |
| `goto.rs`                                                     | partial    |                                                    |
| `hook.rs`                                                     | partial    |                                                    |
| `index.rs`                                                    | text-only  |                                                    |
| `init.rs`                                                     | compliant  | Body routes through `render_init`.                 |
| `integration.rs`                                              | partial    |                                                    |
| `log.rs`                                                      | compliant  | Reference structure-first verb.                    |
| `maintenance.rs`                                              | text-only  |                                                    |
| `marker.rs`                                                   | partial    |                                                    |
| `merge/mod.rs`                                                | partial    |                                                    |
| `monitor.rs`                                                  | text-only  |                                                    |
| `query.rs`                                                    | partial    |                                                    |
| `ready_cmd.rs`                                                | partial    |                                                    |
| `rebase/mod.rs`                                               | partial    |                                                    |
| `remote/mod.rs`                                               | partial    |                                                    |
| `resolve.rs`                                                  | partial    |                                                    |
| `revert.rs`                                                   | partial    |                                                    |
| `review.rs`                                                   | partial    | `review show` JSON parity is the test target.      |
| `semantic_cmd.rs`                                             | partial    |                                                    |
| `session.rs`                                                  | partial    |                                                    |
| `show.rs`                                                     | partial    |                                                    |
| `snapshot.rs`                                                 | compliant  |                                                    |
| `stash.rs`                                                    | partial    |                                                    |
| `status.rs`                                                   | compliant  | Reference structure-first verb.                    |
| `store_cmd.rs`                                                | text-only  |                                                    |
| `support.rs`                                                  | partial    |                                                    |
| `thread.rs`, `thread_cmd.rs`, `thread_shaping.rs`             | partial    | `thread.rs` print helpers (`print_thread_op`, `print_thread_sections`, `print_thread_entry`) renamed to `render_*`; sibling files still pending. |
| `transaction.rs`                                              | partial    |                                                    |
| `undo.rs`                                                     | partial    |                                                    |
| `watch.rs`                                                    | text-only  |                                                    |
| `workflow.rs`                                                 | partial    |                                                    |
| `workspace.rs`                                                | partial    |                                                    |

## How the lint enforces this

`crates/cli/tests/render_lint.rs` walks every file under
`crates/cli/src/cli/commands/` and counts `println!` / `print!`
invocations *outside* functions whose name starts with `render_` or
`write_` (or under `#[cfg(test)]`). The test asserts the total is
**at most** `RENDER_VIOLATION_BASELINE` (currently **722**).

Every cleanup PR that converts one file from `partial` → `compliant`
should *lower* the baseline by the number of removed calls. The test
will then enforce that no future PR re-introduces them. The baseline
ratchet is the part most likely to grow — a violation here costs five
minutes; growing the baseline back up costs us the audit.

When the baseline reaches zero, drop the constant and tighten the test
to `== 0`.
