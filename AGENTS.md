# AGENTS.md — Heddle agent guidelines

Project context, doc map, and operational guidance for agents working in this repo.

## Behavioral contract

The twelve behavioral rules below are the authoritative behavioral spec for this worktree. If a future checkout includes a root `CLAUDE.md`, read it as additional guidance, but do not block on it when it is absent.

If something goes wrong, the first triage step is which rule was violated:

| Mistake | Rule | Often shows up as |
|---|---|---|
| Silent wrong assumption | 1 (Think before coding) | Working from a guess about an API/browser behavior without verifying |
| Over-engineered solution | 2 (Simplicity first) | A 6-cell counter grid when one summary line was asked for |
| Touched code outside scope | 3 (Surgical changes) | "While I was in there" cleanup that breaks an adjacent feature |
| Drifted from success criteria | 4 (Goal-driven) | Loop terminated on a partial pass instead of the full criterion |
| Used the model where code would do | 5 (Judgment only) | Asked the model to decide a retry policy that a status code already answers |
| Overran budget silently | 6 (Token budgets) | 90-minute debug session with no summarization |
| Averaged contradictory patterns | 7 (Surface conflicts) | Two error-handling styles mixed in one function |
| Wrote without reading | 8 (Read before write) | Marketing CLI snippet with a flag that doesn't exist in `cli_args/` |
| Test covered shape, not intent | 9 (Tests verify intent) | Hardcoded return value that passes the assertion |
| No checkpoint mid-task | 10 (Checkpoint) | Step 4 of a 6-step refactor broken; steps 5+6 piled on top |
| Forked the convention | 11 (Match conventions) | Introduced `ease` into a codebase that uses `cubic-bezier(...)` |
| Claimed "done" without verifying | 12 (Fail loud) | "Mobile verified" without running Phase 4.5 at 375×812 |

## Confidence

When you `heddle capture`, set `--confidence` to your honest estimate (0.0–1.0) of how likely your change is correct end-to-end given what you tested: ≥0.9 only when build + tests + manual verification all passed cleanly, 0.75–0.89 when most signals passed but coverage is partial, below 0.75 when you're shipping a draft or have unresolved warnings.

## Compatibility

Heddle is still moving quickly. Prefer the current model over preserving legacy behavior.

- Do not add backwards-compatibility shims unless the user explicitly asks for them.
- When a model or API changes, it is acceptable to update callers, tests, and docs to the new behavior instead of keeping legacy support.

## Project Overview

Heddle is an AI-native version control system written in Rust. It combines content-addressed storage, immutable history with stable change identifiers, explicit human and agent attribution, hosted namespace/repository control-plane primitives, and an emerging web product for repository intelligence and operations.

**Key files:**
- `README.md` - Top-level product and capability overview
- `Cargo.toml` - Workspace configuration
- `crates/` - Primary Rust implementation
- `docs/` - Architecture, hosted model, roadmap, and future-state plans
- `specs/quint/` - Formal specifications and model-checking assets
- `web/` - Hosted web product

**Current Status:**
- All core VCS commands implemented
- 600+ tests passing (including formal spec and hosted integration coverage)
- Wire protocol for remote sync complete
- Git bridge implemented (bidirectional sync)
- Packed refs, packfiles, shallow clone, hooks, and crypto signing implemented
- Multi-agent parallel materialized threads implemented (`start --path`, `thread list/show/drop`, `actor spawn/list/done`)
- Hosted namespaces, repositories, grants, and content inspection APIs implemented in foundation form
- Web product in progress: marketing site (shipped), hosted inspection and admin surfaces (foundation), request-access funnel (shipped)
- GitHub App integration (`heddlebot`): PR semantic review summaries, webhook handling, OAuth login (foundation)
- Public review surface at `/review/:owner/:repo/pr/:number` with SSE streaming analysis (foundation)
- Compare/review UX on hosted provenance and hosted builds/workflows remain future-state

## Documentation Truth Rules

When editing docs, specs, or web copy, classify capabilities explicitly:

- **Shipped** - implemented and safe to describe as current behavior
- **Foundation in place** - partially implemented or structurally supported, but not yet a complete user-facing product surface
- **Planned** - clearly intended future-state documented in `docs/` or `web/PRODUCT_SPEC.md`

