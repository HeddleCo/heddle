# FUSE worker IPC — decision doc (heddle#88)

**Status:** Superseded in part by ADR 0048. The crash-isolation analysis remains
useful, but the proposed `heddle.v1` schema and `heddle-grpc` ownership are not
valid implementation guidance. Worker-private IPC must remain internal and
must not recreate a shared protobuf schema outside `HeddleCo/api`.
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

### Historical Decision A — IPC: gRPC over Unix socket

Each fuse-worker exposes a gRPC service on its own Unix socket. Because the
daemon supports multiple concurrent mounts per repo (keyed by thread; see
`MountRegistry` in `crates/cli/src/cli/commands/daemon/registry.rs:114-220`),
socket paths are per-mount, not singleton:

```text
<heddle_dir>/sockets/
  ├── grpc.sock                          # daemon (singleton)
  ├── grpc.pid                           # daemon pidfile
  ├── fuse-worker-<thread>.sock          # one per active mount
  └── fuse-worker-<thread>.pid           # one per active mount
```

`<thread>` is the mount's `thread_id` from the registry, with any
filesystem-unsafe characters sanitized (impl PR picks the exact scheme —
percent-encoding or a slug+uuid suffix are both acceptable; the daemon's
mount registry holds the authoritative `thread → socket path` mapping).
The daemon's own singleton socket stays at
`<heddle_dir>/sockets/grpc.sock` (`crates/daemon/src/local_daemon.rs:53-55`).

Reuse the same posture across all sockets: mode 0600 + SO_PEERCRED same-UID
check (`check_peer_uid_matches_self`,
`crates/daemon/src/local_daemon.rs:369-382`).

**Rationale:** type-safety from tonic codegen, reuse of the daemon's
established UDS / pidfile / SO_PEERCRED pattern, simpler error handling
than hand-rolled length-prefixed framing. The 100–300µs codegen-overhead
latency cost (vs ~50µs raw framing) is accepted.

### Decision B — Architecture: stateful worker, SEPARATE from heddle-daemon

`heddle-fuse-worker` is its own long-lived process. It owns:
- The `Pending` overlay (`crates/mount/src/core.rs:357-372`).
- The gRPC server on its private UDS.

It **holds** the FUSE device fd (the `/dev/fuse` handle backing the
`fuser::Session`) — received via SCM_RIGHTS from the daemon on spawn —
but does NOT own its lifetime. The **daemon is the lifetime owner**
of `/dev/fuse`: it opens the fd when the mount is requested and retains
its copy across worker respawns. On worker crash, the daemon still holds
the fd and hands a fresh dup to the next worker via SCM_RIGHTS. See §4
spawn sequence and §5 SCM_RIGHTS handshake.

`heddle-fuse-worker` is NOT the same process as `heddle-daemon`. The
three-process model per repo:

- `heddle` CLI — **transient**. Each invocation exits when its command
  finishes. Sends mount/unmount/ship/agent requests to the daemon; sends
  capture/status/invalidate directly to the per-mount fuse-worker socket
  it discovers via the daemon's mount registry.
- `heddle-daemon` — **long-lived; the worker supervisor**. Owns the mount
  registry (`MountRegistry` per `crates/cli/src/cli/commands/daemon/registry.rs`),
  spawns one fuse-worker per mount, holds each mount's `/dev/fuse` fd,
  applies the per-worker retry budget, hosts non-mount gRPC services
  (StateReview, Discussion, Signal, OperationLogQuery, Transaction, Hook),
  and owns the daemon-side `Ship` RPC (follow-up; see §3).
- `heddle-fuse-worker` — long-lived per mount, planned at
  `crates/fuse-worker/`. Mount + Pending overlay + FUSE callback handler.

**Rationale:** strongest isolation. A panic in a FUSE callback can never
corrupt the daemon's agent-loop state because they don't share an address
space. Two clearly-scoped processes, each with a single concern.

### Decision C — Crash recovery: retry-budget then drop

