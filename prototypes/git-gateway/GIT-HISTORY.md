# Complete Git history from admitted native source

The library entrypoint `gateway_view::export_public_git_history` projects the
union of explicit `(Thread, State)` tips into a new, empty caller-owned Git
repository and returns the existing bidirectional `SyncMapping`. The host owns
ref naming, expected tip OID checks, authorization, current disclosure checks,
and atomic publication of an advancing generation.

This API does not turn every object in a native repository into public Git data.
Every selected tip and dependency must be explicitly authorized and backed by
accepted signed originals. Fork bases and cross-Thread integration retain their
existing exact-operation, same-local-owner checks. Every selected ancestor and
tree must be present and public, including original captured restrictions.
One disallowed selected branch rejects the entire union before any object write.
Unselected branches and unrelated native objects never enter the sink.

## Identity and ancestry

- Native-authored States reuse the existing Git exporter and Heddle identity
  footer. Ordered parents are preserved, including merge parent order.
- Shared ancestors are emitted once. Moving a ref forward preserves earlier
  OIDs under the same projection context, including hosted URL/footer policy.
- The canonical native initialization seed is verified as provenance but is not
  emitted as a Git commit. A seed-only tip has no Git history and is refused.
- Imported Git States are permitted only when byte-faithful and independently
  admitted as signed native source. The same existing reconstruction code retains
  raw message bytes, actor identities/timestamps/timezones, and extension headers.
  A Git write adapter verifies the received commit's exact bytes/OID before
  native acceptance; `raw_message` alone is not that proof.
- The full canonical State in each signed original must equal the stored State.
  This includes `git_lossy`, which is deliberately outside the State content hash.
- Lossy imports, residual-only history, nested repository edges, private or
  partial closures, and missing ancestry remain unsupported and fail closed.
- Redacted closures are refused by this Git-compatible path. It never rewrites
  established Git history to stubs while claiming unchanged identity, nor falls
  back to retained original objects. The older native/snapshot view still uses
  its existing safe redaction stubs.
- Source notes, native context/discussions, credentials, source Git mirrors, and
  source mapping caches are not copied. Rich Heddle features remain native-only.

The complete selected closure must fit the caller's explicit `ViewLimits`.
Current prototype defaults are 128 native States (including initialization),
10,000 tree entries, 64 MiB cumulative blob bytes, and 16 MiB per blob. Native
original verification has an additional 4,096-operation/16 MiB budget shared
across tips. At most 128 selected tips and 128 explicit Thread names are accepted.
These are rejection bounds, not a depth cutoff: an over-budget history never
becomes a partial or fabricated history. Large-history paging/streaming is not
implemented by this entrypoint.

Before native Git-write acceptance, `preflight_prepared_git_history` evaluates the
final old-plus-new closure through this same preflight. Prepared signed local
captures are only an inspection overlay; existing admitted originals remain in
the privacy intersection. Combined State, entry, repeated historical blob-byte,
and signed-original work/byte limits must pass before source heads and the retry
receipt are committed. Bounding incoming objects separately is insufficient.
The helper writes neither Git objects nor native admission records.

A crate-private callback variant allows an adapter to inspect prepared originals
against its independently verified current publisher/SourceAuthor authority.
Signatures and governing Thread are checked before that callback, and the same
canonical source/privacy/budget preflight still applies. The local wrapper keeps
its existing local-owner check. Neither callback success nor preflight success
constitutes persisted source admission, receiver CAS, or a hosted writer grant;
the public exporter continues to require accepted originals.

## Moving from the snapshot demo

A non-root snapshot deliberately has no Git parents, so its OID differs from the
full-history commit for the same native State. Switching modes is not promised
to fast-forward an existing snapshot clone. Use a fresh full-history clone or an
explicitly reviewed reset, keeping local work safe. Ordinary fetch/pull advancement
is proven between full-history generations with unchanged projection context.
Mode must remain part of catalog/cache identity.

## Local proof

`crates/git-projection/tests/gateway_history.rs` exercises actual Git commands:
clone, log, branch creation, two-parent merge, fetch, and fast-forward pull across
fresh rebuilt source generations. It checks old OID stability, merge parent
order, exact reachable-object inventory, byte-exact imported reconstruction,
whole-union budget/refusal, withheld branches, forged lossiness changes, and
redaction refusal. No hosted deployment or live writer authority is implied.

Run with a writable isolated Heddle test home:

```sh
export HEDDLE_HOME="$(mktemp -d)"
cargo test --locked -p heddle-git-projection --test gateway_history
cargo test --locked -p heddle-git-projection gateway_view --lib
cargo test --locked -p heddle-git-projection --test gateway_visibility_security
```
