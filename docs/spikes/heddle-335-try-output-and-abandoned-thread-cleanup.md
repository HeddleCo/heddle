# heddle#335 — `try` globals after `--` and abandoned-thread cleanup

**Status:** spike decision note. Implementation remains in #309; this spike
adds no production code.

**Governing rule:** CLI ergonomics over feature count. A new surface is justified
only when it prevents a concrete ambiguity that documentation cannot remove.

## Scope and decision summary

This spike covers `try`, not `attempt`. In this checkout the top-level
`Commands` enum begins at `crates/cli-args/src/cli/cli_args/commands_main.rs:76`
and contains `Try(TryArgs)` at
`crates/cli-args/src/cli/cli_args/commands_main.rs:200`; it has no `Attempt`
variant. Runtime dispatch likewise has `Commands::Run` and `Commands::Try` at
`crates/cli/src/main.rs:389` and `crates/cli/src/main.rs:393`, with no
`Commands::Attempt` arm. Internal comments that mention an attempt workflow do
not create a CLI command.

The decisions are:

1. Choose **A — detect and fail loudly** when a canonical long-form Heddle
   global option appears after `try`'s `--`. Do not lift or reorder it
   automatically. Add one explicit escape,
   `--allow-heddle-global-args`, for an inner command that legitimately owns
   the same spelling.
2. Hide abandoned threads from the default `thread list` view, expose them with
   `thread list --include-abandoned`, and add
   `thread cleanup --abandoned` as the explicit batch cleanup mode. Cleanup
   removes live operational residue but retains the abandoned manager record,
   states, objects, and oplog audit history.

## Decision 1 — fail on Heddle globals after `try --`

### Current behavior

`TryArgs.command` says that everything after `--` lands in the command vector
at `crates/cli-args/src/cli/cli_args/commands_args.rs:848`. Clap declares that
vector with both `trailing_var_arg = true` and `allow_hyphen_values = true` at
`crates/cli-args/src/cli/cli_args/commands_args.rs:850`. `cmd_try` then launches
the first vector item as the program and passes every remaining item unchanged
as child arguments at `crates/cli/src/cli/commands/try_cmd.rs:208` and
`crates/cli/src/cli/commands/try_cmd.rs:210`.

Consequently, in:

```text
heddle try -- bash -lc "printf ok" --output json
```

the `--output json` pair is child argv. It never reaches the global `output`
field declared at `crates/cli-args/src/cli/cli_args/cli_base.rs:57`, so Heddle
uses its default text output even though the invocation visually appears to ask
for JSON.

### Chosen shape

Before `try` opens a repository, creates a thread, or starts the child, inspect
the raw argv after the first top-level `--`. Preserve raw argv for this check;
the parsed command vector is not enough because it no longer distinguishes the
separator. The current main path collects raw argv immediately before Clap at
`crates/cli/src/main.rs:177` and parses it at `crates/cli/src/main.rs:178`, so
the implementation has a natural pre-dispatch source of truth.

The detector recognizes only these canonical long spellings from `Cli`:

- `--output VALUE` and `--output=VALUE`, where `VALUE` is exactly `text`,
  `json`, or `json-compact`;
- `--no-color`;
- `--repo PATH` and `--repo=PATH`;
- `--verbose`;
- `--quiet`;
- `--op-id VALUE` and `--op-id=VALUE`.

Those are the actual global fields declared at
`crates/cli-args/src/cli/cli_args/cli_base.rs:57`,
`crates/cli-args/src/cli/cli_args/cli_base.rs:61`,
`crates/cli-args/src/cli/cli_args/cli_base.rs:65`,
`crates/cli-args/src/cli/cli_args/cli_base.rs:69`,
`crates/cli-args/src/cli/cli_args/cli_base.rs:73`, and
`crates/cli-args/src/cli/cli_args/cli_base.rs:82`. The three output values are
the `CliOutputMode` variants at
`crates/cli-args/src/cli/cli_args/output_mode.rs:8`.

