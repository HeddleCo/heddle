# FUSE worker IPC — decision doc (heddle#88)

**Status:** Spike. Decisions locked; implementation tracked separately (see §6).
**Scope:** Linux only. macOS uses FSKit; Windows uses ProjFS — both kernel-managed
process models out of heddle's control (see §4 "Cross-platform parity").
**Inputs grounded against:** `crates/daemon/src/local_daemon.rs`,
`crates/mount/src/fuse.rs`, `crates/mount/src/core.rs`, `crates/mount/src/shell.rs`.

## 1. Premise + locked decisions

heddle#88 asks: how do we give Linux FUSE the same crash-isolation property
FSKit's `.appex` extension process gives us on macOS? Today the FUSE callback
handler runs in the heddle CLI / `heddled` daemon's address space (see the
`Filesystem for FuseShell` block in `crates/mount/src/fuse.rs:271-494`). The
`guard_call` panic guard added in heddle#74 (`crates/mount/src/fuse.rs:246-259`)
catches panics at the C ABI boundary, so the process doesn't abort, but the bug
that fired DID corrupt the caught-from process's heap before we caught it.

This doc captures the three locked design decisions for the
`heddle-fuse-worker` sub-process that gives Linux that isolation.

### Decision A — IPC: gRPC over Unix socket

The fuse-worker exposes a gRPC service on its own Unix socket
(`<heddle_dir>/sockets/fuse-worker.sock`), separate from the daemon's
`<heddle_dir>/sockets/grpc.sock` (`crates/daemon/src/local_daemon.rs:53-55`).
Reuse the same posture: mode 0600 + SO_PEERCRED same-UID check
(`check_peer_uid_matches_self`, `crates/daemon/src/local_daemon.rs:369-382`).

**Rationale:** type-safety from tonic codegen, reuse of the daemon's
established UDS / pidfile / SO_PEERCRED pattern, simpler error handling
than hand-rolled length-prefixed framing. The 100–300µs codegen-overhead
latency cost (vs ~50µs raw framing) is accepted.

### Decision B — Architecture: stateful worker, SEPARATE from heddle-daemon

`heddle-fuse-worker` is its own long-lived process. It owns:
- The `Pending` overlay (`crates/mount/src/core.rs:357-372`).
- The gRPC server on its private UDS.

It **holds** the FUSE device fd (the `/dev/fuse` handle backing the
`fuser::Session`) — received via SCM_RIGHTS from the supervisor on spawn —
but does NOT own its lifetime. The **supervisor (CLI) is the lifetime owner**
of `/dev/fuse`: it opens the fd at mount time and retains its copy across
worker respawns. On worker crash, the supervisor still holds the fd and
hands a fresh dup to the next worker via SCM_RIGHTS. See §4 spawn sequence
and §5 SCM_RIGHTS handshake.

It is NOT the same process as `heddle-daemon`. Two long-lived processes per repo:
- `heddle-daemon` (existing — `crates/daemon/`) — agent-loop gRPC services.
- `heddle-fuse-worker` (new — `crates/fuse-worker/`, planned) — mount + Pending
  overlay + FUSE callback handler.

**Rationale:** strongest isolation. A panic in a FUSE callback can never
corrupt the daemon's agent-loop state because they don't share an address
space. Two clearly-scoped processes, each with a single concern.

### Decision C — Crash recovery: retry-budget then drop

The CLI process that spawned the worker is its supervisor. It watches the
worker's gRPC socket for EOF and applies a 3-strikes-in-5-minutes budget:

| Crash # in 5-min window | Action |
|---|---|
| 1 | log + respawn + SCM_RIGHTS the FUSE fd to the new worker + supervisor stops accepting output from the dead worker (outstanding kernel requests time out naturally — see §7 respawn budget) + warning banner on next `heddle` command |
| 2 | same as 1 |
| 3 | log full context + `fusermount3 -u <mountpoint>` + supervisor exits + persistent-crash banner: "please file a bug at github.com/HeddleCo/heddle/issues" |

