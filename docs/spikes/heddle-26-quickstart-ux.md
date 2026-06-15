# heddle#26 — first-run UX spike

**Status:** spike (decision doc). Impl tracked in a follow-up issue.
**Scope:** CLI ergonomics only. Marketing copy lives in heddle#229.

The README's `## Quickstart` block (`README.md:79-99`) currently lists 9
commands. A brand-new user installing `heddle-cli` from `cargo install`
must execute several of those in order before they have a first
user-visible commit in Heddle's history. This doc picks the shape of a
single command that collapses that ramp into one invocation.

## §1 — Current state

### What `heddle init` does today

`crates/cli/src/cli/commands/init.rs:22-94` (`cmd_init`):

1. Resolves the target path (cwd by default).
2. Detects `.git/` via `gix::discover` (`init.rs:37`). If found, calls
   `Repository::bootstrap_git_overlay`; otherwise
   `Repository::init_default` (`crates/repo/src/repository.rs:453-457`).
3. Installs the default `.heddleignore` if absent
   (`init.rs:53`, `init.rs:106-145`).
4. If `--principal-name`/`--principal-email` are both passed, writes
   them into `UserConfig` (`init.rs:55-69`).
5. Calls `maybe_prompt_init_install`
   (`integration.rs:133-187`) which only wires harnesses when
   `--install-harnesses` is explicitly passed.
6. Prints a one-line confirmation (`init.rs:73-94`).

In the non-Git path, `Repository::init_default`
(`crates/repo/src/repository.rs:453-457`) is the composite of
`Repository::init` + `Repository::seed_default_thread`. Concretely, a
fresh `heddle init` in an empty cwd produces:

- A `.heddle/` directory with `objects/`, `refs/`, `oplog`, and
  `config.toml` (`repository.rs:414-430`).
- `.heddle/HEAD` written as `Attached { thread: "main" }`
  (`repository.rs:432-434`). The integration test
  `crates/cli/tests/cli_integration/thread_default_current.rs:80-82`
  encodes this: "`heddle init` auto-attaches HEAD to `main`."
- A `main` thread ref pointing at a **synthetic seed state**: an
  empty-tree snapshot stamped with the `seed_principal()` "Heddle
  <init@heddle>" actor (`repository.rs:1570-1581`,
  `repository.rs:1730-1732`).
- A `.heddleignore` with the default exclude list, which also includes
  `.heddle`, `.heddleignore`, `.git`, `target`, `node_modules` per
  `repo_config.rs:257-264`.

The synthetic seed state is filtered out of user-facing log walks by
`is_synthetic_root` (`repository.rs:1738-1743`: "no parents, no intent,
seed principal — pre-history, not user work"). So `heddle log` on a
fresh-init repo prints zero entries even though a thread technically
exists.

Net: after `heddle init`, the repo *is* on `main`, but the user's
history (everything `heddle log` shows) is empty, no user identity is
captured unless flags were passed, and no user-visible checkpoint
exists. The Git-overlay path (`bootstrap_git_overlay`,
`repository.rs:465-478`) mirrors the current Git HEAD instead of
seeding `main`; otherwise the same gap applies.

### What a new user has to type today to reach a "first commit"

Per `README.md:79-99`, the minimum path from `cargo install heddle-cli`
to a *user-visible* checkpointed commit on the Git-overlay path is:

```bash
heddle init
# (HEAD already on main; no need for `heddle start` unless renaming.)
# ... edit some files ...
heddle capture -m "..."           # SnapshotArgs in commands_main.rs:206
heddle checkpoint -m "..."        # CheckpointArgs in commands_main.rs:209
                                   #   — Git-overlay only; native repos skip this step.
```

Plus, before any of that, the user almost always needs to set their
principal identity. Otherwise captures attribute to the
`UserConfig::default()` placeholder. That's
`heddle init --principal-name ... --principal-email ...` *or* a
separate `heddle config` step.

So today's minimum is **2 verbs + identity flags** on Git-overlay (3
verbs if the user renames the thread off `main`), with the user
expected to know `capture`, `checkpoint`, and the identity flag names.
None of that is discoverable from a fresh `heddle --help`.

### What's missing for a 60s first run

The scaffolding `heddle init` already does is *not* the gap. The
remaining gap a quickstart command must close is:

- **Principal identity.** Today: separate flags or a `config`
  invocation. The quickstart must prompt for name+email (or accept them
  up-front via flag) before any capture, because every captured state
  references them.
- **First user-visible capture.** With no captured work, `heddle log`
  is empty (the synthetic root is filtered) and `heddle status` says
  so. Quickstart should produce one non-destructive capture of
  cwd-as-is (or a placeholder file at the repo root) so the user lands
  on a state they can `heddle log` and see history for.
- **First checkpoint where the capability supports it.** Checkpoint is
  Git-overlay-only per `crates/cli/src/cli/commands/checkpoint.rs:67-71`
  (`bail!` when `repo.capability() != RepositoryCapability::GitOverlay`).
  For Git-overlay repos, quickstart should follow the capture with a
  checkpoint. For native-Heddle repos, the equivalent "you're done"
  signal is the capture itself plus the printed pointer below.
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
flag. Behavior: do everything `init` already does (which, as §1
spells out, already creates `.heddle/`, attaches HEAD to `main`, and
seeds the `main` thread), plus resolve identity, make one capture,
and — on Git-overlay only — checkpoint. Identity is taken from
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
  fresh-init repo whose log is empty can put `--quickstart` in
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
multi-step wizard). Anything else (what to capture, capability-specific
follow-up like checkpoint) uses defaults. Reuses the existing
harness-install confirm (`integration.rs:169-176`) for consistency.

