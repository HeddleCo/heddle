# Long-lived Mount Daemon — Design Note

**Status:** Shipped. **Default: daemon. Opt out with `--no-daemon`.**
`heddle start --workspace virtualized` hands the FUSE mount to
the long-lived `heddled` daemon by default; `--no-daemon` keeps the
mount in-process and tied to the CLI process. See "Operator guide"
below for `heddle daemon serve|status|stop` usage and "History" at the
bottom for the migration timeline.
**Owner:** mount lifecycle (`crates/cli/src/cli/commands/mount_lifecycle.rs`)
plus `crates/cli/src/cli/commands/daemon/` (the daemon itself) and
`crates/repo/src/daemon/` (shared scaffolding).
**Trigger (resolved):** `// TODO(virtualized-thread daemon)` comment in
`mount_lifecycle.rs` is now an "implemented and default" pointer.

## Operator guide

`heddle daemon serve` runs a foreground mount daemon for the current
repository. Normally spawned on demand by `heddle start
--workspace virtualized` (the daemon is the default; pass
`--no-daemon` to keep the mount in this CLI process instead).
Running `daemon serve` interactively is for debugging.

`heddle daemon status` reports liveness, version, uptime, and the
current mount count. Returns "not running" success if no daemon is
live — safe to use as a probe in scripts.

`heddle daemon stop` asks a running daemon to drain its mounts and
exit. After receiving the shutdown ack, the CLI waits up to 2 s for
`heddled.endpoint.json` to disappear and a further 2 s for the
recorded PID to die before returning. Sweeps any leftover registry
entries with `fusermount -u` as a safety net before returning. The
post-condition (when the call returns `Ok` after a live shutdown) is
that the daemon process is gone and both `heddled.endpoint.json` and
`mounts.json` are removed — see `cmd_daemon_stop` in
`crates/cli/src/cli/commands/daemon/cmd.rs` for the precise ordering.

State lives under `.heddle/state/`:

- `heddled.endpoint.json` — host, port, PID, protocol version.
- `mounts.json` — atomic mirror of the live mount registry. Used by
  the stale-endpoint sweep in CLI clients to recover from a daemon
  crash.

Threat model: localhost TCP, no auth — same posture as the existing
fsmonitor helper. Single-user dev workstation.

## Problem

`heddle start --workspace virtualized` projects a thread's tree through
a FUSE mount. The mount is owned by the CLI process: when the user kills
heddle (or the process exits), `BackgroundSession::drop` unmounts the FS.

That's fine for a one-shot inspection. It is not fine for the workflows the
product implies:

- An agent runs in its own thread for hours; the operator wants to `cd` into
  the mount from any shell, not the one that started it.
- `heddle status`, `heddle log`, and IDE integrations want to stat the mount
  path *between* CLI invocations.
- A second `heddle start <thread> --workspace virtualized` against an
  already-mounted thread should attach to the existing kernel mount, not race
  it.

`crates/repo/src/fsmonitor.rs` already solves a near-identical problem for
filesystem watching: subprocess helper, JSON-over-TCP, idle timeout, endpoint
file under `.heddle/state/`. We can reuse the bones.

## Path comparison

| | Path A — `heddled` (extend fsmonitor) | Path B — separate mount helper | Path C — accept the limitation |
|---|---|---|---|
| Scope | ~600–900 LOC | ~400–600 LOC | 0 LOC |
| Files touched | `crates/repo/src/fsmonitor.rs`, new `crates/repo/src/daemon/mount.rs`, `crates/cli/src/cli/commands/mount_lifecycle.rs`, `monitor` subcommand | new `crates/cli/src/cli/commands/mount_daemon.rs`, `mount_lifecycle.rs` | docs only |
| Process count per repo | 1 | 2 (fsmonitor + mountd) | 0 background |
| Protocol churn | bumps `HELPER_PROTOCOL_VERSION` from 1 → 2; adds `mount`/`unmount`/`list_mounts` commands | new protocol, parallel evolution | none |
| Survives CLI exit | yes | yes | **no** (current behaviour) |
| Risk | refactor in a hot path; fsmonitor regressions visible to every `status` call | duplicated lifecycle code, two PID/endpoint files to reason about | `heddle status` from a sibling shell observes a stale mount path that 404s on read |

### Recommendation: **Path A**, deferred until two of {hosted preview demo,
attached-reasoning workflow, IDE plugin} need it.

Path A is the right shape — fsmonitor already proved the helper/idle/endpoint
pattern works inside a Rust CLI without pulling in `tokio` or a new transport.
Adding a second daemon (Path B) doubles the lifecycle surface for no
architectural gain. Path C is the honest current state and is fine while
virtualized threads are an opt-in v1 demo surface.

We should not ship Path A until a concrete user pulls on it. The mount
limitation is annotated in code (`mount_lifecycle.rs:144`) and the existing
test (`virtualized_thread_round_trip`) keeps the in-process path honest.

## Lifecycle (Path A)

**Spawn-on-demand, not always-on.** Mirror fsmonitor: the first CLI invocation
that needs a mount tries to load
`.heddle/state/heddled.endpoint.json`. Missing or stale → spawn `heddle daemon
serve` (detached, stdio nulled) and retry for up to ~500 ms. The daemon
persists its TCP endpoint atomically the same way `persist_helper_endpoint`
does today.

**Idle timeout, but mount-aware.** Today's 300 s idle exit (`HELPER_IDLE_TIMEOUT_SECS`)
must change for mounts: the daemon may not exit while it owns a live FUSE
session, regardless of RPC inactivity. Replace the unconditional idle check
with `idle_for >= 300 s && live_mounts.is_empty()`.