Matching is token-based, not substring-based. Reuse the Clap-metadata-driven
long-option/value matching already implemented at `crates/cli/src/main.rs:972`
so a separate-value option matches only when it has a syntactically complete
value, and an equals form matches only the exact option plus `=`. Apply the
explicit output-value allowlist above after that structural match. A shell
program string such as the single argv token `"tool --output json"` does not
match. Unknown values such as `--output artifact.json` do not match `--output`,
because they are not valid Heddle output modes.

Short spellings (`-C`, `-v`, `-q`, attached values, and short clusters) are not
detected. They collide too often with ordinary child commands, and their intent
is materially less legible than a canonical long spelling after an explicit
separator. `--help` and `--version` are also excluded: they are conventional
child options, not fields in the global `Cli` shape above.

If one or more matches are present, reject the invocation with usage exit code
64. The preflight is non-mutating: no repository open that can bootstrap
metadata, no ephemeral thread, and no child process. Collect all matches and
report them together rather than making the user fix one at a time.

Do not use a detected post-separator `--output` to choose the error renderer.
Only a real pre-separator global controls text versus JSON. Thus the broken
example exits 64 with a loud text error; an invocation that already selected
`--output json` before `--` receives the same typed refusal as a JSON error
envelope.

The typed refusal is:

```text
kind: try_global_option_after_separator
Error: Heddle global options appear after `--` and would be passed to the inner command: `--output json`.
Hint: Move Heddle options before `--`. If the listed options belong to the inner command, add `--allow-heddle-global-args` before `--`.
```

The primary `Next:` command is the exact shell-quoted invocation with all
detected option tokens moved, in their original order, immediately before the
separator. For the example above, the complete text surface is:

```text
Error: Heddle global options appear after `--` and would be passed to the inner command: `--output json`.
Next: heddle try --output json -- bash -lc 'printf ok'
Also: heddle try --allow-heddle-global-args -- bash -lc 'printf ok' --output json
```

The JSON envelope carries the same `kind`, `error`, and `hint`, plus the exact
corrected invocation as `primary_command` and both invocations in
`recovery_commands`. The existing text error renderer emits `Error:`, `Next:`,
and `Also:` from typed advice at
`crates/cli/src/cli/commands/error_envelope.rs:79`,
`crates/cli/src/cli/commands/error_envelope.rs:86`, and
`crates/cli/src/cli/commands/error_envelope.rs:92`.

### Legitimate inner-command collisions

Add this `try` option before the separator:

```text
--allow-heddle-global-args  Allow Heddle global-option spellings after `--` and pass them unchanged to the inner command.
```

It disables this detector only; it does not lift, delete, or reinterpret any
child argv. For example:

```text
heddle try --allow-heddle-global-args -- my-tool --output json
```

passes `--output json` to `my-tool`. The escape earns its surface because a
blanket detector without one would make valid child CLIs unreachable. Its long,
explicit name makes the exceptional intent reviewable in scripts.

Add this exact paragraph to `try --help`, before the examples:

```text
Heddle options must appear before `--`. Everything after `--` is passed unchanged to the inner command. If the inner command uses a Heddle global-option spelling, pass `--allow-heddle-global-args` before `--`.
```

### Rejected alternatives

**B, documentation only, is insufficient.** It preserves the current successful
exit and text output, so automation cannot distinguish a misplaced output
request from an intentional text request. Help copy is still useful, but it is
not the enforcement boundary.

**C, lifting globals, is rejected.** Rewriting child argv guesses intent and can
silently change a legitimate inner command. The chosen refusal presents the
corrected command while leaving the decision with the caller.

Detecting every short spelling is also rejected. It turns common child flags
such as `-v` and `-q` into Heddle errors and makes the escape the normal path.
Canonical long options plus an explicit escape catch the legible footgun without
claiming ownership of ordinary child syntax.

## Decision 2 — hide abandoned threads; clean them with `cleanup`

### Current behavior

`thread list` currently has one visibility flag, `--include-auto`, declared at
`crates/cli-args/src/cli/cli_args/commands_thread.rs:187` and
`crates/cli-args/src/cli/cli_args/commands_thread.rs:196`. Its domain collector
only applies that auto-thread filter at `crates/core/src/thread.rs:292` and
`crates/core/src/thread.rs:294`; it has no abandoned-state visibility option.
An abandoned manager record is skipped only when its live thread ref is already
absent, at `crates/core/src/thread.rs:338`.

