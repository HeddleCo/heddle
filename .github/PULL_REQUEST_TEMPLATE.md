<!--
  HeddleCo PR template — fill in every section below; the inline guidance
  comments are safe to leave in place. The DoD CI gate (.github/workflows/
  dod-check.yml) parses this body and will fail the check if a required
  section is missing for your file scope:

    - All PRs:                  a Closes / Fixes / Resolves / Part of /
                                Tracked by / Refs line.
    - Touches non-doc code:     a Test evidence section with a fenced code block.
    - Touches .rs files:        a Red commit section with a SHA.
    - Touches UI code only:     a Manual verification section.

  Pure-docs (.md/.txt) and pure-CI (.github/) PRs are exempt from the
  test-evidence requirement. See the kanban contract for the full DoD shape:
  https://github.com/HeddleCo/.github/blob/main/profile/BOARD.md
-->

## Summary

<!-- 2–4 bullets. What changed and why. Reviewer-facing, not commit-history-facing. -->

-
-

## Closes

<!--
  REQUIRED. Pick one form:
    Closes HeddleCo/<repo>#<n>     — this PR's merge should close the issue.
    Part of HeddleCo/<repo>#<n>    — cross-repo work; the issue stays open until siblings land.
    Refs HeddleCo/<repo>#<n>       — referenced for context only.

  Use Closes only on the *last* PR in a multi-PR change. Use Part of on every
  cross-repo PR otherwise.
-->

Closes HeddleCo/<repo>#<n>

<!--
  Required when the PR touches .rs files. Replace <sha> with the SHA of the
  commit that pushed failing tests *before* implementation, per TDD-Rust DoD.
  Skip the section (or write `N/A — not a Rust PR`) for non-Rust PRs.
-->

## Red commit
`<sha>`

<!--
  Required unless the entire diff is .md/.txt or under .github/. Paste actual
  command output (cargo test, bun test, playwright report, etc.) inside the
  fenced block. Show the tests from the red commit now passing.
-->

## Test evidence
```
<paste test output here>
```

## Manual verification

<!--
  Required for UI PRs (TS/TSX/Svelte/JS). For Rust-only or pure-server PRs,
  write: N/A — covered by tests.

  UI PRs should list:
    - Viewports tested (375×812, 768×1024, 1280×800, etc.)
    - Auth states tested (signed-out, signed-in, admin)
    - Before/after screenshot URLs (Playwright `--screenshot=on` or manual)
-->

## Blocked by → resolved

<!--
  If merging this PR resolves another issue's `## Blocked by` reference,
  list those issues so the orchestrator can re-evaluate them. Otherwise None.
-->

None