**Rationale:** tolerates transient bugs (single bad input the new worker
won't see), surfaces persistent bugs cleanly, no false-positive auto-recovery
loops.

**Hard requirement: respawn budget.** The supervisor MUST respawn the worker
within **500ms** (target; impl PR will benchmark + verify). Slower respawns
risk kernel-side request timeouts on systems with strict `request_timeout`
configuration. The kernel times out in-flight requests naturally during the
respawn gap (the supervisor does not track or replay individual requests —
see §5), so the entire crash-recovery design depends on the gap staying
short. Detail + benchmark plan in §7.

## 2. Architecture diagram

```text
        ┌─────────────────────────────────────────────────────────────┐
        │                          USER SHELL                          │
        │   $ heddle start --workspace virtualized                     │
        └────────────────────────────────┬─────────────────────────────┘
                                         │ fork+exec
                                         ▼
        ┌─────────────────────────────────────────────────────────────┐
        │                CLI SUPERVISOR  (heddle binary)               │
        │  - owns lifetime of both subprocesses                        │
        │  - holds RestartBudget for the worker                        │
        │  - opens /dev/fuse, owns its lifetime across worker respawns │
        │  - hands a duped fd to each worker via SCM_RIGHTS            │
        └────────┬────────────────────────────────────────────┬───────┘
                 │ spawn                                       │ spawn
                 │  + UDS connect                              │  + UDS connect
                 │  ── grpc.sock ──                            │  ── fuse-worker.sock ──
                 ▼                                             ▼
   ┌──────────────────────────────────┐       ┌────────────────────────────────────┐
   │       heddle-daemon              │       │       heddle-fuse-worker           │
   │  (crates/daemon, existing)       │       │  (crates/fuse-worker, planned)     │
   │                                  │       │                                    │
   │  - StateReviewService            │       │  - FuseWorkerService gRPC server   │
   │  - DiscussionService             │       │  - holds ContentAddressedMount     │
   │  - SignalService                 │       │  - holds Pending overlay           │
   │  - OperationLogQueryService      │       │  - holds /dev/fuse fd              │
   │  - TransactionService            │       │    (received via SCM_RIGHTS from   │
   │  - HookService                   │       │     the supervisor on spawn)       │
   │                                  │       │  - runs fuser::Session loop        │
   └──────────────────────────────────┘       └─────────────────┬──────────────────┘
                                                                │ writes replies
                                                                │ to /dev/fuse
                                                                ▼
                                                  ┌──────────────────────────────┐
                                                  │       LINUX  KERNEL          │
                                                  │  (FUSE driver: fuse.ko)      │
                                                  └──────────────┬───────────────┘
                                                                 │ syscalls
                                                                 ▼
                                                  ┌──────────────────────────────┐
                                                  │  user processes touching     │
                                                  │  the mount (cargo, grep, IDE)│
                                                  └──────────────────────────────┘

Read-callback data path:
    kernel  ──read(ino, off, len)──▶  /dev/fuse  ──▶  fuse-worker (fuser loop)
                                                    │
                                                    │  ContentAddressedMount::read()
                                                    │  (in-process; no IPC hop)
                                                    ▼
                                                  reply bytes  ──▶  /dev/fuse  ──▶  kernel

CLI-to-worker call path (e.g. `heddle capture`):
    CLI  ──gRPC over fuse-worker.sock──▶  fuse-worker (FuseWorkerService)
                                          │
                                          │  ContentAddressedMount::capture()
                                          ▼
                                          response  ──gRPC reply──▶  CLI
```

The key architectural commitment: the kernel↔worker path does NOT cross a
gRPC hop. The worker is itself the FUSE callback handler — `fuser::Session`
runs inside the worker process. gRPC is the surface for *external commands
into* the worker (capture, ship, status), not for individual kernel
callbacks. That keeps the per-read syscall cost at FUSE-native latency.

## 3. gRPC service surface (Decision A consequences)

### Where it lives

New file: `crates/grpc/proto/heddle/v1/fuse_worker.proto`. Worker-private
surface — added to the `heddle-grpc` crate but only exported under a
`fuse-worker` feature flag because no out-of-tree consumer should call it.

Rationale: keeping it in the same crate gets us the existing tonic build
pipeline (`crates/grpc/build.rs`) and the same proto idioms as
`crates/grpc/proto/heddle/v1/service.proto`. Gating behind a feature flag
keeps `heddle-grpc`'s public API surface small.

### RPCs

The worker hosts a single service, `FuseWorkerService`. The RPC surface
covers two distinct call categories:

**Category A — CLI control plane.** Commands the CLI sends into the worker
to drive the high-level mount lifecycle. These are infrequent (per-user
action) and the gRPC latency is invisible.

| RPC | Purpose | Errors |
|---|---|---|
| `Capture(CaptureRequest) -> CaptureResponse` | Fold pending overlay into a fresh CAS object; returns its hash. Wraps `ContentAddressedMount::capture`. | `INTERNAL` on store I/O. |
| `Status(StatusRequest) -> StatusResponse` | Mount thread name, pending byte counts, open-handle count. | none. |
| `Stop(StopRequest) -> StopResponse` | Graceful shutdown. Worker flushes hot tier, unmounts, exits. | none. |
| `Invalidate(InvalidateRequest) -> InvalidateResponse` | Tell the worker the underlying state moved — drop the relevant inode cache (mirrors `PlatformShell::invalidate` in `crates/mount/src/shell.rs:139`). | `NOT_FOUND` for unknown node. |

**Why `Ship` is not here.** The worker is the state-owner (it captures
pending overlay → CAS object). The daemon is the network-talker (it takes
a CAS hash and pushes to the remote). `heddle ship` is therefore a
two-step CLI flow: (1) CLI → `FuseWorkerService.Capture` returning a CAS
hash; (2) CLI → daemon's ship RPC with that hash. The daemon side does
not currently expose a `Ship` RPC — `grep -r "Ship" crates/daemon/` is
empty as of this spike — so the impl PR (or a follow-up) will need to add
one. Filed as a follow-up sub-issue alongside this spike's impl issue.

**Category B — FUSE-callback shadow surface (planned, deferred to impl).**
The brief asked for "~15-20 RPCs covering the FUSE callback surface". After
auditing the actual Linux `Filesystem` trait usage in
`crates/mount/src/fuse.rs:271-494`, this turns out to be the *wrong* shape:
the worker IS the FUSE handler, so kernel callbacks never need to cross
gRPC. The brief's framing applies only to a "thin worker" variant of
Decision B that we explicitly rejected (the worker would forward each
kernel callback to the supervisor over gRPC; rejected because Pending
must live with the FUSE fd to keep the per-syscall path cheap).

For completeness, the FUSE callbacks the worker handles *in-process*
(matching the trait methods on `crates/mount/src/fuse.rs`) are: `init`,
`lookup`, `getattr`, `open`, `read`, `readdir`, `write`, `flush`,
`release`, `destroy`. Forward-looking callbacks landing under heddle#180
(`setattr`, `unlink`, `rename`, `mkdir`, `rmdir`, `create`, `link`,
`symlink`, `readlink`, `truncate`, `opendir`, `releasedir`, `forget`) join
the same in-process surface — no IPC for any of them.

### Composition with heddle#199 (NodeState planned)

heddle#199 plans a `NodeState` enum (`Live` / `Orphan(open_count)` /
`Released`) and a NodeId-keyed-throughout `Pending` shape. That work
lives entirely inside `ContentAddressedMount` — it does not change the
gRPC surface, because the gRPC surface (Category A above) is path- and
mount-level, not inode-level. The Category B "shadow surface" we
rejected would have had to evolve with `NodeState`; the rejection removes
that coupling.

