# Heddle command catalog

Generated from `heddle help --output json` (the machine-readable command
catalog). Do not hand-edit — regenerate when the CLI changes:

```bash
heddle help --output json | jq -r '
  .commands[]
  | select(.tier!="hidden") | select(.has_subcommands==false)
  # exclude retired/uncontracted families that `heddle doctor docs` rejects in docs:
  | select((.surface|IN("internal","git_projection"))|not)
  | select((.path[0]|IN("session","presence","actor","checkpoint","cherry-pick","clean","fetch","git-overlay","merge","rebase","spool","stash","support","switch"))|not)
  | "| \`heddle \(.path|join(" "))\` | \(if .mutates then "mut" else "ro" end) | \(if .supports_json then "y" else "-" end) | \(if .supports_op_id then .op_id_behavior else "-" end) | \(.summary // "") |"'
```

Snapshot: heddle 0.10.0 (catalog generated 2026-07-19). The `cargo` version IS the
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
> `client`-feature build and are not present in a stripped 0.10.0 catalog.
> Confirm against your local `heddle auth --help`.

## Everyday commands

The curated native loop (`heddle help`). Start here.

| Command | RW | JSON | op-id | Summary |
|---|---|---|---|---|
| `heddle init` | mut | y | explicit_replay | Initialize a new Heddle repository |
| `heddle adopt` | mut | y | explicit_replay | Convert Git history into Heddle-native storage |
| `heddle status` | ro | y | - | Show what needs attention and the next safe Heddle action |
| `heddle verify` | ro | y | - | Verify this workspace; exits nonzero until every check is clean |
| `heddle start` | mut | y | explicit_replay | Create or resume an isolated thread for focused work |
| `heddle land` | mut | y | explicit_replay | Land a ready thread and optionally publish it |
| `heddle ready` | mut | y | explicit_replay | Prepare this thread for review or merge |
| `heddle commit` | mut | y | explicit_replay | Save current work as one Heddle change, plus a Git checkpoint in Git-overlay repos |
| `heddle log` | ro | y | - | Show state history |
| `heddle show` | ro | y | - | Show state details |
| `heddle diff` | ro | y | - | Show what changed in the worktree, a thread, or two states |
| `heddle undo` | mut | y | explicit_replay | Undo the last Heddle operation |
| `heddle resolve` | mut | y | explicit_replay | Resolve merge conflicts |
| `heddle push` | mut | y | explicit_replay | Push to a remote repository |
| `heddle pull` | mut | y | explicit_replay | Pull from a remote repository |
| clone | mut | y | explicit_replay | Clone from remote |

## Advanced commands

Power surfaces, automation, Git interop, recovery (`heddle help advanced`).
Reach for these when the everyday loop is not enough.