Ordinary `thread drop` changes the record to `ThreadState::Abandoned` and saves
it at `crates/cli/src/cli/commands/thread_cmd.rs:1477`, but deletes the thread
ref only when `--delete-thread` was requested at
`crates/cli/src/cli/commands/thread_cmd.rs:1491`. That is why a default drop can
leave an abandoned row in `thread list`.

The existing `thread cleanup` is already the batch lifecycle-maintenance verb.
Its `--merged` and `--auto --older-than` modes are documented at
`crates/cli-args/src/cli/cli_args/commands_thread.rs:120`, and it already has a
shared `--dry-run` at
`crates/cli-args/src/cli/cli_args/commands_thread.rs:224`. Today its selector
explicitly skips abandoned records at
`crates/cli/src/cli/commands/thread_cmd.rs:1788`.

### Default list behavior

The default text and JSON views hide records whose lifecycle state is
`abandoned`. An abandoned thread is terminal, has no next workflow action, and
does not belong in the everyday work queue.

Add one positive opt-in flag, parallel to `--include-auto`:

```text
--include-abandoned  Include threads whose lifecycle state is abandoned. Hidden by default because they are no longer actionable.
```

The two visibility filters compose independently. A non-current row appears
only if its auto status passes the existing auto filter and its lifecycle state
passes the new abandoned filter. Therefore an abandoned auto-thread requires
both `--include-auto` and `--include-abandoned`. Preserve the existing rule that
the current checkout is never hidden, even if metadata is inconsistent; the
current-thread exception is documented in the collector at
`crates/core/src/thread.rs:295`.

`--include-abandoned` must include abandoned manager records even when their
live refs are gone. This turns the flag into the audit/debug view and replaces
the current unconditional no-ref skip at `crates/core/src/thread.rs:338` with a
visibility-aware decision. It does not recreate a ref or mutate the record.

### Cleanup command and selection

Add `--abandoned` as a third combinable mode on the existing command:

```text
heddle thread cleanup --abandoned [--dry-run]
```

The exact option help is:

```text
--abandoned  Clean abandoned threads that still have a live ref or checkout residue. States and audit history are retained.
```

The `thread cleanup` mode list gains this exact line:

```text
  - --abandoned: remove live refs and checkout residue left by abandoned threads; retain their records, states, and audit history.
```

An entry is eligible when its manager record is `abandoned` and at least one
cleanup residue remains: a live thread ref, an existing recorded execution
path, a thread manifest, actor-presence data, or an active writer lease for that
thread. A clean audit record with none of those resources does not match. This
makes a second `cleanup --abandoned` an honest zero-match no-op rather than
repeatedly reporting the same retained record.

For each eligible entry, reuse the existing cleanup teardown sequence: unmount
a recorded virtualized workspace if necessary, remove only its recorded
thread-owned execution path and manifest, remove actor-presence residue, mark
any active writer lease abandoned, and delete the live thread ref through the
recorded ref-mutation path. Recorded thread-ref deletion emits an oplog
`ThreadDelete` at `crates/repo/src/repository_ref_mutation.rs:97` and
`crates/repo/src/repository_ref_mutation.rs:103`.

Cleanup deliberately retains:

- the abandoned `ThreadManager` record and its workspace sidecar, matching the
  existing cleanup contract stated at
  `crates/cli/src/cli/commands/thread_cmd.rs:1976`;
- writer-lease records after their status is made non-active; the current store
  exposes abandonment, not deletion, at
  `crates/objects/src/store/writer_lease.rs:283`;
- every captured state and content object;
- existing oplog entries, including the recorded ref deletion.

This is operational pruning, not history erasure. The retained manager record
keeps `--include-abandoned` useful, and the immutable history preserves the
ability to inspect or recover state. The model already states that abandoning
an ephemeral thread leaves underlying states addressable at
`crates/repo/src/thread_model.rs:337`.

