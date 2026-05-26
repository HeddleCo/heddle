# Code Review Pitfalls and Hard-Won Lessons

Concrete mistakes made during past reviews and the rules that prevent them.
Each entry is grounded in a real finding or false positive from actual sessions.

---

## Pitfall 1: Claiming content-addressing violations without reading the identity type

**What went wrong:**
A reviewer saw `state.sign(signer)` followed by `store.put_state(&state)` and reported HIGH:
"Signing a state breaks content-addressing — modifying content then storing under the same ID."

**Why it was wrong:**
`State` has *two* identifiers:
- `change_id: ChangeId` — randomly generated UUID-like value, **never derived from content**
- `content_hash: Option<ContentHash>` — BLAKE3 over the tree/parents/metadata, **explicitly excludes the `signature` field**

The signature is computed over `compute_hash()` (which omits the signature), then stored alongside.
Writing back a signed state under the same `change_id` is correct by design.

**Rule:** Before reporting a content-addressing or identity violation, read:
1. The type's field definitions — what is the key/ID?
2. The `hash()` or `compute_hash()` method — what fields are included?
3. Any `PartialEq` / `Hash` impl — what determines equality?

Do not assume "content-addressed" means every field is covered by the hash.

---

## Pitfall 2: Stopping at the first instance of a recurring anti-pattern

**What went wrong:**
`remove_dir_all` through symlinks was caught in `repository_goto.rs` (Session 05) but the same
bug existed in `cherry_pick.rs` and several other CLI commands. Only the first location was
reported.

**What should have happened:**
When any of these patterns are found, **immediately grep the whole codebase** for all instances:

```
remove_dir_all     → search all .rs files
fs::write(         → search all .rs files (atomic write anti-pattern)
WalkBuilder::new   → search all .rs files (each needs .follow_links(false))
is_ok()            → search env::var calls (should check value, not just presence)
unwrap_or_default  → search all crypto + security paths
let _ =            → search all .rs files (silent error discard)
```

Report every location. A finding that appears in 4 places is not 4 separate low-severity findings —
the pattern itself is a systemic issue worth calling out as such.

---

## Pitfall 3: Skipping modules because they "seem straightforward"

Sessions 22 (worktree_cmd/agent_cmd), 24 (semantic/), and 25 (hooks/logging/cli_args) were
under-reviewed or skipped. They contained:

- Child process environment inheritance (HIGH security: exposes API keys to hook scripts)
- `is_ok()` used to check feature-flag env vars (treats `"false"` as enabled)
- Hook script content passed as CLI positional argument (visible in `ps aux`, shell history, hits ARG_MAX)
- `atty` crate is unsound on Unix
- Rename detection using first match instead of best match (semantic/)
- Unbounded AST recursion (semantic/)

**Rule:** Every module gets reviewed. "Utility" and "semantic" modules often contain the most
surprising security issues precisely because they receive less scrutiny.

---

## Pitfall 4: Missing the lock-discipline pattern

Lock discipline for multi-step mutations was not flagged even though it is one of the most
common correctness issues in concurrent code.

**The pattern to look for:**
```rust
// WRONG — state can change between open() and read()
let repo = Repository::open(...)?;
let state = repo.head()?;         // another writer modifies here
repo.snapshot(state)?;

// RIGHT — lock before any read in the sequence
let repo = Repository::open(...)?;
let _lock = repo.locker().write()?;
let state = repo.head()?;         // stable under lock
repo.snapshot(state)?;
```

Any function that does (read → transform → write) without holding a write lock for the
entire sequence is a data race waiting to happen. Heddle uses OS-level file locks
(`fs2::FileExt`) so this affects multi-process scenarios, not just threads.

---

## Pitfall 5: Missing `create_new(true)` for TOCTOU-safe file creation

The pattern:
```rust
// WRONG — race between check and create
if path.exists() { return Err(...); }
std::fs::write(&path, content)?;

// RIGHT — atomic O_CREAT | O_EXCL
std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)   // fails with AlreadyExists if file exists
    .open(&path)?;
```

`create_new(true)` is a single atomic syscall. Any `exists()`-then-`write` sequence is always
wrong for exclusive creation, even with a lock, because the lock may not be held at that point.

---

## Pitfall 6: Missing child-process environment inheritance