| Command | RW | JSON | op-id | Summary |
|---|---|---|---|---|
| `heddle help` | ro | y | - | Curated, progressive-disclosure help |
| `heddle watch` | ro | y | - | Stream live oplog activity |
| `heddle doctor docs` | ro | y | - | Diff-check markdown documentation against the actual CLI surface |
| `heddle doctor schemas` | ro | y | - | Drift-check `docs/json-schemas.md` against the registered schemas |
| `heddle schemas` | ro | y | - | Print the JSON Schema for a `--output json`-emitting verb |
| `heddle try` | mut | y | explicit_replay | Run a command in a sandboxed ephemeral thread |
| `heddle run` | mut | - | - | Run a command inside an existing thread's execution root |
| `heddle sync git` | mut | y | explicit_replay | Bidirectional sync with Git (export + import) |
| `heddle continue` | mut | y | explicit_replay | Continue the active operation without remembering the specific subcommand |
| `heddle abort` | mut | y | explicit_replay | Abort the active operation without remembering the specific subcommand |
| `heddle capture` | mut | y | explicit_replay | Capture a recoverable Heddle step for undo, provenance, and review |
| `heddle timeline status` | ro | y | - | Show the current timeline cursor, counts, and recovery status |
| `heddle timeline record-start` | mut | y | explicit_replay | Record the start of a native tool timeline step |
| `heddle timeline record-finish` | mut | y | explicit_replay | Record the finish of a native tool timeline step |
| `heddle timeline fork` | mut | y | explicit_replay | Fork a timeline branch from a step or native harness tool call |
| `heddle timeline reset` | mut | y | explicit_replay | Reset the logical timeline cursor, optionally materializing checkout files |
| `heddle timeline recover` | mut | y | explicit_replay | Recover a pending timeline materialization after an interrupted reset/seek |
| `heddle retro` | ro | y | - | Summarize a working session |
| `heddle discuss open` | mut | y | explicit_replay | Open a new discussion anchored to a symbol |
| `heddle discuss append` | mut | y | explicit_replay | Append a turn to an existing discussion |
| `heddle discuss resolve` | mut | y | explicit_replay | Resolve a discussion by edit, dismissal, or context annotation |
| `heddle discuss list` | ro | y | - | List discussions on a state, symbol, or by status |
| `heddle discuss show` | ro | y | - | Show a single discussion |
| `heddle query` | ro | y | - | Structured query over the operation log. Filter by actor, time window, signal kind, symbol, thread, verbs. Returns structured results consumable by agents |
| `heddle review show` | ro | y | - | Render the review payload for a state |
| `heddle review sign` | mut | y | explicit_replay | Submit a review signature on a state |
| `heddle review next` | ro | y | - | Walk to the next pending review when review selection is configured |
| `heddle review health` | ro | y | - | Per-module signal health over a rolling window |
| `heddle redact apply` | mut | y | explicit_replay | Declare a redaction on a blob in a state. The blob bytes stay on disk; reads return the stub. Use `heddle redact purge` afterward to physically remove the bytes |
| `heddle redact list` | ro | y | - | List every active redaction in the repo |
| `heddle redact show` | ro | y | - | Show a single redaction by its content-addressed id |
| `heddle redact trust add` | mut | y | explicit_replay | Add an operator public key to `[redact] trusted_keys` in `.heddle/config.toml`. Subsequent fetch/`clone` invocations will accept signed redactions from that key |
| `heddle redact trust list` | ro | y | - | List the currently-trusted operator keys |
| `heddle redact trust remove` | mut | y | explicit_replay | Remove an operator public key from the trust list. Future signed redactions from that key will be refused |
| `heddle redact purge apply` | mut | y | explicit_replay | Physically remove the blob bytes referenced by an existing redaction. Refuses if no redaction declared the blob first |
| `heddle redact purge list` | ro | y | - | List every `Purge` oplog entry — who removed bytes, when, and which redaction the purge acted on |
| `heddle visibility set` | mut | y | explicit_replay | Declare a visibility tier on a state. Public is the default and stays record-free (absence ≡ public); a non-public tier writes a per-state sidecar record and an oplog audit entry |
| `heddle visibility promote` | mut | y | explicit_replay | Promote a state to a less-restrictive tier by appending a superseding record. Requires an existing visibility record to supersede |
| `heddle visibility show` | ro | y | - | Show a state's effective visibility tier |
| `heddle visibility list` | ro | y | - | List every state that carries a non-public visibility tier |
| `heddle revert` | mut | y | explicit_replay | Revert changes from a state |
| `heddle collapse` | mut | y | explicit_replay | Collapse (squash) multiple states into one |
| `heddle expand` | ro | y | - | Expand a squashed land into the captures it collapsed |
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
| `heddle thread cleanup` | mut | y | explicit_replay | Sweep merged or stale auto-created threads |
| `heddle thread marker list` | ro | y | - | List markers, optionally filtered by name prefix |
| `heddle thread marker create` | mut | y | explicit_replay | Create marker at current state |
| `heddle thread marker delete` | mut | y | explicit_replay | Delete marker(s) |
| `heddle thread marker show` | ro | y | - | Show marker details |
| `heddle shell init` | ro | - | - | Emit a shell wrapper function on stdout. Source it from your shell rc to make `heddle start`, `heddle thread switch`, and `heddle thread cd` auto-`cd` into the target thread's worktree |
| `heddle shell completion` | ro | - | - | Generate a shell completion script on stdout |
| `heddle shell prompt` | ro | - | - | Print a compact prompt segment: current thread, dirty marker, and remote divergence. Intended for PS1 helpers |
| `heddle fsck` | mut | y | - | Verify repository integrity |
| `heddle oplog recover` | mut | y | - | Salvage a truncated or torn operation log and report what was recovered |
| `heddle import git` | mut | y | explicit_replay | Import Git commits to Heddle |
| `heddle export git` | mut | y | explicit_replay | Export Heddle states to Git |
| `heddle remote list` | ro | y | - | List configured remotes |
| `heddle remote add` | mut | y | explicit_replay | Add a remote |
| `heddle remote remove` | mut | y | explicit_replay | Remove a remote |
| `heddle remote set-default` | mut | y | explicit_replay | Set the default remote for pull, push, fetch, and Git projection operations |
| `heddle remote show` | ro | y | - | Show remote details |
| `heddle auth login` | mut | - | - | Authenticate with a Heddle server |
| `heddle auth logout` | mut | y | - | Remove stored credentials for a server |
| `heddle auth status` | ro | y | - | Show current authentication status |
| `heddle auth create-service-token` | mut | y | - | Create a service token for CI/scripts, scoped to a namespace |
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
| `heddle context reason git` | mut | y | explicit_replay | Mine Git-agent transcripts and attach reasoning as context annotations |
| `heddle integration list` | ro | y | - | List Heddle-managed harness integrations |
| `heddle integration install` | mut | y | explicit_replay | Install harness integrations |
| `heddle integration doctor` | ro | y | - | Verify installed harness integrations |
| `heddle integration uninstall` | mut | y | explicit_replay | Uninstall harness integrations |
| `heddle integration upgrade` | mut | y | explicit_replay | Rewrite Heddle-managed integrations in place |
| `heddle semantic hot` | ro | y | - | Aggregate semantic-change events across recent history and surface the files or functions with the most activity |
| `heddle daemon serve` | mut | y | - | Run a foreground mount daemon for this repository |
| `heddle daemon status` | ro | y | - | Report daemon liveness, version, uptime, and active mount count. No-op success when the daemon isn't running |
| `heddle daemon stop` | mut | y | - | Ask the running daemon to drain its mounts and exit. Sweeps any leftover registry entries with `fusermount -u` as a safety net before returning |
| `heddle agent serve` | mut | y | - | Run the local agent gRPC daemon |
| `heddle agent status` | ro | y | - | Report whether the local agent daemon is running for this repo |
| `heddle agent stop` | mut | y | - | Ask the running daemon to drain and exit |
| `heddle agent reserve` | mut | y | explicit_replay | Atomically reserve a thread for one writer |
| `heddle agent heartbeat` | mut | y | explicit_replay | Update reservation heartbeat |
| `heddle agent capture` | mut | y | explicit_replay | Capture under a session-validated reservation |
| `heddle agent ready` | mut | y | explicit_replay | Mark a reservation's thread ready for integration |
| `heddle agent release` | mut | y | explicit_replay | Release a reservation (status: complete | abandoned) |
| `heddle agent list` | ro | y | - | List agent reservations (optionally filtered to alive ones) |
| `heddle agent task create` | mut | y | explicit_replay | Create a local agent task assignment |
| `heddle agent task list` | ro | y | - | List local agent task assignments |
| `heddle agent task show` | ro | y | - | Show one local agent task assignment |
| `heddle agent task update` | mut | y | explicit_replay | Update one local agent task assignment |
| `heddle agent fanout plan` | ro | y | - | Preview fan-out lane setup and return commands without writing |
| `heddle agent fanout start` | mut | y | explicit_replay | Create task assignments and materialized child lanes |
| `heddle maintenance inspect` | ro | y | - | Inspect repository performance sidecars and repo shape |
| `heddle maintenance gc` | mut | y | explicit_replay | Garbage collect unreachable objects |
| `heddle hook list` | ro | y | - | List installed hooks |
| `heddle hook install` | mut | y | explicit_replay | Install a hook |
| `heddle hook uninstall` | mut | y | explicit_replay | Uninstall a hook |
| `heddle hook events` | ro | y | - | Show the hook event catalog (W2/A15) |
