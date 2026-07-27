# Iroh transport experiment

This spike carries Heddle's public `heddle.api.v1alpha1` `ListRefs` and pull
messages over Iroh QUIC streams, then sends and installs a real Heddle native
pack on the same operation stream. It consumes `heddle-api` as `api`, with no
client or server feature, so the generated protobuf messages are shared with
the hosted API without linking or naming tonic in this transport. It
deliberately uses Iroh's `Minimal` endpoint preset with explicit loopback
addresses so the first result measures the transport seam without interface
discovery, relay, or NAT behavior.

Run it with:

```sh
cargo test --manifest-path experiments/iroh-transport/Cargo.toml -- --nocapture
```

Run the loopback release benchmark with a payload size in MiB and a control
operation count:

```sh
cargo run --release -p heddle-iroh-transport-experiment \
  --example loopback_bench -- 16 200
```

Run the transport-only ceiling benchmark with a payload size in MiB and a
sample count:

```sh
cargo run --release -p heddle-iroh-transport-experiment \
  --example wire_bench -- 1024 3
```

See [BENCHMARK.md](BENCHMARK.md) for measured results and
[FEATURE_ANALYSIS.md](FEATURE_ANALYSIS.md) for the Iroh-versus-`noq` tradeoff.

The workspace pins Iroh revision `cc876c75e53171dfcaf2e5fa0b81a417904694ba`.
That upstream revision admits stable `ed25519-dalek` and `curve25519-dalek`,
allowing Iroh to share Heddle's root dependency graph before the fix reaches a
crates.io release. The experiment also pins the published `heddle-api` 0.1.2
wire contract used by current main.

## Wire shape

- ALPN: `heddle-sync/3`.
- One logical operation owns one bidirectional stream on an established
  connection. The server handles independent operation streams concurrently.
- A request is `method_len:u16be | fully_qualified_method | body | FIN`. The
  governed method path is shared by dispatch and request signing instead of
  introducing experiment-only numeric operation IDs. A unary response is
  `protobuf | FIN`, so it needs no response tag or length.
- A pull response is `ready_len:u32 | pack_len:u64 | index_len:u64 |
  PullReady | pack | index | PullComplete | FIN`. The known raw-body lengths
  retain Heddle's pack and index size ceilings without connection-global stream
  correlation.
- Pack and index files are sent as owned 1 MiB `Bytes` chunks through Iroh's
  native `SendStream::write_chunk` path and received into the file-backed spool
  through one reusable 1 MiB buffer. Each awaited send is backpressured by
  QUIC's rolling stream credit, so memory does not grow with transfer size.
- The endpoint keeps Iroh's default 1.25 MiB rolling per-stream receive window
  and requests one acknowledgement per 50 ACK-eliciting packets. The latter is
  an experiment result that still needs lossy-path and WAN validation.

The experiment intentionally does not implement auth, pushes, resumability,
relay discovery, or 0-RTT replay policy.
