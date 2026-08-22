# Heddle command catalog

Generated from `heddle help --output json` (the machine-readable command
catalog). Do not hand-edit — regenerate when the CLI changes:

```bash
heddle help --output json | jq -r '
  .commands[]
  | select((.help_visibility|IN("hidden","everyday"))|not) | select(.has_subcommands==false)
  # exclude internal/uncontracted surfaces that `heddle doctor docs` rejects in docs:
  | select((.surface|IN("internal","git_projection"))|not)
  | "| \`heddle \(.path|join(" "))\` | \(if .mutates then "mut" else "ro" end) | \(if .supports_json then "y" else "-" end) | \(if .supports_op_id then .op_id_behavior else "-" end) | \(.summary // "") |"'
```

Snapshot: heddle 0.13.0 (catalog regenerated 2026-08-22 after the surface consolidation folds/culls). The `cargo` version IS the
JSON contract version — pin a `heddle-cli` constraint and these shapes are
stable for that minor (see `exit-codes.md` › Schema/contract stability).

Columns:

- **RW** — `mut` mutates state, `ro` read-only.
- **JSON** — `y` if the verb emits the full machine contract under
  `--output json` / `--output json-compact` (stable `output_kind`). A `-`
  means text-only; requesting JSON there exits **65** (DataErr) — fall back to a
  supported `--output` mode.
- **op-id** — idempotency mode (`explicit_replay` = pass `--op-id <UUID>` to
  make retries safe; `-` = rejects `--op-id`). See `heddle help operation-ids`.

Commands marked `hidden` in the catalog (`complete`, `transaction`, some
`spool`/relay plumbing) are intentionally omitted; they are internal surfaces.

> Client-feature commands: some self-sovereign auth verbs (notably
> `heddle auth derive-agent`, see SKILL.md § Delegated tokens) ship in the
> `client`-feature build and are not present in a stripped 0.13.0 catalog.
> Confirm against your local `heddle auth --help`.

## Everyday commands

The curated native loop (`heddle help`). Start here.

| Command | RW | JSON | op-id | Summary |
|---|---|---|---|---|
| `heddle status` | ro | y | - | Show what needs attention and the next safe Heddle action |
| `heddle diff` | ro | y | - | Show what changed in the worktree, a thread, or two states |
| `heddle commit` | mut | y | explicit_replay | Write captured source history to `.git` in Git Overlay |
| `heddle capture` | mut | y | explicit_replay | Capture a recoverable Heddle step for undo, provenance, and review |
| `heddle start` | mut | y | explicit_replay | Create or resume an isolated thread for focused work |
| `heddle ready` | mut | y | explicit_replay | Prepare this thread for review or merge |
| `heddle land` | mut | y | explicit_replay | Integrate a ready thread into its local target |
| `heddle push` | mut | y | explicit_replay | Push the source-authoritative history to a remote |
| `heddle pull` | mut | y | explicit_replay | Pull source-authoritative history from a remote |
| `heddle continue` | mut | y | explicit_replay | Continue the active operation without remembering the specific subcommand |
| `heddle undo` | mut | y | explicit_replay | Undo the last Heddle operation |
| `heddle verify` | ro | y | - | Verify this workspace; exits nonzero until every check is clean |
| `heddle log` | ro | y | - | Show state history |
| `heddle show` | ro | y | - | Show state details |
| `heddle query` | ro | y | - | Structured query over the operation log. Filter by actor, time window, signal kind, symbol, thread, verbs. Returns structured results consumable by agents |
| `heddle whoami` | ro | y | - | Report the capture actor, then hosted auth |
| `heddle init` | mut | y | explicit_replay | Initialize Heddle in a directory or existing Git checkout |
| `heddle completions` | ro | - | - | Print a tab-completion script for bash, zsh, or fish |
| `heddle adopt` | mut | y | explicit_replay | Adopt Git history into Heddle-native source authority |
| `heddle clone` | mut | y | explicit_replay | Clone from remote |
| `heddle help` | ro | y | - | Curated, progressive-disclosure help |
| `heddle resolve` | mut | y | explicit_replay | Resolve merge conflicts |

## Advanced commands


Power surfaces, automation, Git interop, recovery — everything beyond the everyday loop,
ranked by `heddle help` order.