The daemon is each worker's supervisor. It watches the worker's gRPC
socket for EOF (and waits on the child pid) and applies a 3-strikes-in-5-
minutes budget **per worker**, tracked alongside the mount entry in
`MountRegistry`:

| Crash # in 5-min window | Action |
|---|---|
| 1 | log + respawn + SCM_RIGHTS the FUSE fd to the new worker + daemon stops accepting output from the dead worker (outstanding kernel requests time out naturally — see §7 respawn budget) + warning banner on next `heddle` command |
| 2 | same as 1 |
| 3 | log full context + `fusermount3 -u <mountpoint>` + daemon removes the mount from the registry + persistent-crash banner: "please file a bug at github.com/HeddleCo/heddle/issues" |

The third-strike action is **per-mount**: only the offending mount is
dropped, and the daemon keeps running for other mounts and for its
non-mount RPC services.

**Rationale:** tolerates transient bugs (single bad input the new worker
won't see), surfaces persistent bugs cleanly, no false-positive auto-recovery
loops, isolates a poison mount from the rest of the daemon's work.

**Hard requirement: respawn budget.** The daemon MUST respawn the worker
within **500ms** (target; impl PR will benchmark + verify). Slower respawns
risk kernel-side request timeouts on systems with strict `request_timeout`
configuration. The kernel times out in-flight requests naturally during the
respawn gap (the daemon does not track or replay individual requests —
see §5), so the entire crash-recovery design depends on the gap staying
short. Detail + benchmark plan in §7.

## 2. Architecture diagram

```text
        ┌─────────────────────────────────────────────────────────────┐
        │                          USER SHELL                          │
        │   $ heddle start --workspace virtualized                     │
        └────────────────────────────────┬─────────────────────────────┘
                                         │ exec (transient)
                                         ▼
        ┌─────────────────────────────────────────────────────────────┐
        │               heddle CLI  (transient per command)            │
        │  - issues mount/capture/ship requests to the daemon          │
        │  - exits when the command finishes                           │
        └────────────────────────────────┬─────────────────────────────┘
                                         │ gRPC over grpc.sock
                                         ▼
        ┌─────────────────────────────────────────────────────────────┐
        │              heddle-daemon  (long-lived; supervisor)         │
        │  - owns MountRegistry (thread_id → mount entry)              │
        │  - spawns one fuse-worker per mount via fork+exec            │
        │  - opens /dev/fuse per mount; lifetime-owns each fd          │
        │  - hands a duped fd to each (re)spawned worker via SCM_RIGHTS│
        │  - holds RestartBudget per worker (per-mount, keyed by       │
        │    thread_id); applies 3-strikes-in-5-minutes recovery       │
        │  - reaps all workers on `heddle daemon stop`                 │
        │  - hosts non-mount RPCs: StateReview, Discussion, Signal,    │
        │    OperationLogQuery, Transaction, Hook, Ship                │
        └────────┬───────────────────────────────┬──────────────────────┘
                 │ spawn                          │ spawn (one per mount)
                 │  + SCM_RIGHTS(/dev/fuse)       │  + SCM_RIGHTS(/dev/fuse)
                 │  ── fuse-worker-<t1>.sock ──   │  ── fuse-worker-<t2>.sock ──
                 ▼                                ▼
   ┌──────────────────────────────────┐  ┌──────────────────────────────────┐
   │  heddle-fuse-worker  (thread t1) │  │  heddle-fuse-worker  (thread t2) │
   │  (crates/fuse-worker, planned)   │  │  (crates/fuse-worker, planned)   │
   │                                  │  │                                  │
   │  - FuseWorkerService gRPC server │  │  - FuseWorkerService gRPC server │
   │  - holds ContentAddressedMount   │  │  - holds ContentAddressedMount   │
   │  - holds Pending overlay         │  │  - holds Pending overlay         │
   │  - holds /dev/fuse fd            │  │  - holds /dev/fuse fd            │
   │    (received via SCM_RIGHTS      │  │    (received via SCM_RIGHTS      │
   │     from the daemon on spawn)    │  │     from the daemon on spawn)    │
   │  - runs fuser::Session loop      │  │  - runs fuser::Session loop      │
   └─────────────────┬────────────────┘  └─────────────────┬────────────────┘
                     │ writes replies to /dev/fuse         │
                     └──────────────┬──────────────────────┘
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
    CLI  ──gRPC over fuse-worker-<thread>.sock──▶  fuse-worker (FuseWorkerService)
                                                   │
                                                   │  ContentAddressedMount::capture()
                                                   ▼
                                                   response  ──gRPC reply──▶  CLI

    (The CLI discovers the per-mount socket path by asking the daemon's
    mount registry over grpc.sock first; subsequent calls go direct.)
```

The key architectural commitment: the kernel↔worker path does NOT cross a
gRPC hop. The worker is itself the FUSE callback handler — `fuser::Session`
runs inside the worker process. gRPC is the surface for *external commands
into* the worker (Capture, Status, Stop, Invalidate — see §3), not for
individual kernel callbacks. That keeps the per-read syscall cost at
FUSE-native latency.

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
a CAS hash and pushes to the remote). `heddle land` is therefore a
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
- SO_PEERCRED check matching the daemon's UID
  (`check_peer_uid_matches_self`) — workers and daemon run as the same
  user, so a single same-UID rule covers CLI↔daemon, CLI↔worker, and
  daemon↔worker traffic.