`--abandoned` combines with `--merged` and `--auto`. Classification precedence
is abandoned, then merged, then stale auto, so an abandoned auto-thread appears
only in `abandoned`; aggregate byte totals must not double-count it. Because the
abandoned selector only accepts a terminal state, it takes no `--older-than`
threshold.

### Safety and confirmation

There is no interactive prompt and no new `--force`. The explicit
`cleanup --abandoned` mode is the confirmation, and `thread drop` is the prior
user-visible transition to a terminal state. `--dry-run` is the preview path and
must perform no mutation.

Never clean the current checkout. Healthy state should make “current and
abandoned” impossible because ordinary drop already refuses the current lane,
but cleanup must preserve the existing fail-safe: report that row in `skipped`
with reason `active` and leave every resource unchanged. The current drop
refusal is implemented at `crates/cli/src/cli/commands/thread_cmd.rs:1413`.

The no-mode refusal becomes:

```text
Error: heddle thread cleanup requires at least one mode flag: --merged, --auto --older-than <duration>, or --abandoned.
Hint: Add --dry-run to the cleanup mode you intend; for abandoned threads, run `heddle thread cleanup --abandoned --dry-run`.
```

The abandoned-mode summaries are exact:

```text
would clean N abandoned thread(s) (would reclaim SIZE)
cleaned N abandoned thread(s) (reclaimed SIZE)
No threads matched the cleanup criteria.
```

JSON extends the existing `thread_cleanup` output with an `abandoned` array in
the same row shape as `merged` and `auto`; each row uses reason
`"abandoned"`. `reclaimed_bytes`, `would_reclaim_bytes`, `dry_run`, and
`skipped` retain their current meanings. The current output struct defines those
fields at `crates/cli/src/cli/commands/thread_cmd.rs:1578`.

### Interaction with `drop` and rejected shapes

`thread drop <thread>` remains the one-thread “stop this work” operation: tear
down its checkout, mark its record abandoned, and keep the ref unless
`--delete-thread` was explicitly supplied. The new list default makes the
result leave the everyday view immediately. `thread cleanup --abandoned` is the
batch maintenance operation that removes any live ref and residue later.
`thread list --include-abandoned` remains the inspection path before and after
cleanup.

Reject `thread drop --purge`. `drop` already owns the lifecycle transition and
one-thread teardown; adding a second, stronger deletion semantic overloads that
verb and encourages users to decide retention during an urgent stop action.
The existing `--delete-thread` flag at
`crates/cli-args/src/cli/cli_args/commands_args.rs:1044` already covers explicit
single-ref removal.

Reject `thread prune` and `thread purge`. Either adds a third lifecycle verb for
a selector that fits the established `cleanup` mode pattern. “Purge” also
suggests state or audit-history erasure, which this design explicitly does not
perform.

Reject `thread list --no-abandoned`. A negative flag whose default is already
“no abandoned rows” is redundant. The positive `--include-abandoned` spelling
matches `--include-auto` and clearly marks the exceptional audit view.

## Implementation acceptance for #309

#309 should be able to proceed without another product decision. Its tests must
pin the raw-token matching boundary, all canonical long forms, invalid output
values, shell-string non-matches, the explicit pass-through escape, the exact
typed error copy and exit 64, and proof that refusal creates no thread or child
side effect.

Thread tests must pin default hiding in text and JSON, independent composition
of both include flags, audit visibility after ref cleanup, abandoned residue
selection, dry-run immutability, idempotent zero-match reruns, current-thread
skipping, ref deletion with retained record/states/oplog, combined cleanup modes,
and the extended JSON schema. This spike requires no build; those are
implementation acceptance criteria, not work performed here.

## Proposed follow-ups

- **#309 (existing, not newly filed):** implement both decisions and their CLI,
  schema, help, and integration tests. Its title and acceptance text should drop
  `attempt`, because that command is absent from the current CLI.
- **Proposed, not filed — audit `run` separator collisions:** `RunArgs.command`
  uses the same trailing-vector declaration at
  `crates/cli-args/src/cli/cli_args/commands_args.rs:862`. Decide separately
  whether the detector should become shared after #309 proves the `try` UX;
  do not expand #309 or invent an `attempt` command as part of that audit.
