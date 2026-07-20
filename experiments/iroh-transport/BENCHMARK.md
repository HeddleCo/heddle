# Iroh loopback benchmark

Measured on arm64 macOS 26.5.1 with an optimized build, Iroh revision
`cc876c75e53171dfcaf2e5fa0b81a417904694ba`, and only Iroh's `tls-ring`
feature enabled. Both endpoints bind explicit `127.0.0.1:0` UDP transports.

These are experiment-scale results from one run, not a stable performance
contract. The streaming path generates pack and index files on the server,
copies them into QUIC without a pack-sized buffer, receives them in 256 KiB
chunks, and installs the client spool using Heddle's filesystem-backed
`install_pack_streaming` implementation.

The table immediately below records the earlier streaming baseline. The
current 1 MiB receive-buffer and ACK-frequency results are in the later
"Rolling flow control" section.

| Metric | 16 MiB pack | 64 MiB pack | 256 MiB pack |
|---|---:|---:|---:|
| Endpoint bind | 17.1 ms | 15.0 ms | 13.6 ms |
| QUIC connect | 10.2 ms | 9.7 ms | 8.3 ms |
| Established `ListRefs` p50 | 70.2 us | 70.0 us | 62.4 us |
| Established `ListRefs` p95 | 166.1 us | 147.0 us | 142.0 us |
| Server pack preparation / ready | 71.1 ms | 467.3 ms | 1.093 s |
| QUIC receive into spool | 157.6 ms | 642.1 ms | 2.441 s |
| Streaming receive throughput | 101.5 MiB/s | 99.7 MiB/s | 104.9 MiB/s |
| Destination sync + install | 168.8 ms | 401.2 ms | 1.358 s |
| End-to-end pull | 397.7 ms | 1.511 s | 4.893 s |
| End-to-end throughput | 40.2 MiB/s | 42.4 MiB/s | 52.3 MiB/s |

The earlier buffered path received about 160–165 MiB/s on loopback. Streaming
to the spool costs roughly 37% of that raw throughput, but remains flat through
256 MiB and eliminates memory growth proportional to total pack size. Memory
is now proportional to the 256 KiB receive buffer plus the largest individual
object being encoded. A single very large blob can still be materialized as
one `ObjectData` value by the existing native-pack writer.

## Latest-main retest

After updating from `48e5c20d` to `445c6b9b`, the experiment was moved from
the removed local `heddle.v1` crate to published `heddle-api` 0.1.2 and its
typed `heddle.api.v1alpha1` repository and state identifiers. Four optimized
16 MiB samples produced:

| Metric | Median | Range |
|---|---:|---:|
| Endpoint bind | 16.3 ms | 15.8–16.4 ms |
| QUIC connect | 9.7 ms | 9.1–10.2 ms |
| Established `ListRefs` p50 | 115.4 us | 111.2–126.1 us |
| Established `ListRefs` p95 | 174.3 us | 161.3–182.7 us |
| Server pack preparation / ready | 60.0 ms | 58.3–61.0 ms |
| Streaming receive throughput | 96.7 MiB/s | 92.9–100.9 MiB/s |
| End-to-end pull | 384.0 ms | 369.0–393.6 ms |
| End-to-end throughput | 41.7 MiB/s | 40.7–43.4 MiB/s |

Bulk transfer and end-to-end throughput remain in the earlier range. The
control-operation sample is slower than the original single-run 70.2 us p50,
but this is not a controlled old-versus-new binary comparison, so it is a
signal to A/B the codecs rather than evidence of a transport regression. The
optimized benchmark binary is 8.16 MiB, effectively unchanged from 8.2 MiB.

## Operation-stream framing and native Iroh writes

The original v1 framing used tagged length-prefixed control messages plus
connection-global unidirectional pack and index streams. The revised v2 framing
maps one operation to one bidirectional stream, uses FIN to delimit unary
messages, and sends raw files as owned 1 MiB `Bytes` chunks through
`SendStream::write_chunk`. The A/B adapter was deleted after measurement.

Three alternating optimized samples per adapter produced these medians:

| Metric | 16 MiB v1 | 16 MiB v2 | 64 MiB v1 | 64 MiB v2 |
|---|---:|---:|---:|---:|
| Established `ListRefs` p50 | 64.3 us | 63.5 us | 108.5 us | 108.3 us |
| Established `ListRefs` p95 | 130.2 us | 127.2 us | 139.5 us | 139.5 us |
| Streaming receive throughput | 109.5 MiB/s | 136.7 MiB/s | 109.5 MiB/s | 146.9 MiB/s |
| End-to-end pull | 355.1 ms | 325.5 ms | 1.178 s | 1.029 s |
| End-to-end throughput | 45.1 MiB/s | 49.2 MiB/s | 54.3 MiB/s | 62.2 MiB/s |

