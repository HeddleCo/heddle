# Heddle personas

This doc names the three personas heddle's OSS quality is evaluated against. Each round of "persona eval" walks current heddle `main` from each persona's perspective and reports friction; findings are filed as issues and prioritized across personas.

The three personas are NOT the only people who use heddle. They're chosen because their friction is the leading-indicator signal for OSS adoption + agent compatibility.

## Persona 1 — "Regular AI dev"

**Who:** a human developer who uses an AI pair (Claude Code, Cursor, Aider) to write code. They run heddle commands themselves; they read the output themselves; the AI helps them think but doesn't operate heddle directly.

**What surface they stress:**

- `heddle status` readability — they read it, decide what to do next.
- Diff output — they read changes the AI made.
- Error messages — they have to act on them.
- `--help` discoverability — when they need a new command they ask the CLI first.
- Default UX — they didn't read every flag; defaults matter.

**Friction worth flagging:** anything that makes the human pause, ask the AI "what does this mean?" or "is this normal?"

**Friction NOT worth flagging:** anything that's purely about machine-readable contracts (they don't parse JSON output by hand).

## Persona 2 — "Seasoned git veteran"

**Who:** a developer with deep git intuition (10+ years). They're evaluating heddle as a potential migration. They know `git rebase -i`, `git rev-list`, `git rerere`, `git worktree` cold.

**What surface they stress:**

- Migration cost — how much existing git knowledge transfers.
- Surprise — places heddle's model differs from git in ways they wouldn't predict.
- `bridge git import` / `git_compat` — the heddle/git interop surface.
- `--help` and topic docs — they want the canonical reference.
- "Why is this different from `git X`?" questions.

**Friction worth flagging:** anything that violates git's principle of least surprise without a clear payoff.

**Friction NOT worth flagging:** things that are intentionally different and document why (those are heddle's value prop).

## Persona 3 — "Agent"

**Who:** a headless invocation of heddle from an automated system. Could be Claude operating heddle as a tool, could be CI, could be a server hooking into heddle-grpc.

**What surface they stress:**

- `--output json` correctness — every field has stable shape.
- Exit codes — every documented exit value is correct + every error path maps to one.
- `output_kind` / dispatch discriminators — agents route on field, not output text.
- Dedup / op-id — agents retry; safety depends on heddle's cross-process correctness.
- Error envelopes — `kind` / `code` / `recommended_action` are machine-readable.
- Token economy — JSON payloads should not bloat. Agents pay per token.
- ContentService RPC surface — for FS-less invocation (see [#267](https://github.com/HeddleCo/heddle/issues/267)).

**Friction worth flagging:** anything that makes an agent guess, parse text, or retry blindly.

**Friction NOT worth flagging:** text-rendering cosmetics that don't affect JSON shape.

## Mapping past findings to personas

### Round 3 (filed as #252-#258)

| Issue | Persona |
|---|---|
| [#252](https://github.com/HeddleCo/heddle/issues/252) — exit codes reconcile | Agent |
| [#253](https://github.com/HeddleCo/heddle/issues/253) — status JSON token bloat | Agent |
| [#254](https://github.com/HeddleCo/heddle/issues/254) — advice triplet collapse | Agent |
| [#255](https://github.com/HeddleCo/heddle/issues/255) — init output_kind | Agent |
| [#256](https://github.com/HeddleCo/heddle/issues/256) — `--output auto` sweep | Regular AI dev |
| [#257](https://github.com/HeddleCo/heddle/issues/257) — clone `--help` docs | Git veteran / Regular AI dev |
| [#258](https://github.com/HeddleCo/heddle/issues/258) — thread drop hint | Regular AI dev |

### Round 4 (orchestrator scratch at `persona-eval-round-4.md`)

| Finding | Persona |
|---|---|
| S1 — `output_kind` sweep across 13+ verbs | Agent |
| S2 — error envelope `code` vs `kind` duplicate | Agent |
| S3 — triplet trap extends to `primary_command_*` | Agent |
| S4 — `heddle push` no-remote exit `1` | Agent |
| S5 — `recommended_action: ""` empty string | Agent (verify) |
| P1 — `--output auto` subhelp lines | Git veteran (verify) |
| P2 — `heddle init` welcome lacks next-step | Regular AI dev |
| P3 — `heddle help advanced` flat 40-cmd list | Git veteran / Regular AI dev |
| P4 — clone `--help` flag overflow | Git veteran / Regular AI dev |
| P5 — `bridge git import` lacks `--principal-*` symmetry | Git veteran |

## How to run a round

A persona-eval round is dispatched as a research agent (no code edits). The agent:

1. Reads this file to get persona definitions.
2. For each persona, walks current heddle `main` from that persona's perspective. Uses the actual code paths — reads command impls in `crates/cli/src/cli/commands/`.
3. Cross-references existing backlog (filed issues, in-flight PRs) to avoid duplicates.
4. Per-persona, surfaces 3-5 top friction points: short label, `file:line` where it manifests, severity, novelty.
5. Tiers findings: Tier 1 (file + dispatch next), Tier 2 (file, wait), Tier 3 (file but defer), Tier 4 (drop / verify).
6. Proposes a single highest-leverage Tier 1 to dispatch first.

Past rounds:

- Round 3 (2026-05-26) — filed [#252](https://github.com/HeddleCo/heddle/issues/252)-[#258](https://github.com/HeddleCo/heddle/issues/258).
- Round 4 (2026-05-27) — identified the missing "regular AI dev" persona and motivated this doc.