- This is what B effectively *is* once you handle the "user didn't pass
  identity flags but is on a TTY" case. Calling it out separately just
  to be explicit.

## §3 — Decision

**Recommend Option D (= B + one identity prompt on TTY).**

Justification (note: §1's corrected grounding makes the seam *cleaner*,
not weaker — `init` already produces the scaffolding, so `--quickstart`
only has to add the identity + capture + capability-gated checkpoint
+ "now what" line):

1. **It's the smallest delta to today's code.** `cmd_init`
   (`init.rs:22-94`) already does the scaffolding and the
   HEAD-attach-to-`main`. The quickstart additions reduce to identity
   resolve → capture → (Git-overlay only) checkpoint → next-step
   pointer. No new top-level verb, no new module, no separate
   "start the first thread" step (init already did it). Compare to
   Option A which is a new command surface and ~5-10 prompts worth of
   stdin-driven branching to test.
2. **It's CI-honest.** The existing `--no-harness-install` /
   `is_tty()` plumbing in `integration.rs:138-141` already handles the
   "headless invocation" case. The same gate trivially applies to the
   identity prompt: with flags or `--quiet`, no prompt; without, one
   block of stdin.
3. **It composes with what's already there.** `heddle status` already
   surfaces `recommended_action` (`README.md:101`); the quickstart's
   "now what" line is a one-liner change, not a new system. The
   capability gate on `checkpoint`
   (`checkpoint.rs:67-71`) means `--quickstart` does the right thing on
   both native and Git-overlay repos without quickstart owning new
   capability-detection logic — it can ask the `Repository` what
   capability it ended up with and branch on that.

The discoverability gap (a brand-new user might never see
`--quickstart`) is the one real cost. It's addressable by editing the
README's Quickstart section to lead with `heddle init --quickstart`,
and by having `heddle status` recommend it when run in a fresh repo
whose log is empty.

## §4 — Acceptance shape for the impl issue

The follow-up impl issue should ship:

- [ ] `--quickstart` flag added to `InitArgs`
      (`crates/cli/src/cli/cli_args/commands_args.rs:12`).
- [ ] When `--quickstart` is set, `cmd_init` (`init.rs:22`) does, in
      order, after today's init steps (which already create `.heddle/`
      and attach HEAD to `main`):
  1. Resolve principal identity. Priority: `--principal-name` +
     `--principal-email` flags → existing `UserConfig` → interactive
     prompt (TTY only). On `--quiet` or non-TTY without flags, **fail
     fast** with an actionable error (don't silently use a
     placeholder).
  2. Make one capture on the current thread (`main`, since
     `init_default` / `bootstrap_git_overlay` already attached HEAD).
     If cwd has user files, capture them with message `"quickstart:
     initial capture"`. If cwd is empty *of capturable files*, write
     `QUICKSTART.md` at the repo root (a short pointer file) and
     capture that. The root-level path matters: the default ignore
     list (`repo_config.rs:257-264`) excludes `.heddle/`, so any
     placeholder under `.heddle/` would be silently dropped by the
     capture walk. Either path is non-destructive: existing files are
     not modified.
  3. If `repo.capability() == RepositoryCapability::GitOverlay`, make
     one checkpoint with message `"quickstart: first commit"`. On
     native-Heddle repos, the capture from step 2 *is* the user-visible
     "first commit"; quickstart prints the capture's change-id as the
     completion signal instead. (Rationale: `cmd_checkpoint` bails for
     non-Git-overlay capabilities per `checkpoint.rs:67-71`, so an
     unconditional checkpoint AC would be unimplementable on the
     native path.)
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
- [ ] `heddle status` on a freshly-`init`'d repo whose log is empty
      surfaces `heddle init --quickstart` in `recommended_action`.
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

# 3. Success state assertions. `heddle log --output json` emits
#    `LogOutput { repository_capability, storage_model, states: [...] }`
#    (see crates/cli/src/cli/commands/log.rs:41-46), so the jq
#    fragments index through `.states`, and each entry uses `intent`
#    (StateEntry.intent at log.rs:65) as its capture message field.
test -d .heddle                                                  # init landed
heddle status --output json | jq -e '.thread'                    # a thread exists
heddle log --output json    | jq -e '.states | length >= 1'      # at least one user-visible capture
heddle log --output json    | jq -e '.states[0].intent | test("quickstart")'

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

**Sentinel jq validated locally** against today's `heddle log --output
json` schema (running `heddle init` + a manual `heddle capture -m
"quickstart: initial capture"` against the current build, then the
corrected jq fragments). Output:

```
{
  "cap": "native-heddle",
  "n": 1,
  "first_intent": "quickstart: initial capture",
  "first_change": "hd-03zphd131jcq"
}
```

— i.e. `.states | length` returns `1`, and `.states[0].intent` matches
the capture message. The PR body Round 2 section reproduces this.

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
