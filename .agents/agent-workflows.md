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

## Harness And Actor Model

When a supported coding harness is involved, prefer the Heddle-native mental model:

- thread = the work context
- actor = the active worker
- session = the provenance record for that actor

Do not frame Heddle as a wrapper that should execute the harness. Heddle should follow the harness ambiently when possible.

Current shipped surfaces:

```bash
heddle actor list
heddle actor show
heddle actor explain
heddle integration install claude-code
heddle integration doctor
```

Important current behavior:

- `heddle actor explain` is the first place to inspect why Heddle attached activity to a given actor/session
- `heddle integration install` is the explicit opt-in path for supported harnesses on an existing repo
- `heddle integration relay` are internal plumbing surfaces, not the main user story

## Multi-Agent Isolation

For the common agent flow, prefer:

```bash
heddle start feature/auth --task "Implement auth middleware"
heddle capture -m "Implement auth middleware"
heddle merge feature/auth --preview
```

Important current behavior:

- `heddle start` defaults to a private lightweight thread with a Heddle-managed execution root.
- `heddle thread show/list/refresh/promote/drop` manage thread lifecycle and maintenance.
- `heddle start <thread> --path <dir>` creates a user-visible isolated checkout when the thread needs its own working directory.
- `capture` records changed paths, impact categories, freshness, and promotion warnings for the active thread.
- `merge --preview` shows the semantic integration summary before apply.
- `start <thread> --path <dir>` is the canonical isolated-checkout path.
- `actor spawn` creates a thread-linked actor registry entry only; it does not create filesystem isolation.
- `actor spawn` is for explicit Heddle actors; ambient harness integration may create actors automatically.
- use `start <thread> --path <dir>` when you need a real isolated checkout.

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
