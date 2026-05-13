# CLI Dependency Audit — 2026-05-12

## Why this exists

A dogfooding-driven dep audit on PR #75 found that the `heddle` CLI pulls **485 transitive packages** for a binary whose default behavior is local-only VCS. That number is excessive for a tool whose competitor (`git`) compiles to a static binary with a much smaller transitive footprint, and is the single largest reason `cargo build -p cli` from a cold cache is slow.

This document captures the audit results and the planned remediation so a future session can pick up cold.

## Today's numbers

- **668** total unique packages in the workspace (`cargo metadata`)
- **485** transitive deps for the `cli` binary alone (default features)
- **16** workspace crates, **56** direct external deps on `cli`
- The 5 heaviest *direct* dep subtrees on `cli`:

| Crate | Transitive footprint |
|---|---|
| `gix` | 277 |
| `gix-protocol` | 252 |
| `gix-transport` | 203 |
| `tonic-health` | 139 |
| `tonic` | 132 |

…with `biscuit-auth` (112), `tokio-tungstenite` (90), `hyper-util` (62), and `ed25519-dalek` (60) close behind. See "How to reproduce" at the end.

## The root cause: `cli → server` is a kitchen-sink dependency

[`crates/cli/Cargo.toml:57`](crates/cli/Cargo.toml:57):

```toml
server = { path = "../server", default-features = false }
```

The comment right above it owns this: *"the whole crate is dragged in including its postgres-gated modules — those never run in the CLI binary but cargo links them."*

This single line is responsible for `cli → server → axum → hyper-util` (62 deps), `cli → server → biscuit-auth` (112 deps), `tonic-health` (139), `tower-http`, and `webauthn` machinery. **~120–150 of the 485 transitive deps come through this one path** and none of them execute on the local-only `heddle` codepath.

## What CLI actually needs from `server`

Only two import paths (from `grep -rn 'use server::' crates/cli/src/`):

- `server::server::grpc_local_impl::*` — local gRPC service trait impls (`review`, `query`, `transaction`, `discuss` verbs)
- `server::local_daemon::*` — the UDS listener + pidfile for `heddle agent serve`

Imports inside those modules ([crates/server/src/server/grpc_local_impl/](crates/server/src/server/grpc_local_impl/) + [crates/server/src/local_daemon.rs](crates/server/src/local_daemon.rs)) reference:

```
tokio, tonic, prost, prost-types, chrono, async-stream, futures, tokio-stream
+ workspace: repo, refs, oplog, objects, crypto, grpc, proto
```

**Zero touches** of axum, biscuit-auth, sqlx, tower-http, webauthn, tokens, pg_*, hosted_access — the heavy stuff that's bloating the CLI.

## The right factoring (the priority fix)

A new shared kernel crate, depended on by both `cli` and `server`:

```
crates/
  daemon/                  ← NEW (or call it `agent-kernel`)
    Cargo.toml             ← lean deps: tokio + tonic + prost + chrono + workspace inner crates
    src/
      lib.rs               ← from crates/server/src/local_daemon.rs
      grpc/                ← from crates/server/src/server/grpc_local_impl/
        mod.rs             ← GrpcLocalService + with_idempotency + to_status
        discussion.rs
        hook.rs
        hook_events.rs
        operation_log_query.rs
        signal.rs
        state_review.rs
        transaction.rs
```

Then rewire:

- **`cli` → `daemon`** (drop the `server = { path = ... }` line from default builds)
- **`server` → `daemon`** (the hosted gRPC impls wrap `daemon`'s local impls)
- **`server` keeps**: `grpc_hosted_impl/`, `biscuit/`, `webauthn.rs`, `tokens.rs`, `pg_*`, `hosted_access.rs`, `bootstrap.rs`, `service/` (if hosted-only), axum HTTP layer, sqlx-gated modules.

The CLI's `hosted-client` feature is the only thing that should still touch the `server` crate (and even that could be split further — see "Follow-ups" below).

### Why this is better than just feature-gating `server`

1. **It models the actual architecture.** Local daemon and hosted server *do* share a kernel — they should share a crate. Today's "kitchen-sink crate behind one optional feature" is an architectural lie.
2. **No leaky default-feature unification.** Today even `default-features = false` pulls server in because of feature unification across workspace members. With the carve-out, the local CLI doesn't link the hosted modules at all.
3. **Smaller blast radius for hosted churn.** Today every change to `crates/server/src/biscuit/` or `webauthn.rs` triggers a CLI recompile.
4. **The `grpc_local_impl` module is already self-contained.** The split is mostly file-moves plus updating `mod` paths and a couple of `pub(super)` → `pub` adjustments. No semantic refactor.

### Estimated impact

| Subtree dropped from `cli` | Transitive deps removed |
|---|---|
| axum + tower-http + hyper-util-server-paths | ~50 |
| biscuit-auth | ~40 |
| tonic-health server side + tonic-reflection | ~20 |
| sqlx (if any workspace member enables `postgres`) | ~30 |
| webauthn + supporting crypto | ~15 |

**~150 transitive deps cut**, taking the CLI from 485 → ~335. Binary size drops 30–50MB on release builds. Cold-cache `cargo build -p cli` should drop 30–60s.

### Risks to plan around before starting

1. **`grpc_local_impl/mod.rs:29-50`** defines `GrpcLocalService { repo, dedup, hook_events }` with `pub(super)` fields. If `grpc_hosted_impl/` reaches into those fields, they need to become `pub` (or accessor methods) when the type moves out of `server`. Audit `grep -rn 'GrpcLocalService' crates/server/src/server/grpc_hosted_impl/` before the move.
2. **`server_core.rs`** likely contains the entry point that mounts both local *and* hosted services on one tonic Server. Construction of `daemon::GrpcLocalService` directly should keep working from `server`; the `local_only` arm of any conditional is the canary.
3. **`hook_events::HookEventBroadcaster`** is constructed in the local kernel but observed by capture/merge code paths that may live elsewhere. Verify nobody outside `server` and `cli` reaches into it through `pub` re-exports.
4. **`re-export aliasing`**: existing call sites use `server::server::grpc_local_impl::LocalDiscussionService` etc. After the move, those become `daemon::grpc::LocalDiscussionService`. The `cli/src/cli/commands/{discuss,query,review,transaction}.rs` import lines need updating; the LSP rename + `cargo check` loop catches this cheaply.

### Effort

3–4 hours, mostly mechanical:
- `git mv` the files
- New `crates/daemon/Cargo.toml` (~15 deps)
- Update `Cargo.toml` workspace members list
- Two crates change their `dependencies` block (`cli` drops `server` from default deps; `server` adds `daemon`)
- Rewrite imports in `cli/src/cli/commands/{discuss,query,review,transaction,agent}.rs` and any internal `server` files that referenced the moved modules

## Other tiers (lower-leverage, can land independently)

### Tier 2 — Drop `gix-protocol` + `gix-transport`, shell out to `git` for net I/O

The big weight in the gix family (`gix-protocol`: 252 deps, `gix-transport`: 203) is HTTPS fetch/push. `heddle bridge git push/pull` already shells out to the `git` binary for some operations ([crates/cli/src/cli/commands/integration.rs](crates/cli/src/cli/commands/integration.rs)). Every dev has `git` installed.

If we commit to shelling out fully, we keep only `gix-object` + `gix-hash` (for parsing the commit graph during import and reading packed-refs) and drop the network stack entirely.

**Drop**: ~80–100 deps. **Effort**: 1 day plus dogfooding the `bridge git import` path against URL sources.

### Tier 3 — Hand-write the 22 JSON schemas, drop `schemars`

`schemars` (50 transitive deps) is used to derive JSON Schema for the 22 registered output schemas in [`crates/cli/src/cli/commands/schemas.rs`](crates/cli/src/cli/commands/schemas.rs). The schemas are stable contracts checked by `heddle doctor schemas` — the derive macro is fighting us as often as helping us.

Replace with hand-written JSON Schema files checked into the repo + a tiny match-on-name lookup. `heddle doctor schemas` already does drift detection at runtime — that catches regressions.

**Drop**: ~50 deps. **Effort**: half day (the schemas are small; the derive macro just makes them less readable).

### Tier 4 — Replace `chrono` with std + tiny RFC3339 helper

`chrono` (40 transitive deps) is used for ISO 8601 timestamps and RFC3339 parsing. We don't use date arithmetic or timezone math — just stamp-and-parse.

Replace with `std::time::SystemTime` + a ~30 LOC RFC3339 formatter/parser. Alternatives: `jiff` (smaller but still 15 deps), or hand-rolled.

**Drop**: ~40 deps. **Effort**: half day.

### Tier 5 — Feature-gate `notify`, `clap_complete`, `rmp-serde`, `serde_cbor`

- `notify` (32 deps): file watching. Only daemon path uses it. Mark `optional = true` behind a (future) `daemon` feature.
- `clap_complete` (22 deps): shell completion generation. Either a separate binary or feature-gated. Users run it once, ever.
- `rmp-serde` + `serde_cbor`: internal-only formats; nothing external reads them. Either pick one or drop both in favor of `serde_json` + a length-prefix framer for the wire path.

**Drop**: ~30 deps. **Effort**: 2h.

### Tier 6 — Hand-roll `tracing-subscriber` (or use lighter logger)

`tracing-subscriber` (39 deps) for a CLI that defaults to WARN+ stderr is overkill. A 60-line `FmtSubscriber`-equivalent or `env_logger` would suffice. Daemons can keep the full subscriber behind a feature.

**Drop**: ~30 deps. **Effort**: 2h.

## Pick-one duplications to clean up alongside

| Domain | Current | What we actually use |
|---|---|---|
| Crypto | ed25519-dalek (60) + p256 + rsa | Ed25519 only for internal signing; p256/rsa exist for biscuit interop and could move to `server` after the carve-out |
| Serialization | serde_json + rmp-serde + serde_cbor + toml | Keep serde_json (output contract) + toml (config). Drop rmp/cbor — internal-only formats |
| Datetime | chrono | std + tiny RFC3339 helper (see Tier 4) |
| JSON Schema | schemars at runtime | Hand-written schemas (see Tier 3) |
| Logging | tracing-subscriber | Light fmt subscriber (see Tier 6) |

## Cumulative target

If all tiers land:

| | Before | After |
|---|---|---|
| Workspace packages | 668 | ~320 |
| `cli` transitive | 485 | ~135 |
| Cold-cache `cargo build -p cli` | (current) | -60s estimated |
| Release binary size | (current) | -40–60MB estimated |

## How to reproduce the numbers

```bash
# Total workspace packages
cargo metadata --format-version 1 | python3 -c "
import json, sys; m = json.load(sys.stdin)
print(f'total unique packages: {len(m[\"packages\"])}')"

# CLI's transitive count (default features)
cargo metadata --format-version 1 > /tmp/heddle_meta.json
python3 << 'EOF'
import json
m = json.load(open('/tmp/heddle_meta.json'))
ws = {p['name'] for p in m['packages'] if p['source'] is None}
node_by_id = {n['id']: n for n in m['resolve']['nodes']}
pkg_by_id = {p['id']: p for p in m['packages']}
cli_id = next(p['id'] for p in m['packages'] if p['name'] == 'cli' and p['source'] is None)
seen, stack = set(), [cli_id]
while stack:
    nid = stack.pop()
    if nid in seen: continue
    seen.add(nid)
    for d in node_by_id[nid]['deps']:
        if not any(k.get('kind') is None for k in d.get('dep_kinds', [])): continue
        stack.append(d['pkg'])
ext = {pkg_by_id[i]['name'] for i in seen if pkg_by_id[i]['name'] not in ws}
print(f'cli transitive externals: {len(ext)}')
EOF

# Why a specific dep is in cli
cargo tree -p cli --edges normal --invert <dep>
```

## Suggested order of operations

1. **Tier 1 (daemon carve-out)** — biggest impact, cleanest architecture story. Land first; everything else is independent.
2. **Tier 5 (feature-gating)** — cheap, no behavior change, often unblocks downstream feature unification once Tier 1 stops dragging `server` in.
3. **Tier 3 (hand-write schemas)** — half day, eliminates a layer of derive-macro fighting.
4. **Tier 4 (drop chrono)** — half day, ergonomic win for binary size.
5. **Tier 2 (gix net I/O via subprocess)** — needs dogfooding the URL-source import path; do after Tier 1 unblocks easier compilation.
6. **Tier 6 (light logger)** — last; bikeshed-prone, low leverage.

## What this does NOT touch

- `crates/repo/`, `crates/objects/`, `crates/refs/`, `crates/oplog/` boundaries — those are correctly sized.
- The 22 stdout JSON schemas (their *shapes*; Tier 3 only changes how they're generated).
- Any verb's behavior or flag surface.
- `docs/PRINCIPLES.md` doctrine.
- The `heddle` binary name or invocation surface.

## Open questions for the next session

1. Should the new crate be named `daemon`, `agent-kernel`, `local-agent`, or `agent-runtime`? Naming affects greppability for future contributors.
2. Should the `hosted-client` cargo feature on `cli` also be split — separating the *client* libraries (tonic-web, hyper-util client) from the *server* libraries that currently sneak in via `server`? This is post-Tier-1 question.
3. Is there a path to making `proto` and `grpc` the same crate? Today they're split for proto-gen ordering reasons; that may have been resolvable upstream.