Do not describe a capability as live if it is only mock-backed in the web app or only planned in docs. Future-state positioning is encouraged when it is grounded in the codebase and roadmap, but it must be labeled accurately.

## Agent skills

### Issue tracker

Issues and PRDs are tracked in GitHub Issues for `HeddleCo/heddle`. See `docs/agents/issue-tracker.md`.

### Triage labels

Use the canonical triage labels, with `question` retained as an additional GitHub issue label for general questions. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context repo: use root `CONTEXT.md` and `docs/adr/` for domain language and ADRs, and read `AGENTS.md` plus relevant `.agents/*.md` files for operating guidance. See `docs/agents/domain.md`.

## Guidelines

| Topic | Description |
|-------|-------------|
| [[.agents/rust-guidelines\|Rust Guidelines]] | Code style, error handling, naming, documentation, dependencies |
| [[.agents/architecture\|Architecture]] | Directory structure, design patterns |
| [[.agents/common-tasks\|Common Tasks]] | Adding commands, object types, modifying spec |
| [[.agents/testing\|Testing]] | Running tests, test categories, checklist |
| [[.agents/formal-specs\|Formal Specs]] | Quint specifications, property tests, regression traces |
| [[.agents/commands\|Commands]] | Build, test, lint, run commands |
| [[.agents/code-review\|Code Review]] | Focus areas, review methodology, 7-step checklist |
| [[.agents/review-pitfalls\|Review Pitfalls]] | 10 concrete anti-patterns with examples; false-positive traps; severity calibration table |
| [[.agents/web-copy\|Web Copy]] | Copy principles, elite-status voice, rewrite patterns, key files, scoring rubric |
| [[.agents/hosted-operations\|Hosted Operations]] | Namespaces, grants, control plane, Biscuit, deployment |
| [[.agents/agent-workflows\|Agent Workflows]] | CLI output modes, attribution, multi-agent isolation |
| [[.agents/delta-compression\|Delta Compression]] | Pack format, sliding window, feature flags, known gaps |

## Environment Variables

```bash
# Agent attribution
export HEDDLE_AGENT_PROVIDER="anthropic"
export HEDDLE_AGENT_MODEL="claude-opus-4-7"

# Principal attribution
export HEDDLE_PRINCIPAL_NAME="Your Name"
export HEDDLE_PRINCIPAL_EMAIL="you@example.com"

# Development
export RUST_BACKTRACE=1
export RUST_LOG=heddle=debug
```

Note: `HEDDLE_SESSION_ID` and `HEDDLE_SESSION_SEGMENT` are **not implemented**. Do not set them.

## Known Limitations (v0.1)

- Reftable format not implemented (packed-refs works but degrades above ~10k refs)
- Git bridge may have edge cases with complex histories
- Semantic diff is available but may be conservative
- Partial clone (lazy object fetch) not yet supported
- Provenance-backed local blame and hosted provenance inspection are implemented; richer compare/review UX on top of provenance is still planned
- Hosted builds / workflows / artifact surfaces are planned, not implemented
- `heddle undo` is scoped to the current checkout lane; it only rewinds operations recorded from that checkout's HEAD path
- `heddle actor spawn` does not create filesystem isolation — it only creates a thread and registry entry; use `heddle start <name> --path <dir>` to get an isolated checkout with its own working directory
- `heddle actor spawn` has no `--from` flag; it always bases the new actor's thread on the current HEAD

## Quick Reference

- **Architecture**: `docs/ARCHITECTURE.md` - System design
- **Formal Specs**: `specs/quint/` - Quint models and spec notes
- **Hosted Model**: `docs/HOSTED_NAMESPACES.md` and `docs/HOSTED_ADMIN.md`
- **Roadmap**: `docs/ENTERPRISE_BACKEND_ROADMAP.md`, `docs/RUNNERS_AND_BUILDS.md`, `docs/LINE_PROVENANCE_PLAN.md`
- **Web Product**: `web/PRODUCT_SPEC.md`
- **Examples**: Test cases in `tests/` directory
