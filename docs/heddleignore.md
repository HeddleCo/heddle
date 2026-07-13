# `.heddleignore`

`.heddleignore` is heddle's per-repo file for telling `heddle capture`,
`heddle status`, `heddle ready`, and `heddle land` which paths to ignore. It lives at
the worktree root, next to your code.

## Suggested template

`heddle init` does not write `.heddleignore`. Heddle auto-ignores only
its own `.heddle/` metadata; every project artifact must be named by an
explicit ignore file.

The source tree includes a suggested `.heddleignore` template covering
common cross-platform noise — macOS Finder metadata (`.DS_Store`,
`._*`), Xcode user state (`xcuserdata/`, `*.xcuserstate`), JetBrains /
VS Code / Fleet caches (`.idea/`, `.vscode/`, `.fleet/`, `*.iml`),
Vim/Emacs swap and backup files (`*.swp`, `*.swo`, `*~`), Windows shell
metadata (`Thumbs.db`, `desktop.ini`), LibreOffice locks (`.~lock.*`),
and the two shell-redirect typo artifacts that periodically show up
(`-r`, `-rv` — unanchored, so a `src/-r` typo is suppressed too).

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

In Git-overlay repositories, Heddle reads `.gitignore` first and
treats it as the preferred shared ignore policy so raw `git status`
and Heddle agree. Use `.heddleignore` only for Heddle-specific
excludes that should not affect Git.

In native Heddle repositories, `.heddleignore` is the ignore policy.
Heddle does not auto-ignore project artifacts such as build outputs,
dependency folders, caches, or generated files unless they are named
in `.heddleignore`.

Heddle always protects its own `.heddle/` metadata. Other paths must
be explicit in `.gitignore` or `.heddleignore`.

## Common-noise hints

When `heddle land` refuses because of untracked paths the workflow
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

## macOS `Icon` metadata

macOS Finder stores custom-folder icon metadata in a file whose
basename is literally `Icon` followed by a carriage return (`Icon\r`
— four chars + the `\r` byte). The suggested template does **not**
suppress this. The only glob that targets it without an awkward
literal control-char (`Icon?`) also matches legitimate basenames
like `Icons`, `Icon1`, or an `Icons/` directory full of real
assets, which would silently hide project content from `status` and
`capture`.

**Note:** heddle's `.heddleignore` parser normalizes whitespace
including trailing `\r`, so the `Icon\r` filename cannot be
expressed as a `.heddleignore` pattern — any line written as
`Icon\r` is read back as plain `Icon` and would suppress
legitimate files or directories named `Icon`. If these metadata
files are noise in your workflow, suppress them at the macOS
level instead: delete the custom folder icon, or set Finder's
"Hide" attribute on the file.

## Local-tool state (`.claude/`, etc.)

The suggested template leaves `.claude/` commented out. Some teams
version their tool prompts (`.claude/CLAUDE.md`, agent definitions);
others don't. If your `.claude/` directory only carries local state
(`scheduled_tasks.lock`, ephemeral chat history), uncomment the line
in the suggested template or add the specific paths.

## Hydrating an isolated checkout (`heddle start --hydrate`)

Because heddle never captures ignored paths, an isolated checkout made
with `heddle start <name> --path <dir>` is a faithful *source* tree with
no dependency directories: a JS project's `node_modules`, a Python
project's `.venv`, a Rust workspace's `target/`, and so on are all left
out. That's correct for capture, but it means you can't run
`tsc`/`eslint`/tests in the fresh checkout without reinstalling deps
first.

Pass `--hydrate` to make the checkout immediately buildable:

```
heddle start task/x --path /tmp/hv-x --hydrate
```

`--hydrate` **symlinks** each top-level ignored directory from the
origin checkout into the new checkout. It uses symlinks (not copies) so
the isolated thread stays cheap — a multi-gigabyte `node_modules` is
linked in O(1) rather than duplicated per thread.

What it links, and what it doesn't:

- **Linked:** top-level directories at the origin root that match an
  ignore rule (in Git-overlay repos that includes `.gitignore`; in
  native repos, `.heddleignore`).
- **Not linked:** the admin directories `.git` and `.heddle` (even
  though they're ignored), plain files, and per-package dependency dirs
  nested below the root (e.g. a monorepo's `packages/*/node_modules`).
- **Never clobbered:** if the checkout already has an entry of that
  name, hydrate leaves it alone.

The links point back at the origin's directories and **stay ignored**,
so the hydrated deps are never captured into heddle — the same ignore
rule that hides `node_modules/` at the origin also hides the linked
`node_modules` in the checkout. Because the directories are *shared*
with the origin, treat hydrate as a read-mostly convenience: installing
a new dependency through a hydrated link mutates the origin's copy too.

`--hydrate` applies to bytes-on-disk threads (solid / materialized
checkouts). It has no effect on virtualized (FUSE-mounted) threads.

## Editing

`.heddleignore` is read fresh on every walk — no daemon restart, no
re-init. Add, remove, or reorder rules as needed. The suggested
template is shipped in
`crates/cli/src/cli/commands/heddleignore_defaults.rs` if you want
to copy patterns back in after editing.