**Reboot.** The daemon does not auto-start. If the OS reboots, mounts go away
with the kernel; the next CLI call notices the missing endpoint, finds the
thread refs intact, and either re-mounts on demand (if the thread is active)
or returns the same "thread is set, mount is gone" message it returns today.
We don't need a launchd/systemd unit for v1; we need a clean re-attach flow.

**Discovery.** Endpoint file at `.heddle/state/heddled.endpoint.json`
(host/port, plus a PID we can `kill -0` to detect crashed daemons). Same
shape as `HelperEndpointState` today.

## Protocol sketch

JSON-over-TCP, line-delimited, identical framing to `MonitorHelperRequest`/
`Response`. New commands, additive:

```text
mount       { thread_id, mount_path, repo_root }   -> { ok, handle, mount_path, status }
unmount     { thread_id }                           -> { ok, was_mounted }
list_mounts {}                                      -> { mounts: [{ thread_id, mount_path, pid, since_ms }] }
health      {}                                      -> { ok, version, uptime_s, mount_count, fsmonitor: {...} }
```

`query` and `refresh` (the current fsmonitor verbs) remain. Bumping
`HELPER_PROTOCOL_VERSION` from 1 to 2 lets old CLIs fall back to spawning a
new helper rather than mis-parsing.

JSON over TCP is good enough. A Unix domain socket would be cleaner on
posix and avoid persisting a dynamic port, but the existing helper has
shipped on TCP; switching transport here adds platform branching for no
measurable user benefit.

## Failure modes

- **Daemon dies with mounts alive.** The kernel keeps the mountpoint;
  `BackgroundSession`'s drop never ran. `read()` calls into the mount path
  return EIO until something runs `fusermount -u`. The next CLI invocation
  must (a) detect a stale endpoint file via `kill -0 <pid>`, (b) sweep
  `.heddle/state/mounts.json`, (c) issue best-effort `fusermount -u` per
  registered path before respawning. If sweep fails we surface a "wedged
  mount: run `heddle thread drop <name> --delete-thread`" hint, same posture as the
  current stale-mount comment in `MountHandle::unmount`.

- **Race: two CLI invocations call `mount` for the same thread.** Daemon
  guards with `HashMap<thread_id, MountHandle>`; second call returns the
  existing handle. Same thread/mountpoint pair → idempotent success;
  different mountpoints → reject with a conflict (`thread X is already
  mounted at Y`). This is structurally the same as the
  `Repository::open(repo.root())` + `registry()` pattern in
  `mount_lifecycle.rs:106`, just hoisted out of the CLI process.

- **Daemon vs CLI version skew.** Endpoint protocol version mismatch ⇒ CLI
  removes the endpoint file and respawns. Same fallback fsmonitor uses
  today.

## Migration

Co-existence is straightforward because the daemon path falls back
silently to in-process when the daemon can't be reached.

- **Default**: `--workspace virtualized` hands the FUSE mount to the
  long-lived `heddled` daemon. The mount survives the CLI exit and
  can be shared across subsequent invocations.
- **Opt-out**: `--no-daemon` keeps the mount in this CLI process
  (the legacy behaviour). The mount unmounts when the CLI exits.
- **Affirmation**: `--daemon` is still accepted as an explicit
  affirmation of the default — useful for scripts that want to be
  forwards-compatible with a future change to the default.
- **Fallback**: when `--no-daemon` is *not* passed and the daemon is
  unavailable on this host (no `fusermount`, exec failed, endpoint
  never appeared, daemon reports `mount_unsupported`) the CLI falls
  back to the in-process path with a one-line warning. Real
  daemon-side errors (`mount_conflict`, malformed responses, version
  mismatch in flight) are surfaced — we do not paper over them.

## History

- **2025-Q4**: daemon shipped, opt-in via `--daemon`. Default
  remained in-process so the existing virtualized-mount tests and
  workflows kept their behaviour unchanged.
- **2026-05-02**: default flipped. `--workspace virtualized` now
  defaults to the daemon path; `--no-daemon` is the escape hatch.
  Canary-surface gating from the original migration plan was
  collapsed once the failure-mode coverage in
  `crates/cli/src/cli/commands/daemon/client.rs` and the dispatch
  fallback in `crates/cli/src/cli/commands/mount_lifecycle.rs`
  proved the path was safe to default.

## Concrete next step

**Implemented and default.** `heddle start --workspace
virtualized` hands the FUSE mount off to `heddled` by default;
`--no-daemon` keeps it in-process. See "History" above for the
default-flip date.

When the original spike was estimated, the breakdown was:

1. Day 1 — extract `LocalMonitorServer` into `crates/repo/src/daemon/` and
   land a no-op refactor that keeps the internal monitor serve helper working.
2. Day 2 — bump protocol to v2; add `mount`/`unmount`/`list_mounts`/`health`
   verbs; daemon-side `MountRegistry` reuses the existing
   `ContentAddressedMount::new` + `FuseShell` pair.
3. Day 3 — CLI side: `--daemon` flag wired through `ThreadStartArgs`,
   spawn-on-demand mirrors `try_local_helper_query`.
4. Day 4 — failure-mode coverage: stale-endpoint sweep, dual-mount race,
   daemon-killed-mid-mount recovery test.
5. Day 5 — docs, default-flip flag, release note.

The default flip itself was a separate, smaller change (tri-state
flag + fallback dispatch + this doc update) landed on 2026-05-02.