| `heddle watch` | ro | y | - | Stream live oplog activity |
| `heddle doctor docs` | ro | y | - | Diff-check markdown documentation against the actual CLI surface |
| `heddle doctor schemas` | ro | y | - | Drift-check `docs/json-schemas.md` against the registered schemas |
| `heddle abort` | mut | y | explicit_replay | Abort the active operation without remembering the specific subcommand |
| `heddle discuss open` | mut | y | explicit_replay | Open a discussion anchored to a symbol |
| `heddle discuss append` | mut | y | explicit_replay | Append a durable turn to a discussion |
| `heddle discuss resolve` | mut | y | explicit_replay | Resolve a discussion |
| `heddle discuss reopen` | mut | y | explicit_replay | Reopen a resolved discussion |
| `heddle discuss list` | ro | y | - | List repository discussions |
| `heddle discuss show` | ro | y | - | Show one discussion and its causal heads |
| `heddle review show` | ro | y | - | Render the review payload for a state |
| `heddle review sign` | mut | y | explicit_replay | Submit a review signature on a state |
| `heddle review next` | ro | y | - | Walk to the next pending review when review selection is configured |
| `heddle review health` | ro | y | - | Per-module signal health over a rolling window |
| `heddle redact apply` | mut | y | explicit_replay | Declare a redaction on a blob in a state. The blob bytes stay on disk; reads return the stub. Use `heddle redact purge` afterward to physically remove the bytes |
| `heddle redact list` | ro | y | - | List every active redaction in the repo |
| `heddle redact show` | ro | y | - | Show a single redaction by its content-addressed id |
| `heddle redact purge apply` | mut | y | explicit_replay | Physically remove the blob bytes referenced by an existing redaction. Refuses if no redaction declared the blob first |
| `heddle redact purge list` | ro | y | - | List every `Purge` oplog entry — who removed bytes, when, and which redaction the purge acted on |
| `heddle visibility set` | mut | y | explicit_replay | Declare a visibility tier on a state. Public is the default and stays record-free (absence ≡ public); a non-public tier writes a per-state sidecar record and an oplog audit entry |
| `heddle visibility promote` | mut | y | explicit_replay | Promote a state to a less-restrictive tier by appending a superseding record. Requires an existing visibility record to supersede |
| `heddle visibility show` | ro | y | - | Show a state's effective visibility tier |
| `heddle visibility list` | ro | y | - | List every state that carries a non-public visibility tier |
| `heddle revert` | mut | y | explicit_replay | Revert changes from a state |
| `heddle thread create` | mut | y | explicit_replay | Create a thread ref at the current state |
| `heddle thread current` | ro | y | - | Print the name of the current thread (the thread the working checkout is attached to). Read-only — no state change. Useful in shell pipelines: `cd "$(heddle thread cd "$(heddle thread current)")"` |
| `heddle thread switch` | mut | y | explicit_replay | Switch the current checkout to an existing thread ref |
| `heddle thread cd` | ro | - | - | Print the on-disk path for a thread. Read-only — no state change, no auto-capture. Pair with the shell hook (`heddle shell init`) to land in the right directory: eval "$(heddle thread cd X)" Or use the shell function directly: `heddle thread cd X` becomes `cd <path>` when the hook is installed |
| `heddle thread list` | ro | y | - | List threads |
| `heddle thread show` | ro | y | - | Show one thread with actor and workflow context |
| `heddle thread captures` | ro | y | - | Show granular captures on a thread |
| `heddle thread rename` | mut | y | explicit_replay | Rename a thread ref |
| `heddle thread refresh` | mut | y | explicit_replay | Refresh a thread onto its target thread |
| `heddle thread move` | mut | y | explicit_replay | Move selected captured paths from one thread into another |
| `heddle thread absorb` | mut | y | explicit_replay | Absorb a child thread into its parent or another thread |
| `heddle thread resolve` | mut | y | explicit_replay | Guide a blocked or stale thread toward its next clean state |
| `heddle thread promote` | mut | y | explicit_replay | Materialize an existing thread ref at a chosen path |
| `heddle thread drop` | mut | y | explicit_replay | Drop a thread and mark it abandoned |
| `heddle thread approve` | mut | y | explicit_replay | Record a merge approval for `<source> -> <target>` |
| `heddle thread approvals` | ro | y | - | List approvals recorded for `<source> -> <target>` |
| `heddle thread revoke-approval` | mut | y | explicit_replay | Revoke a previously recorded approval by id |
| `heddle thread check-merge` | ro | y | - | Check whether `<source> -> <target>` would merge under the repo's branch-protection policies. Read-only |
| `heddle thread cleanup` | mut | y | explicit_replay | Sweep merged, stale auto-created, or abandoned threads |
| `heddle thread collapse` | mut | y | explicit_replay | Collapse (squash) multiple states into one |
| `heddle thread expand` | ro | y | - | Expand a squashed land into the captures it collapsed |
| `heddle thread marker list` | ro | y | - | List markers, optionally filtered by name prefix |
| `heddle thread marker create` | mut | y | explicit_replay | Create marker at current state |
| `heddle thread marker delete` | mut | y | explicit_replay | Delete marker(s) |
| `heddle thread marker show` | ro | y | - | Show marker details |
| `heddle shell init` | ro | - | - | Emit a shell wrapper function on stdout. Source it from your shell rc to make `heddle start`, `heddle thread switch`, and `heddle thread cd` auto-`cd` into the target thread's worktree |
| `heddle shell completion` | ro | - | - | Generate a shell completion script on stdout |
| `heddle shell prompt` | ro | - | - | Print a compact prompt segment: current thread, dirty marker, and remote divergence. Intended for PS1 helpers |
| `heddle bridge git import` | mut | y | explicit_replay | Import Git commits to Heddle without changing source authority |
| `heddle bridge git export` | mut | y | explicit_replay | Export Heddle states to Git |
| `heddle remote list` | ro | y | - | List configured remotes |
| `heddle remote add` | mut | y | explicit_replay | Add a remote |
| `heddle remote remove` | mut | y | explicit_replay | Remove a remote |
| `heddle remote set-default` | mut | y | explicit_replay | Set the default Heddle remote for pull and push |
| `heddle remote show` | ro | y | - | Show remote details |
| `heddle auth login` | mut | - | - | Authenticate with a Heddle server |
| `heddle auth logout` | mut | y | - | Remove stored credentials for a server |
| `heddle auth status` | ro | y | - | Show current authentication status |
| `heddle auth trust show` | ro | y | - | Show the descriptor trust controlling a server connection |
| `heddle auth trust replace` | mut | y | - | Atomically replace an automatic descriptor trust pin |
| `heddle auth derive-agent` | mut | - | - | Derive a scoped, short-lived agent token offline |
| `heddle auth create-service-token` | mut | y | - | Create a service token for CI/scripts, scoped to a namespace |
| `heddle identity ensure` | mut | y | explicit_replay | Ensure this machine has an agent identity, reusing an account first |
| `heddle identity claim-link` | mut | y | - | Reissue the short-lived browser claim link for an unclaimed identity |
| `heddle identity serve` | mut | - | - | Keep the Iroh claim endpoint online for an outstanding link |
| `heddle context set` | mut | y | explicit_replay | Attach a context annotation to a file, symbol, line range, or state |
| `heddle context get` | ro | y | - | Show current context annotations for a file or state target |
| `heddle context list` | ro | y | - | List all active context targets |
| `heddle context history` | ro | y | - | Show full revision history for one logical annotation |
| `heddle context edit` | mut | y | explicit_replay | Add a new revision to an existing logical annotation |
| `heddle context supersede` | mut | y | explicit_replay | Create a replacement logical annotation and supersede an older one |
| `heddle context rm` | mut | y | explicit_replay | Remove context annotations |
| `heddle context check` | ro | y | - | Check annotation staleness against current code |
| `heddle context suggest` | ro | y | - | Suggest low-noise targets that may benefit from context |
| `heddle context audit` | ro | y | - | Audit stale, superseded, and duplicate context |
| `heddle integration list` | ro | y | - | List Heddle-managed harness integrations |
| `heddle integration install` | mut | y | explicit_replay | Install harness integrations |
| `heddle integration doctor` | ro | y | - | Verify installed harness integrations |
| `heddle integration uninstall` | mut | y | explicit_replay | Uninstall harness integrations |
| `heddle integration upgrade` | mut | y | explicit_replay | Rewrite Heddle-managed integrations in place |
| `heddle semantic diff` | ro | y | - | Compare the symbols stored in two states' attached semantic indexes |
| `heddle semantic hot` | ro | y | - | Aggregate semantic-change events across recent history and surface the files or functions with the most activity |
| `heddle semantic refs` | ro | y | - | Query persisted refs-of, callers-of, or importers-of at a state |
| `heddle semantic index` | mut | - | - | Backfill the content-addressed merkle semantic index over history |
| `heddle daemon serve` | mut | y | - | Run a foreground mount daemon for this repository |
| `heddle daemon status` | ro | y | - | Report daemon liveness, version, uptime, and active mount count. No-op success when the daemon isn't running |
| `heddle daemon stop` | mut | y | - | Ask the running daemon to drain its mounts and exit. Sweeps any leftover registry entries with `fusermount -u` as a safety net before returning |
| `heddle agent reserve` | mut | y | explicit_replay | Atomically reserve a thread for one writer |
| `heddle agent heartbeat` | mut | y | explicit_replay | Update reservation heartbeat |
| `heddle agent capture` | mut | y | explicit_replay | Capture under a token-authenticated writer lease |
| `heddle agent ready` | mut | y | explicit_replay | Mark a reservation's thread ready for integration |
| `heddle agent release` | mut | y | explicit_replay | Release a reservation (status: complete | abandoned) |
| `heddle agent list` | ro | y | - | List agent reservations (optionally filtered to alive ones) |
| `heddle agent task create` | mut | y | explicit_replay | Create a local agent task assignment |
| `heddle agent task list` | ro | y | - | List local agent task assignments |
| `heddle agent task show` | ro | y | - | Show one local agent task assignment |
| `heddle agent task update` | mut | y | explicit_replay | Update one local agent task assignment |
| `heddle agent fanout plan` | ro | y | - | Preview fan-out lane setup and return commands without writing |
| `heddle agent fanout start` | mut | y | explicit_replay | Create task assignments and materialized child lanes |
| `heddle agent provenance begin` | mut | y | explicit_replay | Begin a provider/model provenance session |
| `heddle agent provenance segment` | mut | y | explicit_replay | Record a provider, model, or policy change within the current session |
| `heddle agent provenance end` | mut | y | explicit_replay | End the current or selected provenance session |
| `heddle agent provenance show` | ro | y | - | Show the current or selected provenance session |
| `heddle agent provenance list` | ro | y | - | List provenance sessions |
| `heddle agent presence list` | ro | y | - | List agent presence records known to this repository |
| `heddle agent presence show` | ro | y | - | Show the current or selected agent presence record |
| `heddle agent presence explain` | ro | y | - | Explain why Heddle attached the current or selected presence record |
| `heddle agent presence complete` | mut | y | explicit_replay | Mark the current or selected presence record complete |
| `heddle agent timeline status` | ro | y | - | Show the current timeline cursor, counts, and recovery status |
| `heddle agent timeline record-start` | mut | y | explicit_replay | Record the start of a native tool timeline step |
| `heddle agent timeline record-finish` | mut | y | explicit_replay | Record the finish of a native tool timeline step |
| `heddle agent timeline fork` | mut | y | explicit_replay | Fork a timeline branch from a step or native harness tool call |
| `heddle agent timeline reset` | mut | y | explicit_replay | Reset the logical timeline cursor, optionally materializing checkout files |
| `heddle agent timeline recover` | mut | y | explicit_replay | Recover a pending timeline materialization after an interrupted reset/seek |
| `heddle maintenance fsck repair git` | mut | y | explicit_replay | Reconcile Git projection metadata or one projected ref |
| `heddle maintenance inspect` | ro | y | - | Inspect repository performance sidecars and repo shape |
| `heddle maintenance refresh` | mut | y | explicit_replay | Refresh repository performance sidecars without changing repository meaning |
| `heddle maintenance repack` | mut | y | explicit_replay | Repack native objects now through the resource-controlled scheduler |
| `heddle maintenance gc` | mut | y | explicit_replay | Garbage collect unreachable objects |
| `heddle maintenance oplog recover` | mut | y | - | Salvage a truncated or torn operation log and report what was recovered |
| `heddle hook list` | ro | y | - | List installed hooks |
| `heddle hook install` | mut | y | explicit_replay | Install a hook |
| `heddle hook uninstall` | mut | y | explicit_replay | Uninstall a hook |
| `heddle hook events` | ro | y | - | Show the hook event catalog (W2/A15) |
| `heddle ci` | ro | - | - |  |
| `heddle ci run` | mut | y | - |  |