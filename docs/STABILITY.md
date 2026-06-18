# Heddle 1.0 stability criterion

> **Status:** strawman. Every `<TBD: maintainer>` marker is a policy
> decision the maintainer must set before this document can gate the
> dogfood cutover. The grounded sections (current state, format
> inventory, what `cargo deny`/`cargo audit` already enforce) are
> citations against today's tree; the placeholders are the proposals
> the maintainer is being asked to confirm or override.

Heddle 1.0 is the version HeddleCo bets its own development on. That
bet is the entire point of the dogfood cutover (epic E7), and it
needs an objective gate: a checklist a reviewer can run against the
tree and either pass or fail, not a vibe.

This document defines that gate across five axes:

1. **Quality metrics** — coverage, performance, bug count, soak.
2. **API stability** — which crates' public Rust APIs are frozen at 1.0.
3. **Format stability** — what on-disk and on-the-wire formats promise.
4. **SemVer policy** — how breaking changes are signalled.
5. **Deprecation policy** — how long deprecated surfaces are kept.

Each axis names the concrete artefact in the tree that backs it, so
when the criterion fires "Heddle is not 1.0-ready" or "Heddle is
1.0-ready" the answer is grep-checkable.

## Where we are today (grounded baseline)

This section is fact, not policy. The maintainer doesn't tune it —
they tune the 1.0 targets the next sections propose against it.

- **Workspace shape.** 18 crates in
  [`Cargo.toml`](../Cargo.toml) `[workspace].members`. All sit at
  version `0.2.4` (last release: `0.2.4 - 2026-05-14`, see
  [`CHANGELOG.md`](../CHANGELOG.md)). Per-crate version sync is
  handled by [`release-plz.toml`](../release-plz.toml) workspace
  mode.
- **Public release vehicle.** Only `heddle-cli` is the user-facing
  CLI binary (workspace `default-members = ["crates/cli"]` in
  [`Cargo.toml`](../Cargo.toml), crates.io badge in
  [`README.md`](../README.md) points at `heddle-cli`). All 17
  publishable crates listed in
  [`release-plz.toml`](../release-plz.toml) are marked
  `release = true`, but only `heddle-cli`'s surface is curated as a
  product surface today — the rest publish because the workspace
  dependency graph requires it for `cargo publish` to resolve.
