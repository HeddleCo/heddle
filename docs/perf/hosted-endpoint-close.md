# Hosted Endpoint Close Performance

Hosted operations explicitly close their QUIC connection and then await
`Endpoint::close()` at their shared teardown boundary. The await is required:
skipping it reproduces `Endpoint dropped without calling Endpoint::close` and
an ungraceful abort. This document records the release-mode cost and its
executable budget so the earlier debug-build number is not treated as shipped
latency.

## Release measurement

Measured on 2026-08-04 at `fb5d4b7c` using rustc 1.97.0's optimized release
profile on Linux x86_64 (AMD Ryzen 7 7700, 8 cores/16 threads). Each sample
established a loopback connection over the hosted ALPN through
`HostedConnection::connect_verified`; endpoint setup and compilation were
outside the timed window. The timer covered only the shared hosted-operation
teardown. Twenty control samples closed the QUIC connection but temporarily
skipped the endpoint-close await, with each sample isolated in a fresh test
process so its intentional ungraceful abort could not contaminate the next
connection.

| teardown | samples | median | p95 | min-max |
| --- | ---: | ---: | ---: | ---: |
| graceful `Endpoint::close().await` | 20 | 0.3965 ms | 0.694 ms | 0.354-0.779 ms |
| control: skip endpoint-close await | 20 | 0.0020 ms | 0.003 ms | 0.002-0.003 ms |

The median release drain attributable to the await was approximately 0.3945
ms, not the 0.87 seconds observed in the historical debug build. All 20 control
runs emitted the #1143 ungraceful-abort error. A non-isolated control also made
the following connection time out, further confirming that skipping the await
is not a viable optimization.

## Budget and decision

The release contract is **p95 <= 20 ms** for the isolated endpoint close. Law 7
in #1218 requires an executable latency gate; its smallest fixed-work product
band is the 20 ms p95 budget for cold metadata commands. Hosted completion is
otherwise expressed relative to RTT plus fixed server work, so client endpoint
drain is gated as fixed local work rather than folded into an unstable total
push, pull, or clone duration.

The measured p95 has about 29x headroom under that budget. The graceful close
therefore stays unchanged: adding a timeout or another teardown path would add
complexity to save less than one millisecond while risking the real leak and
spurious error fixed by #1143/#1151.

## Executable contract

The dedicated release workflow runs:

```sh
TMPDIR=/home/scratch cargo test --locked --release -p heddle-cli --lib \
  hosted_endpoint_close_release_contract -- --ignored --nocapture
```

The test takes 20 samples, prints median/p95/min/max, asserts p95 is within 20
ms, and asserts that the endpoint is closed before each successful hosted
connection is dropped. The workflow also enables Iroh error logs and fails if
the literal #1143 `Endpoint dropped without calling Endpoint::close` message
appears. The test refuses to run as a debug-build gate.

The repeatable negative control adds 25 ms inside the measured close window and
must fail with `HOSTED CLOSE GATE RED`:

```sh
TMPDIR=/home/scratch HEDDLE_HOSTED_CLOSE_NEGATIVE_CONTROL=latency \
  cargo test --locked --release -p heddle-cli --lib \
  hosted_endpoint_close_release_contract -- --ignored --nocapture
```
