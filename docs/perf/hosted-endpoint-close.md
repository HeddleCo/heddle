# Hosted Endpoint Close Performance

Hosted operations initiate a graceful QUIC/endpoint close at their shared
teardown boundary. Calling `Endpoint::close` is required: skipping it
reproduces `Endpoint dropped without calling Endpoint::close` and an
ungraceful abort (#1143/#1151). Awaiting the full drain is not required
on the one-shot CLI path.

## The WAN tax

On a relay/WAN path (preview AX), `Router::shutdown` → `Endpoint::close`
→ `noq wait_all_draining` waits for a close-frame ACK / probe timeout.
After the RPC is already `LocallyClosed`, that wait is a repeatable
**~1000 ms**. It turned a finished ~1 s `whoami` into a ~2 s CLI.

Loopback release measurements from 2026-08-04 (`fb5d4b7c`) remain true
for isolated local fixtures: median 0.3965 ms, p95 0.694 ms. They do
not describe the relay path.

## Decision

`HostedConnection::close` still *starts* graceful shutdown so the
endpoint is not dropped dirty. The foreground wait is bounded at
**20 ms** (`FOREGROUND_ENDPOINT_DRAIN`). On timeout the `JoinHandle`
is forgotten so drain keeps running (wrapping the handle in
`tokio::time::timeout` would drop it on `Elapsed` and cancel
mid-close). Process exit is no longer gated on `wait_all_draining`.

When `heddle netd serve` is running, hosted verbs reuse the daemon's
persistent endpoint and a cached weft QUIC connection. Close of a
proxied handle does not drain weft. If that process opened a provider
(CAS) connection, it bound a local ephemeral endpoint; close starts a
bounded drain of that endpoint only.

Preview vs prod relays stay the `preview` cargo feature. Hosted
connections still take their relay list from the signed descriptor.

## Release measurement (loopback)

Measured on 2026-08-04 at `fb5d4b7c` using rustc 1.97.0's optimized
release profile on Linux x86_64 (AMD Ryzen 7 7700, 8 cores/16 threads).
Each sample established a loopback connection over the hosted ALPN
through `HostedConnection::connect_verified`; endpoint setup and
compilation were outside the timed window. The timer covered only the
shared hosted-operation teardown.

| teardown | samples | median | p95 | min-max |
| --- | ---: | ---: | ---: | ---: |
| graceful `Endpoint::close().await` | 20 | 0.3965 ms | 0.694 ms | 0.354-0.779 ms |
| control: skip endpoint-close await | 20 | 0.0020 ms | 0.003 ms | 0.002-0.003 ms |

The 20 ms foreground bound has large headroom on loopback. The bound
exists for the WAN drain, not to shave the 0.4 ms local case.

## Executable contract

The dedicated release workflow runs:

```sh
TMPDIR=/home/scratch cargo test --locked --release -p heddle-hosted-client --lib \
  hosted_endpoint_close_release_contract -- --ignored --nocapture
```

The test takes 20 samples, prints median/p95/min/max, asserts p95 is
within 20 ms, and asserts that the endpoint is closed before each
successful hosted connection is dropped. The workflow also enables Iroh
error logs and fails if the literal #1143
`Endpoint dropped without calling Endpoint::close` message appears. The
test refuses to run as a debug-build gate.

Unit tests in `connection.rs` fail closed if close again blocks ≥~1 s
on a local fixture after the QUIC session is already LocallyClosed, if
a 1 s injected drain is not returned from early, and if a drain that
outlives the 20 ms bound is aborted instead of completing after detach.

```sh
TMPDIR=/home/scratch HEDDLE_HOSTED_CLOSE_NEGATIVE_CONTROL=latency \
  cargo test --locked --release -p heddle-hosted-client --lib \
  hosted_endpoint_close_release_contract -- --ignored --nocapture
```