The one place heddle#199 touches this design: `Invalidate` in Category A
takes a `node_id`. Once heddle#199's NodeId-keyed model lands, the worker's
implementation of `Invalidate` operates against the same NodeId-keyed
`Pending` maps — no protocol change.

### Auth

Same posture as the daemon (`crates/daemon/src/local_daemon.rs:355-382`):
- Socket file mode 0600 (`set_socket_mode_0600`).
- SO_PEERCRED check matching the supervisor's UID
  (`check_peer_uid_matches_self`).
- Pidfile with the same `PIDFILE_MARKER` + identity-check pattern
  (`crates/daemon/src/local_daemon.rs:94-252`), repurposed as
  `PIDFILE_MARKER = "heddle-fuse-worker"`.

## 4. Process lifecycle (Decision B consequences)

### Spawn sequence

The CLI is the supervisor. On a `heddle start --workspace virtualized` (or
the equivalent mount entry point) the CLI:

1. Acquires the FUSE device fd by opening `/dev/fuse` itself. The
   supervisor is the **lifetime owner** of this fd — it retains its own
   copy across every worker respawn so the kernel never sees the mount go
   away when a worker dies. (Without this, a worker crash drops the fd
   and the kernel tears the mount down before we get a chance to retry.)
2. Forks/execs the `heddle-fuse-worker` binary.
3. Hands the FUSE fd to the new worker via an SCM_RIGHTS message on a
   short-lived bootstrap UDS (separate from the worker's main gRPC
   socket). The worker reads the fd and passes it to `fuser::Session`.