- **Coverage today.** `cargo llvm-cov` against the OSS feature set
  (`git-overlay,native,semantic,zstd`) ran on `main` at SHA
  `f0086130` on 2026-05-15 (workflow run
  [25934018627](https://github.com/HeddleCo/heddle/actions/runs/25934018627))
  and reported **72.02 % line coverage** workspace-wide
  (130 573 regions / 36 227 missed → 72.26 % region; 81 841 lines /
  22 896 missed → 72.02 % line; 7 872 fns / 2 399 missed → 69.52 %
  function). Per-crate breakdown is in the run's "Generate coverage
  summary" step; no per-crate floor is enforced today — the upload
  only fires when `CODECOV_TOKEN` is set, and the workflow does not
  fail the build on a coverage delta. The lift mechanism is
  [`.github/workflows/rust-tests.yml`](../.github/workflows/rust-tests.yml)
  (`coverage` job).
- **Performance baseline.** Two perf surfaces today, neither gated:
  - Snapshot-time microbench *cargo-test* harness at
    [`crates/cli/tests/performance.rs`](../crates/cli/tests/performance.rs)
    (asserts on phase timings; runs as part of the test suite).
  - A real Criterion benchmark crate-target at
    [`crates/cli/benches/local_ops.rs`](../crates/cli/benches/local_ops.rs)
    (declared via `[[bench]]` in `crates/cli/Cargo.toml`). Run with
    `cargo bench -p heddle-cli`. Not currently invoked by any CI
    workflow.
  No Bencher/Codspeed integration; no perf-regression gate in CI;
  Criterion exists only in `heddle-cli`, not in the workspace crates
  whose performance characteristics most matter for 1.0 (objects,
  repo, refs, oplog).
- **Soak / long-running tests.** No continuous long-running soak
  harness today. One large-blob *stress* fixture exists at
  [`crates/cli/tests/cli_integration/realworld_git.rs:242`](../crates/cli/tests/cli_integration/realworld_git.rs)
  (`realworld_git_large_binary_blob_stress_without_git_on_path`,
  gated `#[ignore]` and run only on demand via
  `HEDDLE_LARGE_BLOB_MB`), but there is no scheduled CI job that
  loops commit/undo over hours or days.
- **Supply-chain gate.** `cargo deny` + `cargo audit` run on every PR
  touching `Cargo.toml`/`Cargo.lock`/`deny.toml`/`audit.yml` and on a
  weekly Monday cron — see
  [`.github/workflows/audit.yml`](../.github/workflows/audit.yml) and
  [`deny.toml`](../deny.toml). License allow-list, banned-crates list
  (no OpenSSL/native-tls — rustls only), and RustSec advisory DB are
  all enforced.
- **Format-version constants in the tree** (non-exhaustive — these
  are the ones currently named `FORMAT_VERSION` / `VERSION`; see
  files listed for the authoritative source):
  - Packed oplog binary: `VERSION = 2`, magic `LMOPLOG\0` in
    [`crates/oplog/src/oplog/packed_oplog.rs:21`](../crates/oplog/src/oplog/packed_oplog.rs).
  - Operation dedup file: `DEDUP_FORMAT_VERSION = 1` in
    [`crates/repo/src/operation_dedup.rs:36`](../crates/repo/src/operation_dedup.rs).
  - Operation-index bucket: `INDEX_FORMAT_VERSION = 1` in
    [`crates/refs/src/refs/operation_index.rs:38`](../crates/refs/src/refs/operation_index.rs).
  - Ref-summary index header: `"heddle-ref-summary-v1"` in
    [`crates/refs/src/refs/ref_summary_index.rs:14`](../crates/refs/src/refs/ref_summary_index.rs).
  - State-context blob: `FORMAT_VERSION = 2` in
    [`crates/objects/src/object/state_context.rs:163`](../crates/objects/src/object/state_context.rs).
  - Per-object sidecar blobs at `FORMAT_VERSION = 1`:
    `DiscussionsBlob`, `RiskSignalBlob`, `RedactionsBlob`,
    `ReviewSignaturesBlob`, `FileProvenance`, `StructuredConflict`
    (`crates/objects/src/object/*.rs`).
- **Wire protocol.** Package `heddle.v1` in
  [`crates/grpc/proto/heddle/v1/service.proto`](../crates/grpc/proto/heddle/v1/service.proto)
  defines 14 gRPC services (`RepoSyncService`, `HostedUserService`,
  `AuthService`, `ContentService`, `RepoEventService`,
  `ThreadWorkflowService`, `ReviewService`, `FeedService`,
  `StateReviewService`, `DiscussionService`, `SignalService`,
  `OperationLogQueryService`, `TransactionService`, `HookService`).
  The package version segment is `v1`; the file does not declare a
  separate semantic version.
- **Content-addressing primitives.** `ContentHash` is a 32-byte
  BLAKE3 digest
  ([`crates/objects/src/object/hash.rs:13`](../crates/objects/src/object/hash.rs));
  `ChangeId` is a 128-bit random identifier
  ([`crates/objects/src/object/hash.rs:99`](../crates/objects/src/object/hash.rs)).
- **Compatibility posture today.** From
  [`AGENTS.md`](../AGENTS.md) §"Compatibility": "Heddle is still
  moving quickly. Prefer the current model over preserving legacy
  behavior. Do not add backwards-compatibility shims unless the user
  explicitly asks for them." That posture is the thing 1.0 ends.

## 1. Quality metrics

The 1.0 gate is a conjunction: **every** metric below must clear its
threshold on a green CI run before the cutover. A single regressing
crate or a single open S0 bug fails the gate. This is intentional —
the dogfood bet is whole-product, not best-crate.

### 1.1 Test coverage

**Workspace floor.**

<TBD: maintainer> — current workspace line coverage is **72.02 %**
(run 25934018627). Proposed 1.0 floor: **80 % line / 75 % function**,
measured by `cargo llvm-cov --workspace --features
git-overlay,native,semantic,zstd` (the same invocation the CI job
already uses).

Tradeoff: 80 % is a credible "the core paths are exercised" number
for a 0.2 → 1.0 step without forcing test theatre on glue/CLI
plumbing crates where the integration tests in
[`crates/cli/tests/`](../crates/cli/tests/) carry the real signal.
Setting it higher (e.g. 90 %) would force test additions in code
that's better verified end-to-end; setting it lower leaves the
floor below what we already have, which doesn't gate anything.

**Per-crate floor.**

<TBD: maintainer> — proposed: **65 % line per crate** for the
storage-format and identity-bearing crates (`heddle-objects`,
`heddle-refs`, `heddle-oplog`, `heddle-repo`, `heddle-crypto`,
`heddle-wire`, `heddle-grpc`), and **no floor** for the rest. The
distinction is "if this crate is wrong, on-disk or on-wire data is
wrong" vs. "if this crate is wrong, the CLI prints a worse string".
The integration-test crates and CLI dispatch crates are better
exercised by `cli_integration.rs`, `core_functionality.rs`, and
`comprehensive.rs` than by unit coverage.

Tradeoff: a uniform per-crate floor would force coverage in
`heddle-cli` (which is mostly thin command dispatch and `render_*`
output) where the meaningful tests live higher up the stack.

**How the gate is enforced.**

<TBD: maintainer> — proposed: add a `coverage-floor` step to the
existing `coverage` job in
[`.github/workflows/rust-tests.yml`](../.github/workflows/rust-tests.yml)
that fails when `cargo llvm-cov report --summary-only` reports below
the floor. Today the job uploads to Codecov but does not fail the
build on a delta. The floor enforcement is a one-line `awk` test on
the existing `coverage-summary.txt` artifact; no new tool is needed.

### 1.2 Performance budgets

<TBD: maintainer> — no perf-regression gate exists today. Proposed
1.0 envelope, measured at p50 on the standard CI runner
(`blacksmith-4vcpu-ubuntu-2404-arm` per the workflow), against a
fixed corpus:

| Operation | Corpus | Budget (p50) |
|---|---|---|
| `heddle status` | 10 k tracked files, clean tree | <TBD: maintainer> ms |
| `heddle capture` | 100 changed files | <TBD: maintainer> ms |
| `heddle log --oneline -n 1000` | 10 k-state history | <TBD: maintainer> ms |
| `heddle adopt --ref <branch>` | linux.git head | <TBD: maintainer> s |
| Snapshot 1 000 small files | synthetic | <TBD: maintainer> ms |

Tradeoffs:

- **Absolute budgets vs. ratchet.** A ratchet ("don't get more than
  10 % slower than the previous run") is easier to land — it doesn't
  require establishing absolutes — but it doesn't answer "is Heddle
  fast enough for the bet?". An absolute budget does, at the cost of
  picking numbers somewhat arbitrarily on the first pass. Proposed:
  start with the ratchet (10 % regression on the existing
  `performance.rs` snapshot timings), promote to absolutes as the
  bench corpus stabilises.
- **Where the bench harness lives.** Two seeds exist:
  [`crates/cli/tests/performance.rs`](../crates/cli/tests/performance.rs)
  (cargo-test, timing-assertion shape, runs every CI) and
  [`crates/cli/benches/local_ops.rs`](../crates/cli/benches/local_ops.rs)
  (real Criterion, runs only on demand via `cargo bench`). Neither
  covers `crates/objects`, `crates/repo`, `crates/refs`,
  `crates/oplog` — the formats most sensitive to 1.0 perf claims.
  Proposed: keep `performance.rs` as the cheap test-suite gate;
  expand `local_ops.rs` (or peer Criterion benches under each
  format crate) into a CI-invoked `cargo bench --no-run` smoke check
  + a nightly perf-regression workflow that runs the suite and
  compares against a stored baseline.
- **What gets gated, what gets observed.** Not every CLI verb needs
  a budget. Proposed: budget the verbs in the AGENTS.md "core thread
  workflow" (`status`, `capture`, `commit`, `log`, `show`, `start`,
  `merge --preview`) plus explicit Git-adapter import/export/sync
  entry points; track the rest as observed timings without a
  fail-the-build gate.

### 1.3 Maximum known-bug count by severity

<TBD: maintainer> — proposed thresholds at the 1.0 gate, evaluated
on open issues in `HeddleCo/heddle` with the corresponding label:

- **S0 (data loss / corruption / silent wrong answer):** **0 open**.
  Hard gate. No exceptions.
- **S1 (crash, hang, or feature-broken on a documented path):**
  **<TBD: maintainer — proposed ≤ 2 open with workaround documented
  in the issue>**. Open S1s with no workaround block the gate.
- **S2 (UX regression, recoverable error, cosmetic-but-confusing):**
  **<TBD: maintainer — proposed ≤ 10 open>**.
- **S3 (papercut, minor copy nit):** no count limit, but each must be
  triaged (a label, not an unlabelled "Backlog" card).

Tradeoff: tighter S1 (e.g. zero open) makes the gate punitive —
every CLI verb has at least one edge case, and the marginal harm of
"`heddle diff` is slow on a 10 GB tree" is not "do not bet the
company on this". Looser S0 (e.g. "documented") is not acceptable —
a data-corruption bug with a documented workaround is still a data-
corruption bug.

The severity rubric itself needs writing (it does not exist in the
repo today): proposed home is `docs/SEVERITY.md`, owned by
maintainer, written before this gate fires.

### 1.4 Soak / long-running test

<TBD: maintainer> — no soak harness exists today. Proposed: a
**24-hour** soak that runs `heddle commit` / `heddle undo` /
`heddle undo --redo` in a loop against a synthetic
repository, asserts that no `fsck` errors appear at the end, asserts
that the oplog is replayable from zero, and asserts that
`heddle maintenance gc --prune` does not delete anything reachable.

Tradeoffs:

- **24 h vs. 7 days.** 24 h catches the obvious unbounded-growth and
  flush-ordering bugs. 7 days catches the subtle ones (slow leaks,
  fragment compaction). Proposed: 24 h as the gate, 7 days as
  observed/reported but not blocking; promote to gate after the
  first 1.0.x release that ships clean.
- **Where it runs.** Daily cron on the same `blacksmith-4vcpu`
  runner as `rust-tests.yml`. Cheaper than a dedicated runner; the
  runner cost dominates over the rare cancellation.
- **What's exercised.** Proposed: commit/undo/redo loop plus a
  `bridge git sync` loop against a fixture upstream. The bridge is the
  most state-rich adapter code path and benefits most from a long run.

## 2. API stability commitment

"API stability" here means: **a downstream crate that depends on
this crate's published `0.x` or `1.0` release can upgrade across
patch and minor versions without source-breaking changes to the
items it consumes.** It does **not** mean "the crate's surface is
final and cannot grow."

### 2.1 What's stable at 1.0

<TBD: maintainer> — proposed split. The maintainer decides which
crates are committed to the stability contract and which remain
`0.x` indefinitely.

**Proposed `1.0` (stable Rust API):**

- `heddle-objects` — `ContentHash`, `ChangeId`, blob/tree/state
  types, format constants. Anything that downstream tooling or
  alternative storage backends must agree on.
- `heddle-wire` — native Heddle wire/protocol types, transfer
  planners, and thin Rust adapters. The protobuf/gRPC boundary itself
  is gated separately (§3.4); this is the Rust-side handle on it.
- `heddle-grpc` — service trait shapes and client stubs, so
  downstream gRPC clients can compile against a stable surface.

Rationale: these three are the surfaces that an independent
implementer or an alternative server (e.g. `weft`, which already
depends on them) cannot route around. Pinning them is what makes
ecosystem investment safe.

**Proposed remain `0.x` (no API-stability commitment):**

- `heddle-cli` — the CLI binary's *Rust* surface is not the
  product; the CLI surface (verbs, flags, JSON shapes) is, and
  that's covered by `heddle doctor docs` /
  `heddle doctor schemas` plus the conventions in
  [`CONTRIBUTING.md`](../CONTRIBUTING.md).
- `heddle-cli-shared`, `heddle-client`, `heddle-daemon`,
  `heddle-devtools`, `heddle-ingest`, `heddle-mount`,
  `heddle-oplog`, `heddle-refs`, `heddle-repo`, `heddle-review`,
  `heddle-semantic`, `heddle-state-review`, `heddle-crypto`,
  `weft-client-shim` — internal-shaped today, may absorb churn from
  ongoing work (hosted control plane, semantic diff evolution,
  daemon redesigns). Promoting any of these to 1.0 is a follow-up
  decision after the first 1.0.x release proves the stability
  contract is workable on the three above.

Tradeoff: a wider 1.0 scope (e.g. "all 18 crates frozen") is a
stronger ecosystem signal but raises the cost of every internal
refactor — and most of these crates are pre-product-shaped. A
narrower scope is honest about which surfaces are actually being
consumed externally.

### 2.2 What "stable" means concretely

For the crates in §2.1's stable list:

- **Removing or renaming a `pub` item is a breaking change.** Major
  version bump required (§4).
- **Changing a `pub` function or method signature is a breaking
  change.** Adding optional fields to a `#[non_exhaustive]` struct
  is not, by Rust SemVer convention; adding a required field to a
  struct without `#[non_exhaustive]` is.
- **Items marked `#[doc(hidden)]` are not part of the stable surface
  even if they are `pub`.** Consumers relying on `doc(hidden)` items
  do so at their own risk.
- **MSRV (minimum supported Rust version) bumps are minor.** The
  current MSRV is Rust 1.85 per [`README.md`](../README.md)
  §Prerequisites; bumps are announced in the CHANGELOG.

### 2.3 CLI surface stability

The CLI is the user-facing product. Its stability commitments are
already partially enforced by the meta-tests in
[`crates/cli/tests/`](../crates/cli/tests/) — see
[`CONTRIBUTING.md`](../CONTRIBUTING.md) §"Tests new contributors
meet first":

- `render_lint.rs` — `println!`/`print!` only inside `render_*` /
  `write_*` functions. The render ratchet ensures stdout shape is
  intentional.
- `tier_coverage.rs` — every `Commands` enum variant must be
  classified `Everyday` / `Advanced` / `Hidden`. Hidden verbs are
  not part of the stability commitment.
- `op_id_coverage.rs` + `idempotency_lint.rs` — state-changing
  verbs must thread `--op-id` and behave idempotently on replay.
- `heddle doctor docs` — flag names referenced in tracked markdown
  must match the live binary.
- `heddle doctor schemas` — every documented schema verb registered
  in the command contract table
  ([`crates/cli/src/cli/commands/command_catalog.rs`](../crates/cli/src/cli/commands/command_catalog.rs))
  must produce a schema, and the JSON samples in
  [`docs/json-schemas.md`](json-schemas.md) must validate against
  the runtime registry. The output shapes are contracts; changes
  must be additive (`null` for missing fields, empty arrays
  explicit).

<TBD: maintainer> — for 1.0, proposed additional commitments:

- Tier-`Everyday` verb names + their documented flags are frozen.
  Renames require a major bump and a deprecation cycle (§5).
- Tier-`Advanced` verbs may rename in a major bump without a
  deprecation cycle, but the JSON output schema (when `--output json`
  is passed) is frozen on the same terms as `Everyday`.
- Tier-`Hidden` verbs are explicitly *not* covered — they exist for
  internal use and may change at any time.

## 3. Format stability

These are the formats Heddle promises to keep readable across 1.0.x
patch and minor versions. "Readable" means a Heddle ≥ 1.x.y binary
can read data written by any Heddle 1.x.z binary where z ≤ y, and
that any breaking format change goes through the SemVer process in
§4.

### 3.1 Object format

The on-disk object store (`.heddle/objects/`) holds content-
addressed blobs, trees, and states, each addressed by a 32-byte
BLAKE3 digest (`ContentHash`, defined at
[`crates/objects/src/object/hash.rs:13`](../crates/objects/src/object/hash.rs)).
Pack files are produced by the pack builder under
[`crates/objects/src/store/pack/`](../crates/objects/src/store/pack/).

**1.0 commitment.**

<TBD: maintainer> — proposed:

- The BLAKE3 content-addressing scheme is frozen at 1.0. A change
  to the hash function (e.g. BLAKE3 → BLAKE3-keyed) is a major-
  version event for the whole product and requires a documented
  migration.
- The pack file binary layout (header, object framing, checksum) is
  frozen at 1.0.x for forward-readability; new pack-level features
  ride a version byte in the pack header, and 1.x readers must
  refuse unknown major-version packs cleanly (no silent
  misinterpretation).
- The set of object types is *open* — adding a new object type is
  not a breaking change as long as old readers refuse it cleanly.
- The encoding of an existing object type's body is frozen.

Tradeoff: an "open object type set" lets us add (for example) new
sidecar shapes without major bumps. A "closed" object set is
stronger but conflicts directly with the way new product features
are landing today (see the 0.2.x sidecar additions in
[`CHANGELOG.md`](../CHANGELOG.md): redactions, discussions, risk
signals, structured conflicts, state context, state reviews — each
of which added a new sidecar blob type at `FORMAT_VERSION = 1`).

### 3.2 Sidecar blob formats

Sidecar blobs are versioned individually via a `FORMAT_VERSION` byte
in their encoded body. The current inventory (non-exhaustive — see
[`crates/objects/src/object/`](../crates/objects/src/object/) for
the authoritative list):

| Blob | File | Current `FORMAT_VERSION` |
|---|---|---|
| `StateContext` | `state_context.rs` | 2 |
| `DiscussionsBlob` | `discussion.rs` | 1 |
| `RiskSignalBlob` | `risk_signal.rs` | 1 |
| `RedactionsBlob` | `redaction.rs` | 1 |
| `ReviewSignaturesBlob` | `state_review.rs` | 1 |
| `FileProvenance` | `state_provenance.rs` | 1 |
| `StructuredConflict` | `structured_conflict.rs` | 1 |

**1.0 commitment.**

<TBD: maintainer> — proposed:

- A bump to a sidecar's `FORMAT_VERSION` must be backwards-
  compatible within a 1.0.x minor: a v2 reader reads v1 blobs, and
  a v1 reader refuses v2 blobs with a typed error (the current code
  already does the latter — see e.g.
  [`state_context.rs:173`](../crates/objects/src/object/state_context.rs)).
- A non-backwards-compatible sidecar change is a major-version
  event.

### 3.3 Ref format

Refs are stored under `.heddle/refs/`. The packed-refs encoding and
the `ref_summary_index` header (`"heddle-ref-summary-v1"` at
[`crates/refs/src/refs/ref_summary_index.rs:14`](../crates/refs/src/refs/ref_summary_index.rs))
identify the format generation.

**1.0 commitment.**

<TBD: maintainer> — proposed: the ref-summary index header version
suffix (`-v1` → `-v2`) gates compatibility. 1.0.x readers must
accept any `-v1` index; 1.x.y readers may produce a higher version
suffix only if older readers cleanly reject it (the current
`ref_summary_index.rs` does so).

The thread / marker / HEAD ref-name conventions (no embedded NULs,
no `..`, no leading `-`, etc.) are part of the stability surface;
they're enforced in `crates/refs/` today but should be enumerated
explicitly before 1.0. <TBD: maintainer — explicit enumeration of
ref-name rules to live in this doc or in a sibling
`docs/REF_FORMAT.md`>.

### 3.4 Oplog format

The packed oplog binary at `.heddle/oplog/` is identified by magic
bytes `LMOPLOG\0` and an in-file `VERSION` field, currently `2`
([`crates/oplog/src/oplog/packed_oplog.rs:21`](../crates/oplog/src/oplog/packed_oplog.rs)).
The current code comment explicitly states: "`2` adds the W1 fields:
each entry now encodes its `actor` (principal name + email,
length-prefixed UTF-8) and `operation_id` (tag byte + optional
16-byte UUID). Pre-W1 v1 files are rejected — there are no live
deployments to migrate from."

**1.0 commitment.**

<TBD: maintainer> — proposed:

- 1.0 ships at oplog `VERSION = 2` (or whatever VERSION the 1.0
  cut takes). Pre-1.0 oplog versions are not required to be
  readable post-1.0; the cutover replays the oplog into the new
  format at upgrade time.
- Any oplog version bump within 1.x.y must be forward-compatible
  with a 1.0.x reader's "refuse unknown version" path (the current
  code already enforces this — see
  [`packed_oplog.rs:102`](../crates/oplog/src/oplog/packed_oplog.rs)).

### 3.5 Operation dedup and operation index

`DEDUP_FORMAT_VERSION = 1`
([`crates/repo/src/operation_dedup.rs:36`](../crates/repo/src/operation_dedup.rs))
and `INDEX_FORMAT_VERSION = 1`
([`crates/refs/src/refs/operation_index.rs:38`](../crates/refs/src/refs/operation_index.rs))
gate the dedup-store and operation-index files. Both are derived
state — they can be rebuilt from the oplog — so the 1.0 commitment
here is weaker: <TBD: maintainer — proposed: format bumps are
allowed within 1.0.x as long as the on-upgrade rebuild path is
exercised by the test suite and runs in O(oplog size).>

### 3.6 Wire protocol

The gRPC services in
[`crates/grpc/proto/heddle/v1/service.proto`](../crates/grpc/proto/heddle/v1/service.proto)
sit under package `heddle.v1`. The package version segment (`v1`)
is the wire-stability boundary: 1.0 servers and clients communicate
on `heddle.v1`, and any non-backwards-compatible wire change moves
to `heddle.v2` with both packages served simultaneously during the
transition.

**1.0 commitment.**

<TBD: maintainer> — proposed:

- Adding a new RPC, a new service, or a new optional field to an
  existing message is **not** a breaking wire change (proto3 default
  semantics handle missing fields).
- Removing or renumbering a field, changing a field's wire type, or
  removing an RPC is a breaking wire change and moves to `heddle.v2`.
- `v1` is supported on the server for **<TBD: maintainer — proposed
  12 months>** after `v2` is introduced.

## 4. SemVer policy

Heddle follows [SemVer 2.0.0](https://semver.org/) once 1.0 ships,
with these clarifications.

### 4.1 What is a breaking change

For the crates in §2.1's stable list:

- Removing or renaming a `pub` item.
- Changing a `pub` function or method signature in a non-additive
  way.
- Adding a required field to a non-`#[non_exhaustive]` struct.
- Adding a variant to a non-`#[non_exhaustive]` enum.
- Bumping the major-version segment of a format identifier (oplog
  `VERSION`, sidecar `FORMAT_VERSION`, ref-summary header,
  `heddle.vN` proto package).
- Removing a Tier-`Everyday` CLI verb or any flag it documents.
- Changing the wire shape of any JSON output marked as stable by
  `heddle doctor schemas`.

For the crates **not** in §2.1's stable list, there is no breaking-
change contract within 0.x — they may break their Rust API on any
release.

### 4.2 How breaking changes are signalled

- **In the CHANGELOG.** Every breaking change gets a `BREAKING:`
  line under the relevant version heading in
  [`CHANGELOG.md`](../CHANGELOG.md). The existing changelog already
  uses Keep-a-Changelog headings (`### Added` / `### Changed` /
  etc.); add `### Breaking` as the dominant heading on any release
  that contains one.
- **In the version number.** Breaking changes bump the major
  version (1.x → 2.0). Patch and minor releases must not contain
  breaking changes to stable surfaces.
- **In the deprecation cycle.** See §5.

### 4.3 Workspace-wide versioning

All publishable crates currently move in lockstep via
[`release-plz.toml`](../release-plz.toml) workspace mode. This is
intentional: it keeps the matrix of "which combinations of crate
versions are valid" small. <TBD: maintainer — proposed: keep this
lockstep through 1.0 and re-evaluate per-crate independence as a
follow-up after the first 1.0.x release.>

Tradeoff: per-crate independent versioning lets a hot fix to
`heddle-objects` ship without bumping every other crate. Lockstep
versioning means every fix releases the whole workspace —
operationally simpler, but a 17-crate release every time something
small changes.

## 5. Deprecation policy

The 1.0 contract is "things that were stable stay stable for a
predictable window after they're scheduled for removal." This
section sets that window.

### 5.1 Deprecation cycle

When a stable item is to be removed:

1. **Mark it deprecated.** For Rust items: `#[deprecated(since =
   "1.x.y", note = "...")]`. For CLI verbs/flags: emit a stderr
   warning on every invocation (stderr is unrestricted per
   [`CONTRIBUTING.md`](../CONTRIBUTING.md) §"Render discipline").
   For format constants: bump the format-version and ship a writer
   that produces the new version but a reader that still accepts
   the old.
2. **Announce in the CHANGELOG.** `### Deprecated` heading under the
   release that introduces the deprecation. Include the planned
   removal release.
3. **Wait.**
4. **Remove in a major release.** Bump the major-version segment.
   Removal in a minor or patch release is not permitted for stable
   surfaces.

### 5.2 Minimum support window

<TBD: maintainer> — proposed:

- **Rust APIs in stable crates (§2.1):** **2 minor releases** OR
  **6 calendar months**, whichever is longer. Removal lands in the
  first major release after both conditions are met.
- **Tier-`Everyday` CLI verbs / flags:** **3 minor releases** OR
  **9 calendar months**. CLI consumers (scripts, agents) are the
  hardest to migrate and the slowest to notice — a longer cycle is
  proportionate.
- **Tier-`Advanced` CLI verbs / flags:** same as Rust APIs: 2
  minors / 6 months.
- **JSON output fields covered by `heddle doctor schemas`:** same
  as Tier-`Everyday`. JSON shape changes are silently breaking for
  agents and need the longer window.
- **Wire-protocol fields (proto3 optional):** the field can be
  marked `[deprecated = true]` immediately, but server-side
  handling must continue for the same window as the `heddle.v1`
  package itself (§3.6).
- **Sidecar / oplog format versions:** at least one minor release
  after the new format ships before the old format reader can be
  removed. The reader removal is itself a breaking change (§4.1).

Tradeoffs: a shorter window (e.g. one minor) is operationally
cheaper but burns confidence quickly when consumers can't keep up. A
longer window (e.g. 4 minors) is friendlier to consumers but
accumulates deprecated code paths that are themselves a
maintenance liability — and every deprecated path is a place where
the type system can't tell you you've made a mistake.

### 5.3 Emergency exceptions

Some changes can't wait. The CHANGELOG must be explicit about which
of these applies:

- **Security advisories** — a RustSec or in-house security finding
  that's actively exploitable. Patch immediately; document in the
  CHANGELOG as `### Security`. No deprecation cycle.
- **Data-corruption bugs** — a code path that can write data the
  reader can't recover. Patch immediately; document in the
  CHANGELOG. No deprecation cycle.
- **Spec ambiguity** — two implementations disagree on the meaning
  of a stable surface. Pick one; document the resolution in the
  CHANGELOG; treat the loser as deprecated under the normal cycle.

## How this criterion is used

The 1.0 cutover gate (epic E7) passes when **all of the following
are simultaneously true on a single green CI run on `main`**:

- §1.1 coverage thresholds met workspace-wide and per stable crate.
- §1.2 performance budgets all green (or, on the ratchet variant,
  no >10 % regression vs. the previous green run).
- §1.3 known-bug counts within thresholds, with severity labels
  audited within the last 7 days.
- §1.4 soak test green for the most recent 24-h run.
- §2 stable crates compile and pass tests with no items marked
  `#[doc(hidden)]` that downstream consumers (`weft`) depend on.
- §3 format-version constants match the 1.0 baseline this document
  fixes; any in-flight bumps have shipped to a 0.2.x first.
- §4 the CHANGELOG has a `## 1.0.0` heading with no `### Breaking`
  entries deferred from earlier 0.2.x releases.
- §5 there are no deprecated items past their minimum-support
  window still present in the stable crates.

A reviewer can run this checklist in a single sitting. That's the
point.

## Open follow-ups

- The severity rubric `docs/SEVERITY.md` does not exist yet and is
  referenced from §1.3. Filing follow-up issue: <TBD: maintainer to
  open or assign>.
- The ref-name rule enumeration referenced in §3.3 is not yet
  written. Filing follow-up issue: <TBD: maintainer to open or
  assign>.
- A standing perf-corpus repository (the input to §1.2's bench
  budgets) does not yet exist. Filing follow-up issue: <TBD:
  maintainer to open or assign>.
- The decision in §2.1 about which crates promote to 1.0 may
  change after maintainer review. Whichever set is chosen, the
  `[package.metadata.docs.rs]` block in each stable crate should
  add `rustdoc-args = ["--cfg", "docsrs"]` so docs.rs builds get
  the full stable surface. <TBD: maintainer to confirm.>
