# `.heddleignore`

`.heddleignore` is heddle's per-repo file for telling `heddle capture`,
`heddle status`, and `heddle merge` which paths to ignore. It lives at
the worktree root, next to your code.

## Default template

`heddle init` writes a starter `.heddleignore` covering the
overwhelmingly-common cross-platform noise — macOS Finder metadata
(`.DS_Store`, `._*`), Xcode user state (`xcuserdata/`,
`*.xcuserstate`), JetBrains / VS Code / Fleet caches (`.idea/`,
`.vscode/`, `.fleet/`, `*.iml`), Vim/Emacs swap and backup files
(`*.swp`, `*.swo`, `*~`), Windows shell metadata (`Thumbs.db`,
`desktop.ini`), LibreOffice locks (`.~lock.*`), and the two
shell-redirect typo artifacts that periodically show up (`/-r`,
`/-rv`).

The template is intentionally conservative: only patterns the entire
team is overwhelmingly likely to want suppressed. Project-specific
patterns (build outputs, generated bindings, `.env*`, lockfiles) are
yours to add — the file is plain text.

## Syntax

The matcher is `gitignore`-compatible: glob patterns (`*.log`,
`config/*.toml`), `**` for recursive directory globs, leading `/`
for root-anchored, trailing `/` for directory-only, and `!` for
negation. Order is significant — a later rule can re-include a path
a previous rule ignored. The same rules apply as in `.gitignore`.

## Relationship to `.gitignore`

**Heddle does not read `.gitignore`.** `.heddleignore` and
`.gitignore` are independent files. If you keep both Git and Heddle
in the same repository (the git-overlay workflow), patterns that
should suppress both walks must appear in both files. We considered
auto-reading `.gitignore`, but kept them separate because:

- Heddle is a different VCS with its own capture semantics —
  `heddle capture` snapshots the worktree directly, so what you
  want suppressed during capture may not match what Git ignores
  during `git add`.
- Some teams want one file checked into Git and the other left
  uncommitted as a local override.
- A surprising auto-merge of `.gitignore` rules would make heddle's
  behavior depend on a file it does not otherwise own.

If your team's `.gitignore` and `.heddleignore` should track the
same content, symlink one to the other locally — heddle reads
`.heddleignore` directly off disk every walk, so a symlink is
transparent.

## Common-noise hints

When `heddle merge` refuses because of untracked paths the workflow
didn't expect, paths that look like common noise are annotated
inline with a `.heddleignore` suggestion:

```
heddle: 3 unrelated uncommitted git change(s) outside the merge:
  .DS_Store [HINT: looks like macOS Finder metadata — add `.DS_Store` to .heddleignore?]
  src/.foo.rs.swp [HINT: looks like Vim swap file — add `*.swp` to .heddleignore?]
  src/main.rs
```

Real source files (no matching noise category) are listed without a
hint — heddle never suggests suppressing a path it doesn't recognize
as noise.

## Local-tool state (`.claude/`, etc.)

The default template leaves `.claude/` commented out. Some teams
version their tool prompts (`.claude/CLAUDE.md`, agent definitions);
others don't. If your `.claude/` directory only carries local state
(`scheduled_tasks.lock`, ephemeral chat history), uncomment the line
in the default template or add the specific paths.

## Editing

`.heddleignore` is read fresh on every walk — no daemon restart, no
re-init. Add, remove, or reorder rules as needed. The bundled
default template is shipped in
`crates/cli/src/cli/commands/heddleignore_defaults.rs` if you want
to copy patterns back in after editing.
