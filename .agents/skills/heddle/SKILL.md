---
name: heddle
description: Drive the `heddle` version-control CLI correctly as an agent — the command mental model (capture/commit/push, start/ready/land, discuss/context/review), the `--output json` machine contract and per-verb schemas, the exit-code contract (75 = only safe-retry), `--op-id` idempotent replay, attribution, delegated scoped tokens, and the hosted push flow. Use whenever running any `heddle` command in automation, scripting a heddle workflow, or deciding whether a failed heddle command is safe to retry.
---

# Driving Heddle as an agent

Heddle is agent-native version control. It is built around **saved states**
(captured trees with a stable change id, attribution, and provenance) and
**isolated threads** (named lines of work with their own checkout). Git
compatibility is an interop/output layer, not the thing to think about first.

Heddle **fails closed**: when it cannot prove a move is safe it refuses and
prints a `Next:` line with the one primary recovery command. Read that line
instead of forcing the operation.

**Golden rules for an agent:**

1. Pass `--output json` (or `--output json-compact`) on every command you parse.
   Text is the default and is unstable — never scrape it.
2. Branch on the **exit code**, not on stderr text. Only **75** is safe to retry.
3. Pass `--op-id <UUID>` on mutating commands you might retry, so a replay is
   idempotent instead of doubling state.
4. Set attribution env vars before you make changes, so states are correctly
   credited to your agent.

The full, machine-generated command catalog (RW / JSON / op-id / summary for
every verb) is in **[commands.md](commands.md)** — regenerate it from
`heddle help --output json`; never invent a command.

## Command mental model

Source of truth: `heddle help model`, `heddle help advanced`.

**The everyday loop.**

```
heddle status                     # what needs attention + the next safe action
heddle diff                       # what changed in the worktree
heddle commit -m "..."            # save one change (+ a Git checkpoint in Git-overlay repos)
heddle start <name> --path ../<name>   # isolated thread with its own checkout
heddle ready                      # prepare the thread; shows the semantic integration summary
heddle land --thread <name>       # land a ready thread, optionally publish
heddle undo                       # thread-local rewind of the last operation
heddle verify                     # prove Heddle / Git / worktree / remotes agree
```

**Core nouns.**

- **State** — a captured tree with a stable change id, attribution, intent, and
  provenance. `log`, `show`, `diff`, `undo`, `query` reason over states.
- **Thread** — a named line of work with its own checkout and history. Use it
  for risky edits, agent work, or parallel experiments (no stash juggling).
- **Capture** — a cheap recoverable save point on the current thread (`capture`).
- **Commit** — the normal save path; Git-overlay repos also write the Git
  checkpoint.
- **Checkpoint** — the advanced Git-overlay boundary for already-captured work.
- **Verify** — the proof surface; nonzero until every check is clean.

**Three verb families you will use most:**

- **Save & publish:** `capture` → `commit` → `push` (and `checkpoint` for the
  Git-overlay boundary). `capture` is the low-level recoverable step; `commit`
  is the everyday save.
- **Thread lifecycle:** `start` → `ready` → `land` (managed threads). Prefer
  `land` over the raw `merge`/`rebase` primitives.
