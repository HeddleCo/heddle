# Domain Docs

How the engineering skills should consume this repo's domain documentation and existing agent guidance when exploring the codebase.

## Layout

This is a single-context repo for the mattpocock engineering skills:

- **Domain glossary**: `CONTEXT.md` at the repo root.
- **Architecture decisions**: `docs/adr/`.
- **Repo operating guidance**: `AGENTS.md` and the relevant files under `.agents/`.

`CONTEXT.md` and `docs/adr/` are created lazily. If they do not exist, proceed silently unless the current work resolves a domain term or an architectural decision worth recording.

## Before Exploring

Read `AGENTS.md` first. Then read the `.agents/*.md` files relevant to the work area, such as:

- `.agents/architecture.md` for workspace layout, core patterns, hosted layers, web routes, packfiles, agent checkouts, and formal specs.
- `.agents/rust-guidelines.md` for Rust style, error handling, dependency, security, and performance rules.
- `.agents/common-tasks.md` for command, object, spec, state-machine, hosted, API, and web-page workflows.
- `.agents/testing.md` and `.agents/formal-specs.md` for test and Quint verification expectations.
- `.agents/hosted-operations.md` for hosted namespace, grant, content API, and deployment guidance.
- `.agents/agent-workflows.md` for thread, actor, session, checkout, and attribution guidance.
- `.agents/code-review.md` and `.agents/review-pitfalls.md` for review methodology and known false-positive traps.
- `.agents/web-copy.md` when editing web copy or user-facing hosted product surfaces.

Then read `CONTEXT.md` if it exists, and read any ADRs in `docs/adr/` that touch the area you are about to work in.

## Use the Glossary's Vocabulary

When output names a domain concept in an issue title, refactor proposal, hypothesis, or test name, use the term as defined in `CONTEXT.md`. Do not drift to synonyms the glossary explicitly avoids.

If a needed domain concept is missing, create or update `CONTEXT.md` only when the term has been resolved with enough confidence to record.

## Flag ADR Conflicts

If output contradicts an existing ADR, surface it explicitly rather than silently overriding the decision.
