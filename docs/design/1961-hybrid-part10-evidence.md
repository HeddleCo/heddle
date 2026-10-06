# HYBRID Part 10: Fetch foreign cuts and receiver clock sampling

Fetch recognizes an exact foreign original before loading its causal parents.
It records that original as a structural endpoint and stops the traversal there,
including during `StagedSource::select_prefix`. Native originals retain their
ordinary exact ancestry validation. Staging confers no authority: hosted install
still resolves every foreign dependency through the exact retained prefix under
current receiver trust, with the existing `NativeClosure` verifier.

The host Fetch contract is the selected native ancestry (including integration
source edges and required native claim cutoffs), dependency Genesis records,
and each exact `ForeignDependencyV1` endpoint original. Stop causal traversal
at those exact endpoints. Do not include ancestry behind them merely to satisfy
native staging; unused originals are refused. The host still supplies the
selected State's actual content/reference closure in its pack and index.
A fresh receiver fetches and installs each foreign origin's exact admission
prefix separately before admitting the dependent native closure. That separate
prefix carries the original ancestry and import certificates needed by its own
origin verifier; it does not turn that ancestry into native causal history.
An unmatched signed digest or Thread identity receives no endpoint exception.

Each wall reading is now bracketed by monotonic readings. Rollback compares the
wall difference with the minimum elapsed time outside both brackets. Scheduling
inside either bracket has no fixed bound and cannot masquerade as rollback.
The existing one-millisecond truncation bound remains; signed expiry is never
extended. Monotonic regression inside a bracket or between brackets refuses,
as does an observed wall rollback or time below the durable SQLite floor.
Snapshot, mutation, installation, commit and final access sampling all use the
same internal sampler. There are no retries, sleeps or wider clock tolerance.

## Consumer API

The `Clock` trait and all staging/installation signatures are unchanged:

```rust
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> repo::thread_replication::Result<i64>;
    fn elapsed_millis(&self) -> repo::thread_replication::Result<u64>;
}
```

`thread_api::fetch::Error::Repository(Box<repo::thread_replication::Error>)`
is removed. Fetch carries the small typed projection required at its protocol
boundary instead:

```rust
thread_api::fetch::Error::ForeignPrefixLimitExceeded {
    limit_name: &'static str,
    limit: usize,
}
impl From<repo::thread_replication::Error> for thread_api::fetch::Error
```

That conversion preserves repository prefix-limit fields; other repository
failures become `Preparation(String)`. Weft callers matching the former boxed
variant must match this variant directly. The hosted-client conversion preserves
`ProtocolError::ForeignPrefixLimitExceeded { limit_name, limit }` without a heap
box. Wire types, crypto and capability verifier code are unchanged.

## Regression evidence

The original Fetch path failed the actual Ready/operations/pack/index/Complete
receiver test with `Invalid("incomplete source ancestry")` while loading omitted
foreign parents. Its controls cover native fast-forward and merge landing;
the bidirectional continuation case exercises a foreign original with a causal
Git ancestor and also selects its exact native prefix before hosted install.
The same test passes after the endpoint cut, including fresh receiver prefix
installation and the native hosted installation.

The original clock path failed
`receiver_clock_sampling_delay_does_not_report_rollback` at
`sampling delay must not masquerade as rollback: HostedClock` (exit 101).
The bracketed sampler passes delayed wall and monotonic sampling, including an
arbitrary 4,000,000,000 ms scheduling gap during snapshot history reads.
Real wall and monotonic rollback remain typed `HostedClock` refusals.

Final gate and isolated mutation receipts will be recorded after execution.

## Surfaces

Verb/help/clap and human/agent output contracts are unchanged. This is a native
receiver validation change; Git import/export/projection continues through sley.
No wire fields or new RPCs are added. Forged-reference, native ancestry, wall
rollback, monotonic rollback and retained-prefix refusal controls cover the
reverse states. No production unwrap/expect, fallback, compatibility shim or
trait-object dispatch is introduced.
