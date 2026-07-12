# L8 — Pack install journal (A+, hardened)

**Status:** **Shipped on program tip** (identifiers-only intent, path containment,
pair identity check, quarantine, flock, TTL).

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

1. Stage pack+idx under `.staging/<id>/` (**outside** exclusive lock when possible).
2. Take `packs/.pack-install.lock`.
3. Write durable **prepared** intent.
4. Publish pack → rewrite intent **pack_published**.
5. Publish index → **delete** intent (no Completed rewrite) + fsync intent dir.
6. Best-effort remove staging.

Idempotency: existing final pair is accepted only if pack file **hashes to
`pack_name`** and idx is non-empty.

---

## Concurrency

- Global reentrant flock on install + recover (simple correctness).
- Non-expired incomplete intents are **skipped** (live install).
- Expired incomplete intents are **aborted**.
- Staging outside lock + lock around publish reduces hold time vs staging under lock.

---

## Recovery table

| State | Action |
|-------|--------|
| Can complete (pack final + staged idx) | Complete regardless of TTL |
| Expired incomplete | Abort (reconstructed paths only) |
| Fresh incomplete | Skip in progress |
| Malformed / unknown version | Quarantine |
| Unpaired pack without intent | Option D prune on reload |

---

## Tests (unit)

Path containment, quarantine, pair hash match, relocate packs tree, concurrent
same-pack installs, flock load-bearing expire-vs-live, TTL abort/complete, etc.
(`cargo test -p heddle-objects --lib pack_install_journal`)

---

## Residual

- Full fault-injection matrix at every FS op (property/fault harness).
- Per-pack locks / hosted multi-tenant throughput measurement.
- Hosted metrics product pipeline.
- Wall-clock TTL skew (clock rollback) hardening.
- Selective rollback of pure presentation `*_plan` string modules (architecture debt).
