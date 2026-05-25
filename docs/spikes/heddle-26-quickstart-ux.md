# heddle#26 — first-run UX spike

**Status:** spike (decision doc). Impl tracked in a follow-up issue.
**Scope:** CLI ergonomics only. Marketing copy lives in heddle#229.

The README's `## Quickstart` block (`README.md:79-99`) currently lists 9
commands. A brand-new user installing `heddle-cli` from `cargo install`
must execute several of those in order before they have a first commit
in Heddle's history. This doc picks the shape of a single command that
collapses that ramp into one invocation.

## §1 — Current state

### What `heddle init` does today

`crates/cli/src/cli/commands/init.rs:22-94` (`cmd_init`):

1. Resolves the target path (cwd by default).
2. Detects `.git/` via `gix::discover` (`init.rs:37`). If found, calls
   `Repository::bootstrap_git_overlay`; otherwise `Repository::init_default`.
3. Installs the default `.heddleignore` if absent
   (`init.rs:53`, `init.rs:106-145`).
4. If `--principal-name`/`--principal-email` are both passed, writes
   them into `UserConfig` (`init.rs:55-69`).
5. Calls `maybe_prompt_init_install`
   (`integration.rs:133-187`) which, on a TTY, may offer to wire
   detected harnesses (Claude Code, Cursor, etc.).
6. Prints a one-line confirmation (`init.rs:73-94`).

What it does **not** do: start a thread, capture any work, or make a
checkpoint. The repo is initialized but the user's history is empty.

### What a new user has to type today to ship a "first commit"

Per `README.md:79-99`, the minimum path from `cargo install heddle-cli`
to a checkpointed commit is:

```bash
heddle init
heddle start <thread-name>        # thread.rs:260
# ... edit some files ...
heddle capture -m "..."           # SnapshotArgs in commands_main.rs:206
heddle checkpoint -m "..."        # CheckpointArgs in commands_main.rs:209
```

Plus, before any of that, the user almost always needs to set their
principal identity (otherwise checkpoints attribute to a placeholder).
That's `heddle init --principal-name ... --principal-email ...` *or* a
separate `heddle config` step.

So today's minimum is **4 commands and ≥2 flags**, with the user
expected to know each verb (`start`, `capture`, `checkpoint`) and the
identity flags by name. None of that is discoverable from a fresh
`heddle --help`.

### What's missing for a 60s first run

Concretely, the gap a quickstart command must close:

- **Principal identity.** Today: separate flags or a `config` invocation.
  The quickstart must prompt for name+email (or accept them up-front
  via flag) before anything else, because every later step references
  them.
- **First thread.** A new user has no reason to know the word "thread."
  Quickstart should pick a sensible default name (e.g. `main-work` or
  the current branch) and tell them what it did.
- **First capture + checkpoint.** With no captured work, the repo is
  inert and `heddle status` says so. Quickstart should produce one
  non-destructive capture of cwd-as-is (or a placeholder
  `.heddle/QUICKSTART.md`) so the user lands on a state they can
  `heddle log` and see history.
- **A pointer to "now what."** End by printing the *next* command —
  reuse the `recommended_action` channel already plumbed through
  `heddle status` / `heddle diagnose` (`README.md:101`).

## §2 — Options compared

### Option A — multi-step wizard (`heddle quickstart`, interactive)

Walks the user through Q&A: repo kind, identity, thread name,
remote-or-not, etc. Each step has a default the user can `<enter>`
through.

- **Pros:** discoverable; teaches the verbs as it goes; lowest
  prerequisite knowledge.
