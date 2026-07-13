# Packed-refs scale stress recipe

Reproducible recipe for the known packed-refs degradation threshold (~**10k+**
refs). Labels follow program truth rules.

| Surface | Status |
|---------|--------|
| Product on-disk format: text `packed-refs` | **Shipped** (degrades ~10k+) |
| Criterion scale bench (10k / 50k / 100k) | **Shipped** as stress tool (not a CI oracle gate) |
| Reftable binary format in product `RefManager` | **Planned** / not implemented |
| Continuous CI gate at 10k+ refs | **Unknown** / not claimed |
| Product-path unit stress (2k threads pack+reload) | **Shipped** as CI unit test (`packed_refs_product_stress_two_thousand_threads`) |

Cross-check: `AGENTS.md` known limitations, `GAP_MAP.md` **L2**,
`PLATFORM_MATRIX.md`, `docs/design/reftable-spike.md`.

---

## Why this exists

Packed-refs is a single line-oriented file under `.heddle/refs/packed-refs`.
Cold load parses the whole file into an in-memory model; single cold lookups
pay full parse + map lookup. That is correct and durable for typical repos, but
**latency and rewrite cost grow with ref count**. Product docs already say
packed-refs **degrades ~10k+ refs**. Reftable is a spike prototype only — it is
**not** the production ref store.

This recipe documents how to **measure** that curve without treating the bench
as a false “oracle pass” on tiny fixtures.

---

## Fixture / ref counts

| Scale | Role |
|-------|------|
| **10_000** | Degradation threshold called out in `AGENTS.md` / GAP_MAP L2 |
| **50_000** | Mid stress — confirms trend continues |
| **100_000** | Upper bench size in Criterion spike |

Fixture shape in the bench (not product CLI):

- Threads only (markers = 0); names like `feature/branch-000123`, `topic/…`,
  `release/…`, `user/alice/…`, `user/bob/…`
- Deterministic `ChangeId`s from a fixed multiplier
- On-disk paths under a tempdir: `packed-refs` (text) and `reftable` (prototype bytes)

Source of truth:

- Bench: [`crates/refs/benches/reftable_vs_packed.rs`](../../crates/refs/benches/reftable_vs_packed.rs)
- `const SIZES: &[usize] = &[10_000, 50_000, 100_000];`

Unit/integration tests under `crates/refs` exercise packed-refs **correctness**
on **small** fixtures (handful of threads). Those are **not** scale stress.

---

## Commands

### 1. Preferred: Criterion bench (checked-in, equal sizes)

From repo root:

```bash
# Full stress groups at 10k / 50k / 100k
cargo bench -p heddle-refs --bench reftable_vs_packed

# Optional: filter a single group (Criterion filter)
cargo bench -p heddle-refs --bench reftable_vs_packed -- cold_load
cargo bench -p heddle-refs --bench reftable_vs_packed -- cold_single_lookup
cargo bench -p heddle-refs --bench reftable_vs_packed -- append_one_persist
```

Wrapper script (prints expected graceful behavior, does not invent thresholds):

```bash
bash scripts/program/packed-refs-stress-recipe.sh
# or: bash scripts/program/packed-refs-stress-recipe.sh --filter cold_load
```

**What is measured** (see bench module docs):

| Group | Meaning for product packed-refs path |
|-------|--------------------------------------|
| `cold_load` | Full file read + `PackedRefsModel::parse` (per-process unavoidable cost) |
| `cold_single_lookup` | Open fresh + find one name (full parse + map get for packed-refs) |
| `warm_lookup_x1000` | Model already loaded; 1000 random gets (hashmap is strong here) |
| `list_all` | Enumerate every ref name (status / sync style work) |
| `append_one_persist` | Add one ref + rewrite whole file via `write_file_atomic` (same durability path as production `PackedRefs::save`) |

Stderr also prints on-disk byte sizes at each `n` for packed vs reftable prototype.

### 2. Product CLI path (correctness / ops, not equal-work scale gate)

Product consolidates loose refs into packed-refs via maintenance GC:

```bash
# After a repo has many loose threads/markers:
heddle maintenance gc
# Implementation calls repo.refs().pack_refs() after packing objects
# (crates/cli/src/cli/commands/gc.rs).
```

Inspect ref counts / packed file size:

```bash
# Performance inspection includes packed_refs_present / packed_refs_bytes
# (repository_maintenance / inspect_performance path).
heddle doctor
```

**There is no supported product command that fabricates 10k threads for stress.**
For scale numbers, use the Criterion bench (above), not a hand-rolled CLI loop
in CI as an “oracle.”

API-level product path (library tests / tooling):

```text
RefManager::pack_refs()  →  PackedRefs::save(packed-refs path)
Loose thread/marker files removed after successful pack.
```

---

## Expected graceful behavior

Honest expectations for the **product** packed-refs path at 10k+:

| Outcome | Expectation |
|---------|-------------|
| Correctness | Reads, lookups, list, pack, loose-overrides-packed continue to work |
| Degradation | Cold load / cold single lookup / full rewrite latency grow with N (roughly linear in file size for parse + rewrite) |
| Failure mode | Prefer **slow** over silent data loss; do not expect OOM/timeout as the documented product contract — but a resource-constrained host may timeout a 100k Criterion sample; that is a host limit, not a format corruption signal |
| Durability | `append_one_persist` / production save use atomic write + fsync (+ parent dir sync on platforms that support it) |
| Memory | Full-file model in memory after load — large N means large resident parse cost |

From the reftable **spike** decision doc (`docs/design/reftable-spike.md`):

- Prototype reftable wins cold single-ref lookup dramatically at 100k in bench
  numbers (~orders of magnitude vs packed-refs in the spike write-up).
- Wins are **real but deferred** for product: migration + `RefBackend` wiring
  not done; recommendation was **defer past 0.3**.

**Graceful product stance today:** keep packed-refs; document degradation;
revisit reftable if users hit >10k-ref pain or layout migration is forced.

---

## Explicit non-claims

Do **not** claim from this recipe alone:

1. **Reftable is shipped** — `ReftableModel` is a spike in schema/refs benches;
   **not** wired through `RefManager` / production save path.
2. **Wave 7 full green** — stress docs + recipe close a residual item; they do
   not certify multi-host or continuous CI scale gates.
3. **CI continuously gates 10k+ refs** — `PLATFORM_MATRIX` still lists large-ref
   stress as **Unknown** as a continuous gate.
4. **Bench reftable numbers are product SLA** — prototype format; append path in
   older write-ups had measurement caveats (round-2 correction in design doc).
5. **Partial clone / lazy fetch** — unrelated; remains **Planned** (GAP_MAP L3).
6. **Tiny unit tests “prove” scale** — `refs_packed_tests` prove format behavior,
   not 10k latency.

---

## Classification for Wave 7 residual

| Checklist item | Status |
|----------------|--------|
| Document ~10k threshold + reproducible recipe | **Done** (this file + script) |
| Classify large-ref work as stress, not tiny-fixture oracle | **Done** (Criterion 10k/50k/100k; unit tests remain correctness-only) |
| Reftable product backend | **Planned** — leave open in GAP_MAP L2 |

Related: [`PLATFORM_MATRIX.md`](PLATFORM_MATRIX.md),
[`scripts/program/packed-refs-stress-recipe.sh`](../../scripts/program/packed-refs-stress-recipe.sh).