4. Waits for the worker to write its pidfile + open its gRPC listener,
   then connects.
5. If `heddle-daemon` isn't already running for this repo, the CLI
   ensures-spawned the daemon via the existing
   `crates/daemon/src/local_daemon.rs:serve` path.

The order matters: the CLI owns the FUSE fd so a worker death doesn't
collapse the mount.

### Routing rules

| CLI command | Talks to |
|---|---|
| `heddle capture` (mount-bound) | fuse-worker |
| `heddle ship` | two-step: (1) `FuseWorkerService.Capture` on fuse-worker → CAS hash; (2) daemon's Ship RPC with that hash for the network push |
| Mount status (`heddle status` mount fields) | fuse-worker |
| `heddle agent serve` and any agent-loop RPC | daemon |
| `heddle log` / `heddle review` / state-review RPCs | daemon |
| `heddle daemon stop` | daemon |

The CLI keeps both endpoints discoverable via `<heddle_dir>/sockets/*.sock`
(daemon: `grpc.sock`, worker: `fuse-worker.sock`).

### Shutdown sequence

`heddle stop` (or process exit):
1. CLI sends `FuseWorkerService.Stop` over the worker gRPC.
2. Worker flushes hot tier (`ContentAddressedMount::flush` on each open
   buffer), then drops `fuser::BackgroundSession` (which unmounts).
3. Worker exits 0; pidfile guard removes the pid + socket files (same
   pattern as `PidGuard::drop`, `crates/daemon/src/local_daemon.rs:190-195`).
4. CLI sends `AgentService.Shutdown` or equivalent to the daemon (existing
   path).
5. CLI releases the `/dev/fuse` fd and exits.

If the worker is unreachable when `Stop` is sent, CLI falls through to
`fusermount3 -u <mountpoint>` + SIGTERM + 2s grace + SIGKILL, mirroring
the daemon-stop pattern (`heddle daemon stop` per `docs/design/mount-daemon.md`).

### Cross-platform parity

This entire design is Linux-specific. The build gates it with
`#[cfg(target_os = "linux")]`:

- **macOS** already runs the FUSE-equivalent callbacks in a separate
  process (the FSKit `.appex`), managed by the kernel. heddle's macOS
  shell (`crates/mount/src/fskit/`) plugs into that model; no change.
- **Windows** uses ProjFS, also kernel-managed. heddle's
  `crates/mount/src/projfs.rs` adapter plugs into that model; no change.

The `heddle-fuse-worker` binary is built only on Linux. The CLI's
mount-launch path on macOS/Windows continues to call the existing in-
process FSKit / ProjFS shells.

## 5. Crash recovery protocol (Decision C consequences)

### `RestartBudget`

Lives in the CLI supervisor. Shape:

```rust
struct RestartBudget {
    /// Crashes observed since `window_start`.
    count: u32,
    /// Wall-clock start of the current 5-minute window.
    window_start: Instant,
}

impl RestartBudget {
    const WINDOW: Duration = Duration::from_secs(5 * 60);
    const MAX_RESTARTS: u32 = 2;  // 3rd crash gives up

    /// Record a crash. Returns Action::Respawn or Action::GiveUp.
    fn observe_crash(&mut self, now: Instant) -> Action {
        if now.duration_since(self.window_start) > Self::WINDOW {
            self.count = 0;
            self.window_start = now;
        }
        self.count += 1;
        if self.count > Self::MAX_RESTARTS {
            Action::GiveUp
        } else {
            Action::Respawn
        }
    }
}
```

The `> MAX_RESTARTS` rather than `>= MAX_RESTARTS` matches the locked
"crash #3 in window → give up" semantics (count=1 → respawn,
count=2 → respawn, count=3 → give up).

### SCM_RIGHTS handshake (spawn + respawn)

The supervisor passes the FUSE fd to each worker over a short-lived
bootstrap socket:

1. Supervisor creates a `socketpair(AF_UNIX, SOCK_STREAM)`.
2. Supervisor spawns the worker with one end of the socketpair inherited
   on a known fd (e.g. fd 3).
