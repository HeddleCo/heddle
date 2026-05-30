# heddle#325 — semver parsing: `semver` crate vs `cargo metadata`

**Status:** spike (decision doc). Impl tracked in **#81** (blocked by this).
**Scope:** the hand-rolled mini-semver inside `scripts/check-publish-pipeline.sh`
— the publish-pipeline asserter's internal-consumer / publishable-version
compatibility check. Verified against code 2026-05-30.
**Deliverable:** pick `semver` crate vs `cargo metadata`, justify, and hand #81
the concrete migration shape.

**Recommendation up front:** **Option A — the `semver` crate**, by moving the
version-compat check out of the embedded Python and into a `heddle-devtools`
subcommand (the same shape #103 already used to retire the
`check-no-silent-default-tree-load` regex asserter). Option B (`cargo metadata`)
does **not** address the asserter's actual need and is rejected — rationale in §3.

---

## §1 — The current hand-rolled implementation

The "mini-semver" is **not** Rust and **not** in any crate. It is Python embedded
in a bash heredoc inside `scripts/check-publish-pipeline.sh`, in the
"Internal-consumer / publishable-version compat" section. Three functions:

- `parse_ver_full(s)` — `scripts/check-publish-pipeline.sh:411-427`. Splits a
  version string into `(major, minor, patch, prerelease)`, stripping `+build`
  metadata and coercing non-numeric components to `0`.
- `_cmp_prerelease(a, b)` — `:430-457`. Implements semver §11 prerelease
  precedence (dot-separated identifiers, numeric-vs-alphanumeric ranking,
  shorter-prefix-loses).
- `satisfies(req, ver)` — `:460-575`. The core: evaluates Cargo caret semantics
  (`^`, bare-default-caret, `=` exact/partial, wildcard rejection, prerelease
  opt-in). Returns `True`/`False`, or `None` to signal an unsupported comparator
  shape that the caller treats as a hard error (`:689-692`).

### What it's used for

One thing only: for every internal workspace consumer of a *publishable* crate,
assert the consumer's declared `version = "…"` requirement is satisfied by that
crate's **current** version (`:673-697`). This guards the failure from
heddle#63 r2 (documented at `:338-351`): in-workspace `cargo build` is happy
because path deps override version reqs, but the push-to-main `cargo publish`
strips path deps and resolves against crates.io — so a stale `version = "0.2"`
against a bumped `0.3.0` source fails publish loud. The asserter catches that
**statically, at PR time**, before it reaches the publish job.

So the need is precisely: **does published version `V` satisfy requirement `R`
under Cargo's caret rules** — string parsing + comparison, not dep-graph
resolution.

### Self-tests

Seven `_selftest(...)` bundles, `:589-654` — 31 assertion cases total (the issue
calls this "the 28 self-tests"; the count drifted as edge cases were added).
Each bundle pins one Codex finding from the heddle#77 r1–r3 review rounds.

### Gaps / known divergences

