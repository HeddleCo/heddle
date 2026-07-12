# L8 — Pack install journal (A+, hardened)

**Status:** **Shipped on program tip** (identifiers-only intent, path containment,
pair identity check, quarantine, per-pack locks, TTL clock-skew clamp, process
metrics hooks, fault-inject checkpoints).

**Module:** `crates/objects/src/store/fs/pack_install_journal.rs`

---

## Intent schema (v2)

```json
{
  "version": 2,
  "install_id": "hex-time-rand",
  "pack_name": "blake3-hex",
  "phase": "prepared | pack_published",
  "created_unix": 0
}
```

**No paths in JSON.** Recovery rebuilds:

| Path | Construction |
|------|----------------|
| staging pack | `{packs}/.staging/{install_id}/pack` |
| staging idx | `{packs}/.staging/{install_id}/idx` |
| final pack | `{packs}/{pack_name}.pack` |
| final idx | `{packs}/{pack_name}.idx` |

`install_id` and `pack_name` are validated (no `..`, no separators; pack_name hex).

Malformed / unknown-version intents are moved to
`{packs}/.install-intent/quarantine/` (never deleted in place).

---

## Protocol

1. Stage pack+idx under `.staging/<id>/` (**outside** the per-pack lock).
2. Take per-`pack_name` exclusive lock (`.pack-locks/<pack_name>.lock`).
3. Write durable **prepared** intent.
4. Publish pack → rewrite intent **pack_published**.
5. Publish index → **delete** intent (no Completed rewrite) + fsync intent dir.
6. Best-effort remove staging.

Idempotency: existing final pair is accepted only if pack file **hashes to
`pack_name`** and idx is non-empty.

Fault-inject checkpoints (env `HEDDLE_FAULT_INJECT` or test
`with_fault_points`): `pack_install_after_stage_*`, `pack_install_after_pack_lock`,
`pack_install_after_intent_prepared`, `pack_install_after_publish_pack`,
`pack_install_after_intent_pack_published`, `pack_install_after_publish_idx`,
`pack_install_after_intent_removed` (+ stream variants).

---

## Concurrency

- **Per-pack** exclusive flock for install publish + recover mutate.
- Short **global** listing lock only while scanning intent filenames.
- Recover uses **try_lock** per pack: if install holds it → `skipped_in_progress`.
- Non-expired incomplete intents are **skipped** (live install).
- Expired incomplete intents are **aborted** (when lock acquired).
- Distinct pack names install in parallel; same pack name serializes.

---

## TTL / clock skew

- Default TTL: 24h (`DEFAULT_PACK_INSTALL_INTENT_TTL_SECS`).
- Mild future `created_unix` (≤ `INTENT_CLOCK_SKEW_TOLERANCE_SECS`, 300s) is
  clamped to `now` so small clock skew does not look like forgery.
- Far-future `created_unix` (beyond tolerance) **expires immediately** so a
  forged timestamp cannot dodge expiry forever.

---

## Metrics hooks

Process-local atomics via `pack_install_metrics_snapshot()` /
`pack_install_metrics_reset()`:

| Counter | Meaning |
|---------|---------|
| `installs_ok` / `installs_err` | journaled install outcomes |
| `recover_completed` / `recover_aborted` | recover mutations |
| `recover_skipped_in_progress` | live install / fresh intent |
| `recover_quarantined` | bad intent files |

Surfaced on `RepositoryMaintenanceRunReport.pack_install_metrics` and tracing
on recover. Not a full hosted product pipeline — stable scrape hooks only.

---

## Recovery table

| State | Action |
|-------|--------|
| Can complete (pack final + staged idx) | Complete regardless of TTL |
| Expired incomplete | Abort (reconstructed paths only) |
| Fresh incomplete | Skip in progress |
| Pack lock held | Skip in progress |
| Malformed / unknown version | Quarantine |
| Unpaired pack without intent | Option D prune on reload |

---

## Tests (unit)

Path containment, quarantine, pair hash match, relocate packs tree, concurrent
same-pack + many-distinct-pack installs, pack-lock load-bearing expire-vs-live,
TTL abort/complete, clock skew, fault-inject recover, metrics counters.
(`cargo test -p heddle-objects --lib pack_install_journal`)

---

## Residual

- Full hosted product metrics pipeline (dashboards / multi-tenant SLOs).
- Property/fuzz harness over the full fault matrix under process kill (not just
  in-process `maybe_fail_at`).
- Further selective collapse of pure presentation `*_plan` string modules
  (partial: `oss_plan`, `index_plan`, `switch_plan`, `collapse_plan`,
  `completion_plan`).