The stream mapping itself modestly improved the 16 MiB control tail and fixed
correlation for concurrent pulls. The owned-`Bytes` send path supplied the
large gain: median receive throughput improved 25% at 16 MiB and 34% at 64 MiB;
end-to-end throughput improved 9% and 15%, respectively. Pack construction and
destination sync/install now dominate the remaining elapsed time.

The v3 request header replaces the experiment-only one-byte operation tag with
the governed fully-qualified method path and advances the ALPN to
`heddle-sync/3`. One optimized 16 MiB smoke sample measured a 57.2 us
`ListRefs` p50 and 175.5 MiB/s receive-to-spool throughput. Neither regressed
against the preceding operation-stream samples; the roughly 50-byte unary header
does not show a measurable cost at this sample size, while giving dispatch and
request signing the same stable method identity.

## Rolling flow control and raw wire ceiling

The transport-only benchmark requests a generated response, drains it with
Iroh's `RecvStream::read_many_chunks`, and discards the bytes. It excludes
protobuf, pack construction, file I/O, and installation. Five 256 MiB samples
with Iroh's default transport configuration produced a 147.5 MiB/s median and
148.3 MiB/s maximum.

Iroh's 1.25 MiB per-stream receive window is already rolling credit, not a
total-transfer limit: consuming bytes advances `MAX_STREAM_DATA`. Increasing
it to 16 MiB without changing ACK behavior did not improve loopback throughput.
The useful setting was QUIC ACK frequency:

| Endpoint profile | 256 MiB median | Maximum |
|---|---:|---:|
| Iroh default | 150.0 MiB/s | 150.2 MiB/s |
| Default window, ACK threshold 10 | 174.4 MiB/s | 174.9 MiB/s |
| Default window, ACK threshold 20 | 184.1 MiB/s | 185.0 MiB/s |
| Default window, ACK threshold 50 | 187.4 MiB/s | 187.9 MiB/s |
| 16 MiB window, no ACK request | 146.6 MiB/s | 146.9 MiB/s |
| 16 MiB window, ACK threshold 50 | 188.7 MiB/s | 192.8 MiB/s |

The retained profile keeps Iroh's default rolling stream window and uses ACK
threshold 50. The 16 MiB profile's small gain did not justify multiplying the
per-active-stream flow-control budget. A sustained 1 GiB, three-sample run on
the retained profile measured 183.9 MiB/s median and 185.7 MiB/s maximum,
about 1.54 and 1.56 Gbit/s of application payload respectively.

Matching the spool receive buffer to the sender's owned 1 MiB chunks removed
the next adapter bottleneck. Three full 256 MiB runs on the retained profile
produced:

| Metric | Median | Range |
|---|---:|---:|
| Raw generated Iroh stream | 178.1 MiB/s | 177.0–187.1 MiB/s |
| QUIC receive into file-backed spool | 176.0 MiB/s | 175.6–179.3 MiB/s |
| End-to-end pull/build/install | 72.1 MiB/s | 69.1–72.2 MiB/s |

Compared with the preceding 256 MiB operation-stream sample, file-to-spool
throughput rose from 99.8 to 176.0 MiB/s (+76%) and end-to-end throughput rose
from 54.3 to 72.1 MiB/s (+33%). The adapter is now effectively wire-speed on
this host. Pack generation and durable checksum-validating install are the
remaining pipeline costs.

The window still has to cover bandwidth-delay product to saturate a remote
path. For example, 1 Gbit/s at 100 ms needs roughly 12.5 MB in flight. This
pinned Iroh revision permits changing the connection-wide receive window at
runtime, but not the per-stream receive window. A bounded high-BDP endpoint
profile therefore remains a future WAN experiment rather than a dynamically
resized stream window.

## Current costs and caveats

- The standalone minimal-feature benchmark binary is 8.2 MiB. Iroh's default
  features produced an 8.9 MiB binary in the same experiment.
- Adding the root-workspace revision pin added 135 lockfile packages. Iroh's
  normal dependency closure contains 274 packages on this target.
- The first optimized build of the default-feature experiment took 2m50s on
  this warm workspace. Relinking after a small transport edit took 56s.
- The server completes its file-backed pack before writing the pull prelude.
  Overlapping `NativePackStreamingWriter` flushes with Iroh sends will require
  replacing the upfront body lengths with a bounded chunk/end marker or a
  separately delimited data stream. `GrowingPackChunkReader` supplies the
  storage side of that future pipeline.
- Current Iroh `main` timed out when the experiment immediately dialed its
  automatically advertised macOS interface addresses. Explicit loopback
  addresses work. Relay, address discovery, NAT traversal, and WAN behavior
  still need their own experiment.
- This benchmark does not compare the same operation against tonic/gRPC and
  does not exercise 0-RTT replay behavior.