3. Supervisor writes a single `sendmsg` with `SCM_RIGHTS` carrying the
   `/dev/fuse` fd as ancillary data.
4. Worker reads the message, extracts the fd, and constructs its
   `fuser::Session` against it.
5. Both sides close the bootstrap socket. All subsequent traffic uses
   the main gRPC socket.

On respawn the same dance runs against the *same* `/dev/fuse` fd —
critically, the supervisor never closes it across the worker death, so
the kernel keeps the mount alive across the supervisor-observed gap.

### In-flight callbacks (no supervisor bookkeeping)

When the worker dies mid-call, the kernel still expects replies on each
FUSE request that was outstanding. The supervisor does NOT track request
IDs and does NOT replay EIO for them — kernel-side request IDs live
inside `fuser` in the worker process and never cross the supervisor
boundary. The respawn handoff is at the **FUSE fd level**, not the
**request level**.

The recovery strategy:
- During a respawn the kernel observes a brief gap on the FUSE fd. The
  supervisor stops accepting any output from the dead worker, opens the
  new worker, and SCM_RIGHTS-hands it the same `/dev/fuse` fd.
  Outstanding kernel requests **time out naturally** on the kernel side
  if the gap exceeds the kernel's `request_timeout`; otherwise the new
  worker's `fuser::Session` picks them up and replies fresh. Either
  outcome is acceptable — there is no per-request supervisor recovery.
- This makes the respawn-budget contract load-bearing: the supervisor
  must complete fork+exec+SCM_RIGHTS+listener-up within the **<500ms
  target** (see §7 respawn performance budget) so the kernel does not
  time out the in-flight set on conservatively-configured systems.
- The original "supervisor tracks in-flight callbacks" path in the
  brief was based on the rejected thin-worker shape (where every
  callback hops through the supervisor). Under Decision B's stateful-
  worker shape, the supervisor never sees individual callbacks, so it
  cannot — and does not — track them.

If respawn fails (budget exhausted), the supervisor:
1. Closes `/dev/fuse` (kernel sees EIO on every pending request).
2. Runs `fusermount3 -u <mountpoint>` to drop the mount cleanly.
3. Exits.

### User-visible signal

A per-repo `<heddle_dir>/state/last-fuse-worker-crash.json` written by
the supervisor on every respawn:

```json
{
  "at_unix_secs": 1716482400,
  "crash_count_in_window": 2,
  "worker_pid": 12345,
  "last_log_tail_path": ".heddle/state/fuse-worker-crash-12345.log"
}
```

The next `heddle` command (any verb) checks for this file at startup and
prints a single-line banner above its usual output if present and recent
(< 1 hour old):

```
⚠ heddle-fuse-worker crashed 2× in the last 5 minutes; tail at
  .heddle/state/fuse-worker-crash-12345.log. Mount is currently live.
```

On the third-strike "give up" path, the banner shape changes to point at
the bug-report URL and notes the mount has been dropped.

## 6. Implementation follow-up sketch

The orchestrator will file this as a new issue, blocked-by-resolved when
this spike's PR merges.

