# heddle#578 — `checkpoint` a preserved (non-current) state by change-id

**Status:** spike (decision doc). No production code lands in this issue. The
follow-up impl issue is #485, currently reframed as "blocked-by this spike."

**Scope:** decide the UX for checkpointing a *preserved* (captured-but-not-the-
checkout's-current) Heddle state, identified by its change-id, before any impl.
The deliverable is a command signature + semantics + safety guards + acceptance
criteria — and an explicit build / don't-build recommendation.

**Grounding:** every "today's behavior" claim is checked against the code at the
cited `path:line` (worktree at `spike/578-checkpoint-changeid`, off
`origin/main`, verified 2026-06-29).

> **EXISTING vs PROPOSED — read this first.** This is a design doc; nothing here
> is built. Rust snippets are illustrative of the *current* code only where a
> `path:line` is cited; the proposed shape is prose, not code.

---

## 1. The use case (why anyone asked)

This spike was split out of #485. The concrete driver is the
**checkpoint-failure recovery path**, not a general "checkpoint any state"
feature.

When `heddle commit` captures a Heddle state and then the Git checkpoint step
fails (Git checkout problem, missing identity, ref-update preflight blocked),
the capture is already durable but the Git commit is not. The user sees:

> `capture <change_id> was preserved, but checkpoint failed: <err>`
> (`crates/cli/src/cli/commands/git_compat.rs:1119`)

The recovery advice tells them to re-run a checkpoint that repairs *only the Git
side* without re-capturing
(`checkpoint_recovery_command`, `git_compat.rs:1129`). Today that recovery
relies on the preserved state still being the checkout's **current** state, and
uses the hidden `--from-index-snapshot` flag for the staged-index case
(`commands_advanced.rs:14`).

The #578 question: should there be an explicit, public
`heddle checkpoint --change-id <id>` that targets a *named* preserved state, so
the recovery (and any "I captured X earlier, never checkpointed it, want a Git
commit for it now" workflow) does not depend on X being current?

The term **"preserved state"** here = a `change_id` that exists in the object
store (captured) but has **no Git checkpoint record yet**
(`latest_git_checkpoint_for_change`, `repository.rs:1781`) and is **not
necessarily** the tip of the attached thread (`head` / `current_state`,
`repository.rs:2126` / `:2205`).

---

## 2. Today's behavior (cited)

### 2.1 What `checkpoint` checkpoints

`heddle checkpoint` is hardwired to the **current** state of the **attached
thread**. The flow (`crates/cli/src/cli/commands/checkpoint.rs`):

1. `CheckpointArgs` has exactly two knobs: `-m/--message` and a hidden
   `--from-index-snapshot` recovery flag. There is **no** state/target argument
   (`commands_advanced.rs:7-16`).
2. `create_git_checkpoint_inner` runs preflights, then
   `ensure_current_state(...)` — it captures or reuses the **current** state, it
   does not accept a target (`checkpoint.rs:191-197`).
3. Idempotency: if a Git checkpoint record already exists for that change-id, it
   returns the existing record and writes nothing
   (`latest_git_checkpoint_for_change`, `checkpoint.rs:213-215`).
4. The Git commit is produced by
   `bridge.write_through_current_checkout_with_message(state.change_id, summary)`
   (`checkpoint.rs:234-236`).

### 2.2 The load-bearing constraint: write-through is thread-tip-only

`write_through_current_checkout` does **not** commit an arbitrary tree. It:

- resolves the **attached thread** (`Head::Attached { thread }`, else it
  *skips* with `DetachedHead`) — `git_core.rs:1431-1435`,
- `export_current_thread(self, &thread)` — exports the **thread's tip**, not a
  caller-supplied state — `git_core.rs:1448`,
- rebuilds the checkout from that exported thread
  (`write_thread_checkout_from_existing_mirror`) — `git_core.rs:1453`.

A comment at `git_core.rs:1443-1448` makes the scoping explicit and
intentional: *"Checkpoint/commit write-through is intentionally scoped to the
attached thread. Moving every Git branch during an everyday save surprised Git
users…"*

**Consequence:** the Git commit checkpoint always reflects **the attached
thread's tip state**. The `state.change_id` passed into
`write_through_current_checkout_with_message` only selects the commit
**message** override (`set_commit_message_override`, `git_core.rs:1461`) — it
does **not** redirect *which* tree gets committed. So "checkpoint state X" only
coincides with "commit X's tree" when **X already equals the thread tip**.

### 2.3 HEAD does not move on checkpoint

`current_state()` is just `head()` → `get_state` (`repository.rs:2205-2210`).
`checkpoint` never calls a thread-switch or HEAD-move. It mirrors the current
HEAD outward to Git; it never moves HEAD to a different state. There is no
worktree re-materialization on checkpoint beyond the index/checkout rebuild from
the *current* thread tip.

### 2.4 How other verbs resolve a state-spec (the prior art for `--change-id`)

`resolve_state_id` (`history_target.rs:44`) is the canonical resolver every
state-taking verb uses. It accepts full IDs, short prefixes, marker names,
thread names, and `HEAD`/`@~N`. `redact`, `purge`, `visibility`, `context` all
take a state spec through it. So a `--change-id` surface would have well-trodden
resolution machinery to lean on — the resolver is **not** the hard part.

---

## 3. The core tension

The ask ("checkpoint state X by id") and the mechanism (write-through commits
**the thread tip**, `git_core.rs:1448`) only line up in **one** case: **X is
already the thread tip but has no Git checkpoint record yet** (capture
succeeded, Git checkpoint failed → exactly the #485 recovery shape).

For any **other** X — an ancestor, a sibling on another thread, an arbitrary
historical state — "checkpoint X" is ambiguous and arguably dangerous:

- **Ancestor of the tip.** Committing an *older* tree to the Git branch ref
  would either rewind the Git branch (data-loss surprise for a colocated Git
  user) or require a detached/side commit that the current write-through path
  has no shape for.
- **Sibling / foreign-thread state.** Would require a HEAD/thread switch (which
  checkpoint deliberately does **not** do, §2.2-2.3) or a brand-new "commit this
  tree onto the current branch as a new tip" operation — that is a *cherry-pick
  / rebase / restore-then-checkpoint*, not a checkpoint.
- **Worktree conflict.** `require_clean_worktree` (`checkpoint.rs:202-211`)
  rejects a dirty worktree that doesn't match the current state's tree.
  Checkpointing a *non-current* X against a worktree that matches a *different*
  state is a guaranteed dirty-worktree refusal unless we also redefine the
  cleanliness check — more surface, more footgun.

So the honest framing: **"checkpoint a preserved state by id" is well-defined
and safe only for the tip-but-uncheckpointed case.** Everything beyond that is a
different feature wearing checkpoint's name.

---

## 4. Candidate UX shapes

### Shape A — public `heddle checkpoint --change-id <id>`, restricted to the tip

Add a public `--change-id <spec>` to `CheckpointArgs`. Resolve via
`resolve_state_id`. **Guard:** the resolved id MUST equal the attached thread's
current tip (`repo.head()`), else refuse with a precise error. On success it is
the existing checkpoint flow, just with an explicit assertion that you're
checkpointing the state you named.

- **Pros:** explicit, auditable ("I am checkpointing exactly hd-…"), reuses the
  canonical resolver, replaces the hidden `--from-index-snapshot` recovery
  string with a self-documenting one, zero new commit machinery.
- **Cons:** the flag is *only ever* a no-op-or-assertion when X==tip. A user who
  reads `--change-id` will reasonably expect it to checkpoint a **non-tip**
  state, and we refuse — the flag's surface over-promises relative to what it
  safely does. Mild [[cli-ergonomics-over-feature-count]] violation: a flag
  whose only real effect is an equality assertion.

### Shape B — no new flag; make the recovery path idempotent on the preserved id internally

Don't expose `--change-id` at all. Instead, fix #485 entirely **inside** the
recovery code: `checkpoint_recovery_command` already knows the preserved
`change_id` (`git_compat.rs:1110-1127`); route the retry to a
checkpoint-the-current-preserved-state path (the existing
`--from-index-snapshot`-style internal path) that asserts "the current state IS
the preserved change_id, and it has no checkpoint record yet" and repairs only
the Git side. The user-facing command stays `heddle checkpoint -m "…"`.

- **Pros:** zero new public surface — strongest [[cli-ergonomics-over-feature-
  count]] fit. The id is plumbed internally where it's actually known; the user
  never types a change-id. Idempotency already exists
  (`checkpoint.rs:213-215`), so the only real work is making the recovery assert
  on the preserved id rather than re-resolving "current."
- **Cons:** doesn't give a *general* "checkpoint state X" verb (but §3 argues
  that general verb is mostly ill-defined anyway). If a user manually lands in
  "captured X, never checkpointed, X still tip" outside the commit-recovery
  flow, they get no explicit knob — though a plain `heddle checkpoint` already
  handles that (it reuses the current state and is idempotent).

### Shape C — full "checkpoint any preserved state" (switch-then-checkpoint)

`heddle checkpoint --change-id <id>` for **any** captured state: if X != tip,
switch HEAD/thread to X (re-materializing the worktree), then checkpoint.

- **Pros:** matches the most literal reading of the issue title.
- **Cons:** this is `thread switch` + `checkpoint` glued together, with all of
  switch's worktree-materialization and dirty-worktree hazards
  (`checkpoint.rs:202-211`), and it silently moves HEAD — exactly the
  "surprised Git users" failure the write-through scoping comment warns against
  (`git_core.rs:1443-1448`). High footgun, low marginal value over running the
  two verbs explicitly. **Reject.**

---

## 5. Recommendation

**Recommended: Shape B (don't add the public flag; fix the recovery internally),
with Shape A available as a thin, tip-guarded escape hatch only if a concrete
non-recovery need appears.**

Net build/don't-build call on the *public `--change-id` flag*: **don't build it
now.** Rationale:

1. The only well-defined, safe meaning of "checkpoint preserved state X" is
   "X is the current tip and has no Git checkpoint yet" (§3). That case is
   **already** served by a plain `heddle checkpoint` (idempotent per change-id,
   `checkpoint.rs:213-215`) — the gap #485 found is purely that the *recovery
   wiring* re-captures instead of repairing-by-id, which is an internal-plumbing
   fix (Shape B), not a missing user-facing flag.
2. A public `--change-id` that refuses every non-tip id (the only safe Shape A)
   over-promises: users will expect non-tip checkpointing and hit a wall. That
   is the [[cli-ergonomics-over-feature-count]] anti-pattern — a flag added for
   completeness, not because the ergonomics demand it.
3. The genuinely-general version (Shape C) is `switch` + `checkpoint` and is a
   footgun; we already have both verbs.

If, after Shape B lands, a real recurring "I have a preserved tip state and want
to name it explicitly when checkpointing" need shows up (e.g. scripts/audit
trails wanting the id in the command), add **Shape A** then — tip-guarded,
resolver-backed, and replacing the hidden `--from-index-snapshot` recovery
string with `--change-id`. It is cheap to add later and expensive to walk back
once public.

---

## 6. If Shape A is later approved — the safe shape spec

For the record, so a future impl issue doesn't re-derive it:

- **Surface:** `heddle checkpoint --change-id <spec> [-m <msg>]`. `<spec>`
  resolved through `resolve_state_id` (`history_target.rs:44`) — full id, short
  prefix, marker, `HEAD`/`@~N`.
- **Semantics:** checkpoint the resolved state **iff** it equals the attached
  thread's current tip (`repo.head()`). HEAD does not move. Reuses the existing
  checkpoint flow verbatim after the assertion.
- **Idempotency:** if the resolved state already has a checkpoint record
  (`latest_git_checkpoint_for_change`, `repository.rs:1781`), print the existing
  record and exit 0 (no-op), matching `checkpoint.rs:213-215`.
- **Mutual exclusivity:** `--change-id` and `--from-index-snapshot` should not
  combine until a case demands it (clap `conflicts_with`).
- **Error cases (each via `RecoveryAdvice`, matching the file's existing style):**
  - resolved id ≠ tip → refuse: *"`--change-id <id>` is not the current tip
    (<tip-id>); checkpoint only commits the attached thread's tip. Switch to it
    with `heddle thread switch …` or checkpoint the current state."* No refs,
    worktree, or Git state changed.
  - spec doesn't resolve → reuse `state_not_found_advice`
    (`history_target.rs:76`).
  - detached HEAD / no attached thread → reuse the existing skip surfacing
    (`git_core.rs:1431-1435`).
  - not a Git-overlay repo → reuse `native_checkpoint_unavailable_advice`
    (`checkpoint.rs:369`).

---

## 7. Acceptance criteria for the follow-up impl (#485)

The impl issue (#485) is the **recovery-plumbing fix (Shape B)**, NOT a public
flag. ACs:

1. The checkpoint-failure recovery (`git_compat.rs` commit path,
   `commit_checkpoint_failed_advice` / `checkpoint_recovery_command`,
   `:1110-1142`) repairs the Git checkpoint **for the already-preserved
   `change_id`** without minting a second capture from the same tree (the #485
   duplication bug).
2. The retry path asserts the preserved `change_id` **is** the current tip and
   has **no** existing Git checkpoint record
   (`latest_git_checkpoint_for_change`); if a record exists it is a no-op exit-0
   (idempotent), matching `checkpoint.rs:213-215`.
3. Regression test: `commit` captures, Git checkpoint fails (inject a
   write-through skip/identity failure), recovery runs → **exactly one** state
   exists for that tree and **exactly one** Git checkpoint record is written.
   Covers both the `--from-index-snapshot` (staged-index) and full-worktree
   recovery scopes called out in #485.
4. No new **public** CLI flag is introduced by #485 (the `--change-id` decision
   is deferred per §5). If the impl finds it genuinely needs an internal target
   parameter, it stays non-public / `hide = true` like `--from-index-snapshot`.
5. Docs/advice strings updated so the recovery command shown to users does not
   imply re-capture.

**Deferred (separate future issue, only if a real need appears):** public
`heddle checkpoint --change-id` per the Shape A spec in §6.

---

## 8. One-line answer

Don't ship a public `heddle checkpoint --change-id` flag. The only safe meaning
of it is "the current tip, not yet checkpointed," which a plain `heddle
checkpoint` already covers; #485's real bug is recovery plumbing that
re-captures instead of repairing the preserved id — fix that internally (Shape
B). Keep the tip-guarded `--change-id` (Shape A, §6) in the back pocket for if a
concrete need surfaces; reject the switch-then-checkpoint reading (Shape C)
outright.