- Pidfile with the same `PIDFILE_MARKER` + identity-check pattern
  (`crates/daemon/src/local_daemon.rs:94-252`), repurposed as
  `PIDFILE_MARKER = "heddle-fuse-worker"`, one pidfile per mount at
  `<heddle_dir>/sockets/fuse-worker-<thread>.pid`.

## 4. Process lifecycle (Decision B consequences)

### Spawn sequence

The daemon is each worker's supervisor. On a `heddle start --workspace
virtualized` (or the equivalent mount entry point):

1. The transient CLI ensures the daemon is running (auto-spawning it via
   the existing `crates/daemon/src/local_daemon.rs:serve` path if not),
   then sends a `MountThread { thread_id, mount_path, … }` request over
   `grpc.sock`.
2. The daemon, on receiving the mount request:
   1. Allocates a `MountRegistry` entry for `thread_id` and the
      per-mount socket path `fuse-worker-<thread>.sock`.
   2. Acquires the FUSE device fd by opening `/dev/fuse` itself. The
      daemon is the **lifetime owner** of this fd — it retains its own
      copy across every worker respawn so the kernel never sees the
      mount go away when a worker dies. (Without this, a worker crash
      drops the fd and the kernel tears the mount down before we get a
      chance to retry.)
   3. Forks/execs the `heddle-fuse-worker` binary with the per-mount
      socket path and a bootstrap socketpair fd inherited on a known
      fd (e.g. fd 3).
   4. Hands the FUSE fd to the new worker via a single `sendmsg` with
      `SCM_RIGHTS` over the bootstrap socket. The worker reads the fd
      and passes it to `fuser::Session`.
   5. Waits for the worker to write its pidfile + open its gRPC
      listener at `fuse-worker-<thread>.sock`.
3. The daemon returns the worker's socket path (and `thread_id`) to the
   CLI as the mount response. Subsequent CLI calls for that mount
   connect directly to the worker socket; the daemon's role on those is
   only supervision (crash watch + retry budget), not RPC proxying.

The order matters: the daemon owns the FUSE fd so a worker death doesn't
collapse the mount, and ownership lives in the long-lived process —
never in the transient CLI invocation, which has already exited by the
time the worker is running.

### Routing rules

| CLI command | Talks to |
|---|---|
| `heddle start` / mount registration | daemon (then daemon spawns the worker) |
| `heddle capture` (mount-bound) | fuse-worker (per-mount socket; CLI looks up via daemon's registry) |
| `heddle land` | two-step: (1) `FuseWorkerService.Capture` on fuse-worker → CAS hash; (2) daemon's `Ship` RPC with that hash for the network push |
| Mount status (`heddle status` mount fields) | fuse-worker (per-mount socket); daemon aggregates if multi-mount |
| `heddle agent serve` and any agent-loop RPC | daemon |
| `heddle log` / `heddle review` / state-review RPCs | daemon |
| `heddle daemon stop` | daemon (which then reaps every fuse-worker) |

The daemon's mount registry is the source of truth for resolving
`thread_id → fuse-worker-<thread>.sock`. The CLI either asks the daemon
once at command start, or reads the registry snapshot from
`<heddle_dir>/sockets/` directly.

### Shutdown sequence

`heddle daemon stop` (the explicit user verb; process exit follows the
same path on the daemon side):

1. CLI sends `DaemonService.Stop` over `grpc.sock`.
2. Daemon iterates its `MountRegistry`. For each entry:
   1. Sends `FuseWorkerService.Stop` over the per-mount worker socket.
   2. Worker flushes hot tier (`ContentAddressedMount::flush` on each
      open buffer), then drops `fuser::BackgroundSession` (which
      unmounts).
   3. Worker exits 0; pidfile guard removes the pid + socket files
      (same pattern as `PidGuard::drop`,
      `crates/daemon/src/local_daemon.rs:190-195`).
   4. If the worker is unreachable, the daemon falls through to
      `SIGTERM` + 2s grace + `SIGKILL`, then runs `fusermount3 -u
      <mountpoint>` to drop the kernel-side mount cleanly.
3. Daemon closes every `/dev/fuse` fd it was holding.
4. Daemon removes its own pidfile + socket files and exits 0.

The transient CLI that issued `heddle daemon stop` waits for the daemon
to disappear (pidfile gone) and then exits.

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

Lives in the daemon, one instance per worker (i.e. per `MountRegistry`
entry, keyed by `thread_id`). Shape:

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

The daemon passes the FUSE fd to each worker over a short-lived
bootstrap socket:

1. Daemon creates a `socketpair(AF_UNIX, SOCK_STREAM)`.
2. Daemon spawns the worker with one end of the socketpair inherited
   on a known fd (e.g. fd 3).
3. Daemon writes a single `sendmsg` with `SCM_RIGHTS` carrying the
   `/dev/fuse` fd as ancillary data.
4. Worker reads the message, extracts the fd, and constructs its
   `fuser::Session` against it.
5. Both sides close the bootstrap socket. All subsequent traffic uses
   the main per-mount gRPC socket.

On respawn the same dance runs against the *same* `/dev/fuse` fd —
critically, the daemon never closes it across the worker death, so the
kernel keeps the mount alive across the daemon-observed gap.

### In-flight callbacks (no supervisor bookkeeping)

When the worker dies mid-call, the kernel still expects replies on each
FUSE request that was outstanding. The daemon does NOT track request
IDs and does NOT replay EIO for them — kernel-side request IDs live
inside `fuser` in the worker process and never cross the daemon
boundary. The respawn handoff is at the **FUSE fd level**, not the
**request level**.

The recovery strategy:
- During a respawn the kernel observes a brief gap on the FUSE fd. The
  daemon stops accepting any output from the dead worker, opens the
  new worker, and SCM_RIGHTS-hands it the same `/dev/fuse` fd.
  Outstanding kernel requests **time out naturally** on the kernel side
  if the gap exceeds the kernel's `request_timeout`; otherwise the new
  worker's `fuser::Session` picks them up and replies fresh. Either
  outcome is acceptable — there is no per-request supervisor recovery.
- This makes the respawn-budget contract load-bearing: the daemon
  must complete fork+exec+SCM_RIGHTS+listener-up within the **<500ms
  target** (see §7 respawn performance budget) so the kernel does not
  time out the in-flight set on conservatively-configured systems.
- The original "supervisor tracks in-flight callbacks" path in the
  brief was based on the rejected thin-worker shape (where every
  callback hops through the supervisor). Under Decision B's stateful-
  worker shape, the daemon never sees individual callbacks, so it
  cannot — and does not — track them.

If respawn fails (budget exhausted) for a given worker, the daemon:
1. Closes that mount's `/dev/fuse` fd (kernel sees EIO on every pending
   request for that mount).
2. Runs `fusermount3 -u <mountpoint>` to drop the kernel-side mount cleanly.
3. Removes the entry from `MountRegistry`.

The daemon itself **does not exit** on a single-mount drop — only that
mount goes away. Other mounts and non-mount RPC services keep running.

### User-visible signal

A per-repo `<heddle_dir>/state/last-fuse-worker-crash.json` written by
the daemon on every respawn:

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
> - [ ] Supervisor logic in the daemon (extends
>       `crates/cli/src/cli/commands/daemon/registry.rs` and
>       `.../daemon/server.rs`) implementing Decision C's `RestartBudget`
>       (one per mount, keyed by `thread_id`) + SCM_RIGHTS handoff. The
>       daemon opens `/dev/fuse` per mount, forks/execs the worker, and
>       supervises it. CLI is unchanged beyond issuing mount/capture/ship
>       RPCs to the daemon and (for capture/status) to the per-mount
>       worker socket the daemon advertises.
> - [ ] Per-mount socket paths `fuse-worker-<thread>.sock` / `.pid` under
>       `<heddle_dir>/sockets/`, registered in the daemon's mount registry.
> - [ ] Daemon-side `Ship` RPC follow-up filed (worker has `Capture`
>       only; daemon owns `Ship`).
> - [ ] Pidfile + SO_PEERCRED auth, mirroring
>       `crates/daemon/src/local_daemon.rs:355-382`.
> - [ ] On non-Linux platforms the mount-launch path is unchanged
>       (FSKit / ProjFS shells still in-process).
> - [ ] Integration tests covering:
>     - Happy path: spawn worker, mount, read/write, capture, stop.
>     - Single worker crash + recovery: SIGKILL the worker; verify the
>       daemon respawns it, FUSE fd survives, mount remains usable.
>     - Three-strikes drop: kill the worker 3× inside 5 min; verify the
>       daemon drops the mount (only that mount) and the persistent-crash
>       banner is queued; daemon keeps running for other mounts.
>     - Crash-banner file written + surfaced on next `heddle` invocation.
>     - Multi-mount: two mounts in one repo each get a distinct
>       `fuse-worker-<thread>.sock`; crashing one does not affect the other.
>     - `heddle daemon stop` reaps every fuse-worker (SIGTERM, 2s grace,
>       SIGKILL on timeout) before the daemon exits.
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
during the daemon-observed gap between worker death and the new
worker's first reply. The daemon does not change kernel settings
(no `request_timeout` tuning, no probe at mount time), so the design
takes a tight respawn budget instead.

**Hard target: respawn within 500ms** (worker exit observed → new
worker has SCM_RIGHTS-received the fd and is reading from `/dev/fuse`).
The impl PR's benchmark suite MUST measure actual respawn time and the
**merge gate MUST fail if p99 > 500ms** (equivalently: the worst observed
respawn in the benchmark run must stay under 500ms — the impl PR may
phrase it as "p99 ≤ 500ms" or "max ≤ 500ms", but median is **not** an
acceptable summary). The 500ms cap pins a hard kernel-side deadline: a
single tail-latency spike is enough to break recovery, so the worst
case is the metric that defines the contract, not the typical case.

What could blow the budget, and the mitigations the impl carries:

| Risk | Mitigation |
|---|---|
| Cold `cargo` rebuild | N/A — production release builds; the worker binary is on disk before any crash. |
| Large initial state hydration in the new worker | Lazy load — the worker re-attaches to the existing `/dev/fuse` fd and pulls `Pending` state from disk on demand, not at startup. |
| `fork`/`exec` cost | Typically <50ms on Linux; not a real risk at the 500ms budget. |
| Listener-up race (CLI connects before worker `listen()`) | Bootstrap socket completes the SCM_RIGHTS handshake before the worker advertises its gRPC listener; the daemon only advertises the new worker's per-mount socket once its pidfile + listener are visible. |

This budget is the coupling point with §5's "in-flight requests time out
naturally" recovery model: the kernel can only tolerate the recovery gap
if it is short. The two sections must move together — relaxing the
budget here requires re-opening the §5 recovery story.