```rust
// WRONG — full parent env passed to hook
let mut cmd = Command::new(&hook_path);

// RIGHT
let mut cmd = Command::new(&hook_path);
cmd.env_clear()
   .env("PATH", std::env::var("PATH").unwrap_or_default())
   .env("HOME", std::env::var("HOME").unwrap_or_default())
   .env("HEDDLE_REPO", &ctx.repo_path);
```

`Command::new` without `env_clear()` passes the entire process environment to the child.
In a developer environment that includes `ANTHROPIC_API_KEY`, `AWS_SECRET_ACCESS_KEY`,
`GITHUB_TOKEN`, etc. This is HIGH severity when the child is user-supplied code (hooks).

Search for every `Command::new` in the codebase. Each one needs `env_clear()` unless the
subprocess is fully trusted infrastructure. Production CLI paths must not spawn `git`; Git
fixture helpers in tests are not an exception to environment hygiene when they execute
outside tightly controlled setup.

---

## Pitfall 7: Missing startup-time vs shutdown-time cleanup distinction

If a process crashes after creating a temp file but before renaming it, the temp file is
permanent on disk. Cleanup at **shutdown** races with in-flight operations. Cleanup at
**startup** (during `Repository::open`) is safe — the process just acquired the repo, so
no concurrent writer is active at that moment.

When reviewing temp-file creation, ask: "what happens if this process crashes right here?"
Then check whether the codebase has startup cleanup for stale temps. If not, flag it.

---

## Pitfall 8: Not flagging the CAS delegation pattern

When a codebase has both `delete_foo` (non-transactional) and `delete_foo_cas` (fully
transactional, under lock, atomic writes), the non-CAS version should delegate to the CAS
version rather than duplicate the logic:

```rust
pub fn delete_marker(&self, name: &str) -> Result<Option<ChangeId>> {
    let state = self.get_marker(name)?;
    if state.is_some() {
        self.delete_marker_cas(name, RefExpectation::Any)?;
    }
    Ok(state)
}
```

Separate non-CAS implementations diverge over time and are usually less safe.
When you see parallel `foo` / `foo_cas` pairs, check whether the non-CAS version re-implements
the same logic rather than delegating.

---

## Pitfall 9: Accepting `Result<bool>` as safe for verification functions

```rust
// DANGEROUS: ? operator silently discards the inner bool
signer.verify(data, &sig)?;   // returns Ok(false) but ? treats it as Ok — invalid sig accepted!

// SAFE
fn verify(...) -> Result<(), SignerError>  // failure is always Err
```

`#[must_use]` helps at the call site but does nothing when the caller uses `?`. For any
function where the caller *must* check the result, encode failure as `Err`, not `Ok(false)`.

---

## Pitfall 10: Overlooking CLI positional args for file content

```rust
// This is always wrong for anything longer than a simple name
HookCommands::Install { name: String, content: String }
// content as a positional arg:
// - visible to all users via `ps aux` / /proc/<pid>/cmdline
// - logged in shell history
// - cannot contain newlines without complex quoting
// - hits ARG_MAX (~256 KB on macOS) for any real script
```

Search for `Commands` variants where a field is intended to hold file content. Flag them and
suggest `--from-file <PATH>` instead.

---

## Severity calibration reminders

| Finding | Correct severity |
|---------|-----------------|
| Operator precedence in crypto dispatch | CRITICAL |
| Encoder/decoder format mismatch | CRITICAL |
| `remove_dir_all` through symlinks | HIGH (data destruction) |
| No hash verification on read (FsStore, S3) | HIGH |
| TLS cert auth without CA validation | HIGH |
| Child process inherits full env | HIGH |
| Lock not held for full read→write sequence | HIGH |
| `unwrap_or_default()` on system time check (fail-open security) | HIGH |
| `fs::write` without temp-rename | MEDIUM |
| `is_ok()` on feature-flag env vars | MEDIUM |
| Missing `follow_links(false)` on WalkBuilder | MEDIUM (can become HIGH with symlinks to dirs) |
| Pack reader opened on every query | MEDIUM (performance) |
| `let _ =` on ref creation in import | MEDIUM |
| Stale temp files not cleaned at startup | LOW |
| Redundant double-read under lock | LOW |
