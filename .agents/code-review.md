# Code Review Focus Areas

When reviewing changes, pay attention to:

1. **Error handling** - Are errors propagated correctly? Are error messages helpful?

2. **Edge cases** - Empty inputs, missing files, concurrent access

3. **Spec compliance** - Does the implementation match SPEC.md?

4. **Testing** - Are there tests for new functionality?

5. **Performance** - Any obvious inefficiencies for large repositories?

6. **Security** - Path traversal, sensitive data exposure

---

## Review Methodology

### Step 1: Understand the type's identity contract before reporting violations

Before flagging any "identity" or "content-addressing" bug, read:
- The type's field definitions (what is the key/ID?)
- The `hash()` or `compute_hash()` method (what fields are hashed?)
- Any `PartialEq` / `Hash` impl

Heddle's `State` type has **two separate identifiers**: `change_id` (random, never derived from
content) and `content_hash` (excludes the `signature` field by design). Do not assume every
field is covered by the hash. See `review-pitfalls.md#pitfall-1` for the full story.

### Step 2: When you find a recurring anti-pattern, grep for all instances

The moment any of these are found, search the entire codebase:

| Pattern found | Search for |
|--------------|-----------|
| `remove_dir_all` on structured dir | `remove_dir_all` in all `.rs` files |
| `fs::write(` for a critical file | `fs::write(` in all `.rs` files |
| `WalkBuilder` missing `.follow_links(false)` | `WalkBuilder::new` in all `.rs` files |
| `is_ok()` on feature-flag env var | `env::var.*is_ok` in all `.rs` files |
| `unwrap_or_default()` in security path | `unwrap_or_default` in all `.rs` files |
| `let _ =` discarding a Result | `let _ =` in all `.rs` files |
| `Command::new` without `env_clear()` | `Command::new` in all `.rs` files |

Report every location. A pattern appearing in 4 places is a systemic issue.

### Step 3: Check every module — no exceptions

Common assumption: "utility" or "semantic" modules are low-risk. This is wrong.
Past reviews found HIGH severity issues in `hooks/` (env inheritance), `logging/` (feature
flag bypass), and `cli_args/` (content in CLI positional args). Every module gets a review.

### Step 4: Concurrency checklist for every mutation function

For any function that does (read → transform → write):
- [ ] Is a write lock held **before** the first read?
- [ ] Is the lock held for the **entire** sequence (not just the write step)?
- [ ] Is the lock OS-level (`fs2::FileExt`) so it covers multiple processes?

### Step 5: File I/O checklist

For every file write:
- [ ] Is it atomic? (write to temp → fsync → rename; never `fs::write` for critical files)
- [ ] For exclusive creation: `create_new(true)` not `exists()`-then-`write`?
- [ ] For reads: is the content hash re-verified against the filename/expected hash?

For every directory walk:
- [ ] `.follow_links(false)` set on `WalkBuilder`?
- [ ] `is_symlink()` checked before `is_dir()` in the loop body?
- [ ] `remove_dir` (non-recursive) used, not `remove_dir_all`?

### Step 6: Child process checklist

For every `Command::new(...)`:
- [ ] Does it call `env_clear()` before setting env vars?
- [ ] Are only known-safe env vars explicitly re-added (`PATH`, `HOME`, heddle-specific)?

### Step 7: Verify before accepting automated reviewer claims

If another agent or tool reports a finding, verify it against the actual code before
accepting it. Common false positives:
- "Signing breaks content-addressing" — check whether the ID is random or content-derived
- "Double-free / use-after-free" — Rust's borrow checker prevents most of these
- "Race condition" — check whether a lock is actually held

---

## Change Summary Log

As you implement changes, keep a running summary in `CHANGELOG.md` under the `Unreleased` section. Use short bullet points, group related changes, and keep entries readable without requiring code context.

Example format:

```
- Added: CLI state resolution for `HEAD~N` and short IDs.
- Fixed: undo/redo now moves HEAD and preserves thread attachment.
- Tests: expanded CLI integration coverage for state specs.
```