The parser was hardened across **three** Codex review rounds (9 distinct
caret-semver edge cases — see #81's context). The residue:

1. **Prerelease range semantics are knowingly wrong vs Cargo.** The parser pins a
   prerelease requirement to the *exact* `(major, minor, patch)` tuple
   (`:546-549`): under it, `^1.0.0-alpha` rejects `1.0.1`. But Cargo (and the
   `semver` crate it uses) treats `^1.0.0-alpha` as `>=1.0.0-alpha, <2.0.0`, so
   `1.0.1` **does** satisfy it. #81's own issue body acks this: *"Round-3 acked
   one residual edge case (`1.0.0-alpha` allowed to update to `1.0.1` per Cargo)
   as out-of-scope."* The hand-rolled parser is divergent-by-design here, tolerated
   only because no heddle workspace crate currently uses a prerelease requirement.
2. **Wildcards rejected as policy, not parsed.** `1.2.*` / `*` return `None`
   (`:502-504`) — a deliberate workspace-convention choice ("caret only"), not a
   parsing limitation. Cargo accepts both.
3. **Comparator coverage is partial by construction.** `>`, `<`, `~`, and
   multi-clause (`,`) requirements all bail to `None` (`:530-531`). Fine while the
   workspace is caret-only, but it means the asserter cannot *evaluate* those
   shapes — it can only refuse them.
4. **Silent `0`-coercion of malformed components** (`:423-426`): a junk version
   like `1.x.0` parses to `(1,0,0)` rather than erroring.
5. **Maintenance cost is the real gap.** The git history is the indictment: a
   ~115-line hand-rolled caret evaluator that took three review rounds to
   stabilize and still ships a documented Cargo divergence. Every future
   prerelease/range need is another round.

---

## §2 — Option A: the `semver` crate

[`semver`](https://docs.rs/semver) is the dtolnay crate that **Cargo itself uses**
to parse and match version requirements. `VersionReq::parse(req)?.matches(&Version::parse(ver)?)`
is a one-line replacement for the entire `satisfies()` body.

### API fit — exact

The asserter's need (does `V` satisfy `R` under Cargo caret rules) is the crate's
primary use case. Caret-by-default, the `^`/`~`/`=`/`*` operators, partial-version
widening, build-metadata-ignored precedence (§10), and prerelease opt-in are all
implemented to Cargo's interpretation because it *is* Cargo's implementation.

### Correctness — verified by PoC

I ran all 31 hand-rolled self-test cases through `semver` 1.0.28 (PoC in §5).
Result: **28 identical, 3 divergent — and every divergence is a case where the
hand-rolled parser is the wrong one:**

| Case | hand-rolled | `semver` | Verdict |
|---|---|---|---|
| `satisfies("1.0.0-alpha", "1.0.1")` | `false` | `true` | **`semver` is Cargo-correct.** This is the exact edge case #81 acked as a known hand-rolled bug. Migration *fixes it for free.* |
| `satisfies("1.2.*", "1.2.5")` | `None` (reject) | `true` (match) | Policy difference, not correctness. `semver` correctly evaluates the wildcard; the "reject non-caret" convention becomes a separate one-line policy guard (see §4). |
| `satisfies("*", "1.0.0")` | `None` (reject) | `true` (match) | Same as above. |

So `semver` reproduces the hand-rolled behavior on every case that matters, and
*corrects* the one prerelease case the team already flagged as wrong.

### Dependency weight — effectively zero

`semver 1.0.28` is **already in `Cargo.lock`** (transitive via `rustc_version` →
`wasmparser`/`wit-parser` in the existing tree). It is already compiled in CI.
Adding `semver = "1"` to `crates/devtools/Cargo.toml` pulls in no new
transitive deps. The crate is `no_std`-capable, zero-dependency, and dtolnay-grade.
MSRV is a non-issue: the workspace is `edition = "2024"` (`Cargo.toml:28`), far
above `semver`'s MSRV.

### Ergonomics

The version-compat check becomes ~10 lines of Rust against a typed API, with the
31 self-tests reframed as ordinary `#[test]` cases. No bash/Python heredoc, no
hand-maintained caret arithmetic, no `None`-as-error sentinel threading.

---

## §3 — Option B: `cargo metadata`

`cargo metadata --format-version 1` emits the workspace dependency graph as JSON:
per-package `version`, and per-dependency `req` (the requirement string) plus a
fully **resolved** graph under `resolve`.

### Why it's the wrong tool here

The asserter is guarding against *exactly the thing `cargo metadata`'s resolution
hides*. The heddle#63 bug (`:338-351`) is that **in-workspace, path deps satisfy
the build regardless of the version requirement**. `cargo metadata`'s `resolve`
graph is produced by that same local resolver — it will report the path dep as
satisfied even when the declared `version` req is incompatible with what would be
published. So the resolved graph would be *green on the very mismatch we exist to
catch.* It cannot replace the check.

`cargo metadata` *does* expose the raw `req` and `version` strings (in the
unresolved `packages[].dependencies[]` array) — but those are the same strings the
script already reads straight from `Cargo.toml`. To decide satisfaction you must
**still** evaluate `req.matches(version)` yourself — i.e. you still need a semver
matcher. So `cargo metadata` could at most replace the `glob`+`tomllib` Cargo.toml
enumeration (~30 lines that already work), while leaving the actual hard part —
the caret evaluation — unsolved.

### Cost

Subprocess spawn + full-workspace metadata computation + JSON parse, versus the
current in-process TOML read. Adds a `cargo`-on-PATH and clean-workspace failure
mode for no benefit, against the issue's `<1s` asserter budget.

**Verdict:** B addresses dep-graph *resolution*; the asserter needs version-string
*satisfaction*. Different problem. Rejected. (Note: #81's body mused that B is
"more robust… delegating the spec to cargo means drift is impossible" — that
intuition holds for resolution questions, but the path-dep override makes
`cargo metadata` resolution blind to *this specific* check. `semver` — literally
Cargo's own matcher — delivers the "no drift" property without the blindness.)

---

## §4 — Recommendation & migration shape for #81

**Adopt the `semver` crate.** Move the version-compat check from the embedded
Python into a new `heddle-devtools` subcommand, mirroring the #103 precedent that
already turned `check-no-silent-default-tree-load` into a Rust devtool wrapped by
a thin shell script. This keeps the asserter's home consistent and lets the 31
self-tests live as real unit tests.

### Concrete steps

1. **Add the dep.** `semver = "1"` (or a `semver.workspace` entry) to
   `crates/devtools/Cargo.toml`. No lockfile churn — 1.0.28 is already resolved.
2. **New subcommand** `check-consumer-versions` in `crates/devtools/src/`
   (new module, registered in `crates/devtools/src/main.rs:14-22` alongside
   `check-no-silent-default-tree-load`). It:
   - enumerates `crates/*/Cargo.toml` (reuse `walkdir` + a toml parse; `toml` is
     not yet a devtools dep, so either add `toml` or shell the existing tomllib
     read — adding `toml` is cleaner and lighter than keeping Python),
   - builds the publishable-set from each `[package].publish` field (port
     `:406-408` verbatim — that policy is correct and unrelated to semver),
   - for each internal consumer→publishable dep, evaluates
     `VersionReq::parse(req)?.matches(&Version::parse(src_ver)?)`.
3. **Function-by-function replacement:**

   | Hand-rolled (Python) | Replacement |
   |---|---|
   | `parse_ver_full` `:411-427` | `semver::Version::parse` |
   | `_cmp_prerelease` `:430-457` | `semver::Prerelease` ordering (built in) |
   | `satisfies` `:460-575` | `VersionReq::parse(req)?.matches(&v)` |
   | `None`-as-unsupported sentinel | a `VersionReq::parse` `Err` is the natural error; report it as the failure |

4. **Caret-only policy guard (decide explicitly).** The hand-rolled parser
   *rejects* `*`, `>`, `<`, `~`, multi-clause. `semver` *evaluates* them. If the
   "workspace uses caret only" convention is still wanted, keep it as an explicit
   one-line lint *separate from* satisfaction: inspect `VersionReq.comparators`
   and fail any `Op` that isn't `Caret`/`Exact` (or whatever the convention
   permits). Per the no-backcompat / cleanest-replacement stance, **do not** port
   the `None` sentinel — make the policy its own check with its own message.
5. **Tests #81 must add** — port all 7 self-test bundles (`:589-654`) as
   `#[test]` cases, **with two deliberate changes** that record this spike's
   findings:
   - `^1.0.0-alpha` vs `1.0.1` flips `false → true` (the bug fix). Add a comment
     citing #325 so the change is auditable, not mistaken for a regression.
   - `1.2.*` / `*`: assert against the *chosen* policy guard from step 4, not
     against `satisfies` returning `None`.
   Plus a regression test reproducing the original heddle#63 mismatch
   (consumer `0.2` vs source `0.3.0` → fail).
6. **Delete** the Python `satisfies`/`parse_ver_full`/`_cmp_prerelease` block and
   its heredoc plumbing (`:359-730`) once the devtool is green. Per #81 AC: drop
   the hand-rolled `satisfies()`. The unrelated YAML structural checks
   (`:44-336`) stay in the shell script — out of scope.
7. **Wire CI** like the sibling asserter: a thin
   `scripts/check-consumer-versions.sh` doing
   `cargo run -p heddle-devtools -- check-consumer-versions`, invoked from
   `.github/workflows/release-pipeline-check.yml` (next to the existing
   `cargo build -p heddle-devtools` step at `:71`). Bench stays well under the
   `<1s` budget on warm caches (it's an in-process parse + match).

### Risk / divergence to flag in the #81 PR

The `^1.0.0-alpha → 1.0.1` behavior change is intentional and *more correct*, but
it is a behavior change. Call it out in the PR description as the headline diff so
Codex doesn't re-litigate it as a regression — it's the resolution of the edge
case heddle#77 r3 explicitly deferred.

---

## §5 — PoC (throwaway, not committed)

Run against `semver = "1"` (resolved 1.0.28), comparing the crate to all 31
hand-rolled self-test cases. 28 identical; the 3 differences are the rows in §2's
table. Reproduce with a throwaway `cargo new` bin:

```rust
use semver::{Version, VersionReq};

fn satisfies(req: &str, ver: &str) -> Option<bool> {
    let r = VersionReq::parse(req).ok()?;
    let v = Version::parse(ver).ok()?;
    Some(r.matches(&v))
}

fn main() {
    // Cargo-correct where the hand-rolled parser diverges:
    assert_eq!(satisfies("1.0.0-alpha", "1.0.1"), Some(true)); // hand-rolled: false (acked bug)
    assert_eq!(satisfies("1.2.*", "1.2.5"),       Some(true)); // hand-rolled: None (policy reject)

    // Identical to hand-rolled on every case that matters, e.g.:
    assert_eq!(satisfies("0", "0.5.2"),                Some(true));
    assert_eq!(satisfies("0", "1.0.0"),                Some(false));
    assert_eq!(satisfies("0.3", "0.3.0-alpha.1"),      Some(false)); // prerelease excluded
    assert_eq!(satisfies("=0.3.0-alpha.1", "0.3.0"),   Some(false));
    assert_eq!(satisfies("=0.3.0", "0.3.0+build"),     Some(true));  // build metadata ignored
    assert_eq!(satisfies("=4.2", "4.3.0"),             Some(false)); // partial = widens
    // ...full matrix (all 7 bundles) verified during the spike.
}
```

No production code is changed by this spike. No `semver` entry was added to any
committed `Cargo.toml`; the PoC lived in a throwaway crate outside the workspace
and is not part of this PR.

---

## §6 — Decision summary

| | `semver` crate (A) | `cargo metadata` (B) |
|---|---|---|
| Addresses the actual need (string satisfaction) | **Yes** — it's Cargo's own matcher | **No** — resolves the graph; path deps mask the mismatch |
| Correctness vs hand-rolled | 28/31 identical, 3× *more* correct | n/a (doesn't do matching) |
| New dependency weight | zero (already in lockfile) | none, but adds subprocess + cargo-on-PATH failure modes |
| Still need a semver evaluator? | no | **yes** — so B can't stand alone |
| Maintenance | typed API, no caret arithmetic | JSON shape + a matcher anyway |

**Pick A.** Migrate the version-compat asserter to a `heddle-devtools` subcommand
backed by the `semver` crate, port the self-tests, fix the acked prerelease
divergence, and keep the caret-only convention as an explicit separate policy
guard. #81 is the impl.