- **Collaboration & review:** `discuss` (open/append/resolve discussions
  anchored to symbols), `context` (code-context annotations agents can read and
  write), `review` (render a state's payload, sign it, check signal health).

`undo` is **thread-local** — it only rewinds operations recorded from the
current checkout. Run it from the specific isolated checkout you want to rewind.

## The machine contract

Source of truth: `heddle help output-formats`, `heddle schemas`,
`heddle help --output json`, and [`docs/json-schemas.md`](../../../docs/json-schemas.md).

- **`--output text`** (default, always — no TTY/pipe auto-detection). Human-only.
- **`--output json`** emits the full contract: a stable **`output_kind`**
  discriminator, exit codes, and recovery templates. Dispatch on `output_kind`.
- **`--output json-compact`** emits only the decision surface — `output_kind`,
  `status`/`coordination_status`, `blockers`, `next_action`, `changed_paths`,
  `conflicts` — same `output_kind`, fewer tokens.

**Per-verb JSON Schemas:** `heddle schemas <verb>` prints the JSON Schema; the
list of schema-bearing verbs is `heddle schemas` (no arg). The `output_kind`
field is the discriminator you switch on.

**Command catalog / side-effect catalog:** `heddle help --output json` returns
a `command_catalog` with, per command: `path`, `summary`, `tier`, `mutates`,
`supports_json`, `supports_op_id` / `op_id_behavior`, `json_discriminators`
(the `output_kind` values), `schema_verbs`, `exit_codes`, and a **side-effect
catalog** — `side_effects` / `side_effect_class` plus booleans like
`network_io`, `writes_worktree`, `writes_git_refs`, `writes_config`,
`destructive_data`, `may_import_git`, `requires_git_executable`. Read this to
know, before running, exactly what a command will touch.

**Recovery templates** carry an `agent_may_fill` flag. When it is `false`,
treat `action` / `argv_template` as display-only: do **not** substitute
`<name>`/`<url>` placeholders — surface to a human. Substituting and running
passes the literal `<name>` to Heddle and fails.

A verb without a JSON contract (`supports_json: false`) exits **65** if you
request `--output json`; fall back to a supported mode.

## Exit-code contract

Source of truth: [`docs/exit-codes.md`](../../../docs/exit-codes.md). Codes follow
BSD `sysexits.h`. Classification is keyed on typed error kinds, not message
text, so rewording an error never changes its code.

| Code | Symbol      | Meaning / agent action |
| ---: | ----------- | --- |
| 0    | `Ok`        | Success. |
| 64   | `Usage`     | Bad CLI args / unknown subcommand. Fix the invocation; do not retry as-is. |
| 65   | `DataErr`   | Well-formed input, semantically rejected (nothing to commit, unresolvable conflict, corrupt state, or `--output json` on a text-only verb). No retry helps — surface it. |
| 73   | `CantCreat` | Output file refused (exists / unwritable / state dir uncreatable). |
| 74   | `IoErr`     | Generic IO failure. **Catch-all** — treat any undeclared non-zero as this. |
| **75** | **`TempFail`** | **Transient — the ONLY code that is safe to retry with the same args.** |
| 76   | `Protocol`  | Remote rejected the payload. The *inputs* are the problem — do not loop; change strategy or surface. |
| 77   | `NoPerm`    | Refused for permission reasons. |
| 78   | `Config`    | Missing/ambiguous precondition (no upstream, no remote, no repo, conflicting identity). Print the missing setting; don't retry. |

`2` is reserved for `set -e` / unhandled panic — never emitted intentionally.

**Retry rule (memorize):** retry **only** on 75. Retrying a 76 resends the same
rejected payload; retrying a 78 hits the same missing precondition. Per-command
declared codes live in each catalog entry's `exit_codes`; commands not yet
swept contract only "0 on success, unspecified non-zero on failure" — treat
their failures as 74.

## Idempotent replay — `--op-id`

Source of truth: `heddle help operation-ids`.

Commands that advertise **`supports_op_id: true`** accept `--op-id <UUID>` (or
`HEDDLE_OPERATION_ID`). Replaying the **same id with the same body** returns the
recorded outcome; the **same id with a different body** returns a typed
conflict. Without an id, dedup is bypassed and the call just executes.

- `op_id_behavior: explicit_replay` — you must supply the id (this is the common
  case for mutating verbs). Generate one UUID v4 per logical operation and reuse
  it across retries of that operation.
- `op_id_behavior: none` — the command rejects `--op-id`.
- `generated_resume` / `persists_op_id: true` — reserved for commands that save
  a generated id across an interrupted retry loop.

Combine with the exit-code rule: on a **75**, retry with the **same `--op-id`**
— that is exactly what makes the retry safe.

The dedup store is file-backed locally
(`.heddle/state/operation_dedup.bin`, 7-day retention) and Postgres-backed in
hosted deployments.

## Attribution

Source of truth: `.agents/agent-workflows.md`, `heddle help agent-flags`.
Attribution is **auto-detected** from the harness when possible; set these env
vars to be explicit when you make changes:

```bash
export HEDDLE_AGENT_PROVIDER="anthropic"
export HEDDLE_AGENT_MODEL="claude-opus-4-5-20250120"
export HEDDLE_PRINCIPAL_NAME="Your Name"
export HEDDLE_PRINCIPAL_EMAIL="you@example.com"
```

Per-capture overrides (fall back to the env var, then config):
`--agent-provider`, `--agent-model`, `--policy`, `--no-agent`, `--no-policy`.
Precedence, highest first: explicit flag → active thread actor → env var →
harness probe → active session → user config → repo config.
`HEDDLE_SESSION_ID` / `HEDDLE_SESSION_SEGMENT` are **not** implemented — do not
rely on them.

## Multi-agent isolation

For concurrent agents, take an exclusive **writer lease** on a thread rather
than racing the worktree:

```bash
heddle start feature/auth --task "Implement auth middleware"
heddle agent reserve --thread feature/auth        # → lease_id + bearer token
heddle agent heartbeat --lease <LEASE> --token <TOKEN>
heddle agent capture   --lease <LEASE> --token <TOKEN> -m "..."
heddle agent ready     --lease <LEASE> --token <TOKEN>
heddle agent release   --lease <LEASE> --token <TOKEN>
heddle land --thread feature/auth
```

The lease is a five-minute heartbeat lease; `heartbeat`/`capture`/`ready` renew
it. Guarded commands need both `--lease` and `--token`; prefer
`HEDDLE_RESERVATION_TOKEN` over `--token` when process-list visibility matters.

## Delegated scoped tokens — `heddle auth derive-agent`

Source of truth: `.agents/agent-attenuation.md`,
[`docs/SELF_SOVEREIGN_AUTH.md`](../../../docs/SELF_SOVEREIGN_AUTH.md). (Ships in the
`client`-feature build.)

Spawn a sub-agent with **strictly narrower** authority than yours, with no
server round trip: you append an attenuation block to your own Biscuit. A child
block can only ever narrow authority, never widen it, and the server validates
the whole chain on the child's first call.

```bash
heddle auth derive-agent \
  --server grpc.heddle.sh \
  --agent-id review-worker \
  --ttl 3600 \
  --scope repo:acme/heddle \
  --allow Push --allow GetState
```

- Without `--allow`, a curated safe set is installed (push/pull, repo reads,
  context, discussions, `WhoAmI`). Repeating `--allow` selects a **subset**; it
  cannot opt into an unsafe method. A mandatory deny policy always rejects
  credential issuance, auth-trust mutation, recovery enrollment, and repo/
  namespace deletion.
- By default the child **replaces** the active stored credential for `--server`.
  Use `--out <DIR>` to write a portable 0600 bundle (`token`, `device-key.pem`,
  `metadata.json`) for handing to another process. Token-only `--stdout` export
  is intentionally unsupported (the bearer could not satisfy its proof binding).

## Hosted flow — adopt / remote / push

Source of truth: `heddle help remotes`, `heddle help git-overlay`,
`.agents/hosted-operations.md`.

```bash
heddle clone <url> <dir>              # clone a hosted or Git repo
# or, for an existing checkout:
heddle init                           # native metadata; Git commits stay in .git
heddle adopt --ref <branch>           # convert Git history into Heddle-native storage
heddle remote add origin <url-or-path>
heddle remote set-default origin
heddle push                           # → default remote (git refs + heddle refs, network_io)
heddle pull
heddle verify                         # confirm repo state before/after remote ops
```

`push`/`pull`/`fetch` use the default remote unless you pass a remote name.
`push` declares exit codes **75** (unreachable — retry), **76** (rejected —
don't retry), **78** (no upstream). Git-overlay repos default to a fast
git-mirror push; native adoption (`adopt`) is an opt-in enhancement, not a
prerequisite. When a remote action is unsafe, Heddle reports the blocker and one
primary next command instead of falling back to raw Git.