> **Title:** impl: `heddle-fuse-worker` crate — subprocess FUSE callback
> handler with retry-budget supervisor
>
> **Premise:** heddle#88 (this spike) committed to gRPC-over-UDS,
> stateful-worker-separate-from-daemon, retry-budget-then-drop. This
> issue ships the implementation.
>
> **Acceptance criteria:**
> - [ ] New crate `crates/fuse-worker/` with `bin/heddle-fuse-worker`
>       entrypoint. Linux-only build (`#[cfg(target_os = "linux")]`).
> - [ ] gRPC service per Decision A: `FuseWorkerService` with RPCs
>       `Capture`, `Status`, `Stop`, `Invalidate`. Proto at
>       `crates/grpc/proto/heddle/v1/fuse_worker.proto`, feature-gated
>       behind `fuse-worker` in `heddle-grpc`. (No `Ship` — the worker
>       captures to a CAS hash and the daemon handles the network push;
>       follow-up sub-issue adds the daemon-side `Ship` RPC.)
> - [ ] Supervisor logic in `crates/cli/src/cli/commands/mount_lifecycle.rs`
>       (or a new sibling module under `crates/cli/src/cli/commands/`)
>       implementing Decision C's `RestartBudget` + SCM_RIGHTS handoff.
> - [ ] Pidfile + SO_PEERCRED auth, mirroring
>       `crates/daemon/src/local_daemon.rs:355-382`.
> - [ ] On non-Linux platforms the mount-launch path is unchanged
>       (FSKit / ProjFS shells still in-process).
> - [ ] Integration tests covering:
>     - Happy path: spawn worker, mount, read/write, capture, stop.
>     - Single worker crash + recovery: SIGKILL the worker; verify
>       supervisor respawns, FUSE fd survives, mount remains usable.
>     - Three-strikes drop: kill the worker 3× inside 5 min; verify
>       supervisor unmounts and exits with the persistent-crash banner.
>     - Crash-banner file written + surfaced on next `heddle` invocation.
>
> **Effort:** xhigh (cross-process design, lifecycle complexity,
> security-adjacent: SO_PEERCRED + SCM_RIGHTS).
>
> **Blocked by:** heddle#88 (this spike merges first).
>
> **Soft blocks on:** heddle#199 (NodeState model) — the worker's
> in-process `Pending` interactions should compose with the
> NodeId-keyed-throughout shape #199 plans. If #199 is still in flight
> when impl starts, build against current `Pending` and refactor as part
> of #199's impl PRs.

## 7. Benchmarks (deferred to impl)

The original heddle#88 ACs asked for latency benchmarks across three
candidate IPC mechanisms (UDS-framed, shmem-ring, gRPC-UDS). With the
mechanism chosen up-front, benchmarks would only confirm the choice or
surface a *surprise* that overturns it. Per the brief, the impl PR's
test suite will validate the estimated latencies below.

**Estimated latencies** (carried forward from the heddle#88 issue body's
table, to be validated post-impl):

| Mechanism | Estimated per-call | Status |
|---|---|---|
| UDS + length-prefixed frames | < 50 µs | rejected (Decision A) |
| Shared memory + ring buffer | < 10 µs | rejected (Decision A) |
| **gRPC over UDS** | **100–300 µs** | **chosen** |

The chosen mechanism's per-call cost only matters on **Category A**
RPCs (§3) — i.e. CLI control-plane commands like `Capture` and `Status`.
Kernel↔worker FUSE callbacks (Category B) do not cross gRPC at all, so
the budget on per-callback latency is the existing FUSE-native cost,
not the gRPC cost.

Validation plan in the impl PR: add `crates/fuse-worker/bench/control_plane_rtt.rs`
that measures `Capture` and `Status` RTT against a populated mount, and
gate the merge on `< 1 ms` round-trip for `Status` (a tight upper bound
on the 100–300 µs estimate that catches accidental regressions like
"forgot to set TCP_NODELAY equivalent" — though UDS has no such knob,
the gate guards against similar foot-guns).

### Respawn performance budget

Crash recovery (§5) depends on in-flight kernel requests *not* timing out
during the supervisor-observed gap between worker death and the new
worker's first reply. The supervisor does not change kernel settings
(no `request_timeout` tuning, no probe at mount time), so the design
takes a tight respawn budget instead.

**Hard target: respawn within 500ms** (worker exit observed → new
worker has SCM_RIGHTS-received the fd and is reading from `/dev/fuse`).
The impl PR's benchmark suite MUST measure actual respawn time and the
merge gate MUST fail if median > 500ms.

What could blow the budget, and the mitigations the impl carries:

| Risk | Mitigation |
|---|---|
| Cold `cargo` rebuild | N/A — production release builds; the worker binary is on disk before any crash. |
| Large initial state hydration in the new worker | Lazy load — the worker re-attaches to the existing `/dev/fuse` fd and pulls `Pending` state from disk on demand, not at startup. |
| `fork`/`exec` cost | Typically <50ms on Linux; not a real risk at the 500ms budget. |
| Listener-up race (CLI connects before worker `listen()`) | Bootstrap socket completes the SCM_RIGHTS handshake before the worker advertises its gRPC listener; the supervisor only re-routes traffic after the worker pidfile + socket are visible. |

This budget is the coupling point with §5's "in-flight requests time out
naturally" recovery model: the kernel can only tolerate the recovery gap
if it is short. The two sections must move together — relaxing the
budget here requires re-opening the §5 recovery story.
