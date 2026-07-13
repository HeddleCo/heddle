# Agent Workflows

## CLI Output

- Prefer `--output json` for automation.
- Treat text output as human-facing and unstable.
- Text is the default even when stdout is piped; pass `--output json` or `--output json-compact` explicitly.

## Attribution

Set these when an agent is making changes:

```bash
export HEDDLE_AGENT_PROVIDER="anthropic"
export HEDDLE_AGENT_MODEL="claude-opus-4-5-20250120"
export HEDDLE_PRINCIPAL_NAME="Your Name"
export HEDDLE_PRINCIPAL_EMAIL="you@example.com"
```

Do not rely on `HEDDLE_SESSION_ID` or `HEDDLE_SESSION_SEGMENT`; they are not implemented.

## Recommended Automation Flows

Use Heddle as a JSON-speaking CLI:

```bash
heddle status --output json
heddle log --output json
heddle diff --output json
heddle show HEAD --output json
```

Common write flow:

```bash
heddle capture -m "Implement feature X"
heddle diff --semantic --output json
heddle query --attribution src/file.rs --output json
```

## Harness And Agent Model

When a supported coding harness is involved, prefer the Heddle-native mental model:

- thread = the work context
- presence = attribution and work context for the active worker
- provenance session = provider/model/policy epochs for that worker
- task = the local delegation record
- writer lease = exclusive, token-authenticated write authority for a thread

Do not frame Heddle as a wrapper that should execute the harness. Heddle should follow the harness ambiently when possible.

Current shipped surfaces:

```bash
heddle agent presence list
heddle agent presence show
heddle agent presence explain
heddle integration install claude-code
heddle integration doctor
```

Important current behavior:

- `heddle agent presence explain` is the first place to inspect why Heddle attached activity to an agent
- `heddle integration install` is the explicit opt-in path for supported harnesses on an existing repo
- `heddle integration relay` are internal plumbing surfaces, not the main user story

## Multi-Agent Isolation

For the common agent flow, prefer:

```bash
heddle start feature/auth --task "Implement auth middleware"
heddle agent reserve --thread feature/auth
heddle agent heartbeat --lease <LEASE> --token <TOKEN>
heddle agent capture --lease <LEASE> --token <TOKEN> -m "Implement auth middleware"
heddle agent ready --lease <LEASE> --token <TOKEN>
heddle agent release --lease <LEASE> --token <TOKEN>
heddle land --thread feature/auth
```

Important current behavior:

- `heddle start` defaults to a private lightweight thread with a Heddle-managed execution root.
- `heddle thread show/list/refresh/promote/drop` manage thread lifecycle and maintenance.
- `heddle start <thread> --path <dir>` creates a user-visible isolated checkout when the thread needs its own working directory.
- `capture` records changed paths, impact categories, freshness, and promotion warnings for the active thread.
- `ready` shows the semantic integration summary before `land`.
- `start <thread> --path <dir>` is the canonical isolated-checkout path.
- use `start <thread> --path <dir>` when you need a real isolated checkout.

## Reservation Liveness

Agent reservations use a five-minute heartbeat lease. `agent heartbeat`,
`agent capture`, and `agent ready` renew a current lease. `--hold-for-pid`
adds an early-death signal for a long-lived orchestrator process; PID liveness
does not replace heartbeats.

`agent reserve` returns a `lease_id` and bearer `token`. `heartbeat`, `capture`,
`ready`, and `release` require both. Guarded commands
require both. Prefer `HEDDLE_RESERVATION_TOKEN` over `--token` when process-list
visibility matters. Actor session IDs identify provenance; they do not grant
writer authority.

## Harness Integration Install

Supported harnesses in this branch:

- `codex`
- `claude-code`
- `opencode`

Preferred install flow:

```bash
heddle integration install claude-code --scope repo
heddle integration install opencode --scope repo
heddle integration install codex --scope user
heddle integration doctor
```

Notes:

- `heddle init` can offer the same install step interactively, but it remains optional
- repo-local install is preferred when the harness supports it
- Codex currently uses a user-scope `notify` install path
- Claude Code uses hooks and an optional Heddle-owned status line command
- OpenCode uses a Heddle-managed plugin file plus a
  `heddle.timeline.json` capability manifest. The manifest advertises the
  shipped timeline commands agents and desktop integrations can call:
  `log --timeline`, `timeline fork`, `timeline reset`, and
  `timeline recover`.
- `heddle integration list --output json` and
  `heddle integration doctor --output json` expose `capabilities` and
  `capability_paths` for installed integrations.
- The OpenCode plugin currently relays events. Native OpenCode tool
  registration should be added only after the OpenCode plugin tool API is
  verified; until then, use the capability manifest or TS SDK helpers.

## Current Caveats

- `heddle undo` is thread-local: it only rewinds operations recorded from the current checkout.
- Use `heddle undo` from the specific isolated checkout you want to rewind.
- If automation needs hosted admin behavior, prefer the dedicated hosted commands and HTTP admin API rather than parsing server logs.