- **Cons:** *bad in CI* — every CI runner that tries the new flow has
  to feed `<enter>` to stdin or the wizard hangs. Engineers hate
  wizards (cf. `npm init`'s endured-not-loved status). Lots of new
  surface to test (every prompt is a branch).
- **CI-friendly?** No, unless every prompt also has a flag override —
  which is just Option B with a slow path layered on top.
- **Test surface:** large. Each prompt is a stdin-driven branch.
- **Surprise budget:** moderate — the user explicitly answers each
  question, so nothing happens behind their back.
- **Fit:** would be a new top-level `Commands::Quickstart` variant in
  `cli_args/commands_main.rs`, plus a new module under
  `cli/commands/quickstart.rs`.

### Option B — single-flag-driven `heddle init --quickstart`

Extends today's `heddle init` (`init.rs:22`) with a `--quickstart`
flag. Behavior: do everything `init` already does, plus start a thread,
make one capture, and checkpoint. Identity is taken from
`--principal-name`/`--principal-email` if present, else prompted *once*
in a single TTY block (with a clear "skip and use placeholder?"
escape).

- **Pros:** scriptable (`heddle init --quickstart --principal-name X
  --principal-email Y`); reuses existing `InitArgs`
  (`commands_args.rs:12-39`); reuses the existing harness-install
  prompt (`integration.rs:133-187`) which already has TTY/`--quiet`
  fallbacks. One new flag, no new top-level verb.
- **Cons:** less discoverable — a new user running `heddle init` with
  no flags still gets today's minimal behavior. Mitigation: when `init`
  runs interactively on an empty cwd, the success line can suggest
  re-running with `--quickstart`. (Also: `heddle status` on a
  fresh-init repo with no thread can put `--quickstart` in
  `recommended_action`.)
- **CI-friendly?** Yes — fully flag-driven. Identity flags + the
  existing `--no-harness-install` cover headless invocation.
- **Test surface:** small. One new code path through `cmd_init`;
  existing tests cover the rest.
- **Surprise budget:** low. The flag is explicit; the only new state
  written is what the user just opted into.
- **Fit:** lives entirely inside `cmd_init`. No new module.

### Option C — config-file-prompted (`.heddle/quickstart.toml`)

User writes a small TOML, then `heddle init` consumes it.

- **Pros:** zero-prompt CI experience; declarative.
- **Cons:** defeats the pitch — the user has to learn the TOML shape
  before they can get to first commit. We just moved the friction
  upstream.
- **CI-friendly?** Yes, but so is B with flags.
- **Verdict:** dominated by B for the 60s use case. Could be a
  follow-up for "I'm provisioning N repos in a CI matrix" but that's
  not who heddle#26 is for.

### Option D — hybrid: `--quickstart` flag + sensible interactive fallback

Same as B, but: when invoked on a TTY with no `--principal-*` flags,
prompt for name+email in a single block (one prompt cycle, not a
multi-step wizard). Anything else (thread name, what to capture) uses
defaults. Reuses the existing harness-install confirm
(`integration.rs:169-176`) for consistency.

- This is what B effectively *is* once you handle the "user didn't pass
  identity flags but is on a TTY" case. Calling it out separately just
  to be explicit.

## §3 — Decision

**Recommend Option D (= B + one identity prompt on TTY).**

Justification:

1. **It's the smallest delta to today's code.** `cmd_init`
   (`init.rs:22-94`) already does steps 1-5 of the 6-step plan; the
   quickstart additions are "start thread → capture → checkpoint" plus
   a single-block identity prompt. No new top-level verb, no new
   module. Compare to Option A which is a new command surface and
   ~5-10 prompts worth of stdin-driven branching to test.
2. **It's CI-honest.** The existing `--no-harness-install` /
   `is_tty()` plumbing in `integration.rs:138-141` already handles the
   "headless invocation" case. The same gate trivially applies to the
   identity prompt: with flags or `--quiet`, no prompt; without, one
   block of stdin.
3. **It composes with what's already there.** `heddle status` already
   surfaces `recommended_action` (`README.md:101`); the quickstart's
   "now what" line is a one-liner change, not a new system.

The discoverability gap (a brand-new user might never see
`--quickstart`) is the one real cost. It's addressable by editing the
README's Quickstart section to lead with `heddle init --quickstart`,
and by having `heddle status` recommend it when run in a fresh `.heddle/`
with no thread yet.

## §4 — Acceptance shape for the impl issue

The follow-up impl issue should ship:

- [ ] `--quickstart` flag added to `InitArgs`
      (`crates/cli/src/cli/cli_args/commands_args.rs:12`).
- [ ] When `--quickstart` is set, `cmd_init` (`init.rs:22`) does, in
      order, after today's init steps:
  1. Resolve principal identity. Priority: `--principal-name` +
     `--principal-email` flags → existing `UserConfig` → interactive
     prompt (TTY only). On `--quiet` or non-TTY without flags, **fail
     fast** with an actionable error (don't silently use a
     placeholder).
  2. Start a thread. Default name: `quickstart` (no collision check
     beyond what `cmd_start` already enforces). Configurable via
     `--quickstart-thread <name>`.
  3. Make one capture. If cwd has user files, capture them with
     message `"quickstart: initial capture"`. If cwd is empty, write
     `.heddle/QUICKSTART.md` (a short pointer file) and capture that.
     Either path is non-destructive: existing files are not modified.
  4. Make one checkpoint with message `"quickstart: first commit"`.
- [ ] **Confirmation gate before any destructive write.** If cwd
      already has `.heddle/` *or* `.git/HEAD` referencing a non-empty
      history, print what `--quickstart` would do and require y/N
      confirmation. `--yes` (or `--quickstart=force`) bypasses.
- [ ] **ESC-able.** Ctrl-C at any prompt exits with status 130 and
      leaves the worktree exactly as the prompt found it. Concretely:
      writes happen *after* all prompts, in one batch, so a Ctrl-C
      mid-prompt leaves no `.heddle/` half-written. (If `heddle init`
      already created `.heddle/` before the identity prompt — which is
      today's order — split that so the directory creation also
      happens post-confirm. Test: Ctrl-C during the identity prompt on
      an empty cwd leaves no `.heddle/`.)
- [ ] Output ends with a `recommended_action`-style "next:" line
      pointing at `heddle log` or `heddle status`.
- [ ] `heddle status` on a freshly-`init`'d repo with no thread surfaces
      `heddle init --quickstart` in `recommended_action`.
- [ ] README's `## Quickstart` (`README.md:79-99`) leads with the new
      single command; the verb-by-verb tour stays as a follow-on
      section for users who want the long form.
- [ ] Sentinel test as in §5.
- [ ] Telemetry-free. No new dependencies pulled in just for the
      prompt — reuse whatever `integration.rs` already uses for stdin
      (`io::stdin().read_line` at `integration.rs:173-174`).

## §5 — Sentinel test

The DoD AC says: "fresh CI runner with no prior heddle state can hit
the success state from a single command." Sketch:

```bash
# tests/cli/quickstart_sentinel.rs (integration test) OR
# .github/workflows/quickstart-sentinel.yml step.

set -euo pipefail

# 1. Fresh tempdir, no .heddle / no .git.
TMP="$(mktemp -d)"
cd "$TMP"

# 2. Single command, fully flag-driven (no stdin).
heddle init --quickstart \
  --principal-name "CI Sentinel" \
  --principal-email "ci@example.invalid" \
  --no-harness-install \
  --yes

# 3. Success state assertions:
test -d .heddle                       # init landed
heddle status --json | jq -e '.thread'                  # a thread exists
heddle log --json   | jq -e 'length >= 1'               # at least one checkpoint
heddle log --json   | jq -e '.[0].message | test("quickstart")'

# 4. Timing budget (advisory, not a hard gate in CI):
#    the whole block above should complete in <5s on the standard runner;
#    the 60s budget includes the install + first edit, which the sentinel
#    doesn't measure.
```

The test should live as a Rust integration test under
`crates/cli/tests/` (where `heddle init` integration tests already
live, per the pattern there), invoked via `cargo test`. A
workflow-level variant can be added later if we want the timing
budget enforced.

## §6 — Out of scope

Explicitly **not** addressed by either this spike or the impl issue
that follows it:

- Marketing landing-page copy, hero animation, or anything visible
  outside the CLI. That's heddle#229.
- Telemetry of any kind (success rate, time-to-first-commit, etc.).
  The AC's "telemetry-free" rule is a hard constraint.
- Hosted-Tapestry onboarding (account creation, OAuth, remote setup).
  Tapestry-side per the issue body.
- Multi-repo / monorepo bootstrap. `--quickstart` runs against cwd.
- Migration from an existing non-empty `.heddle/` (different problem
  — that's a config-doctor concern, not first-run).
- A `heddle quickstart` top-level verb. The decision is to put this
  behavior behind a flag on `init`, not a new verb. (Reconsider if a
  future feature needs a quickstart that doesn't go through `init` at
  all — none on the roadmap today.)
- New dependencies. The spike's constraint forbids adding deps; the
  impl uses what's already in `Cargo.toml`.
