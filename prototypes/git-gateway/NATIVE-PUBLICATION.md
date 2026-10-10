> Preserved historical preparation-only adapter checkpoint. Current Heddle 0.30.0 hosted authentication, receiver acceptance and publication code is described in [HOSTED-PUBLICATION.md](HOSTED-PUBLICATION.md) and [HOSTED-RUNTIME.md](HOSTED-RUNTIME.md). Statements below about missing hosted interfaces apply to this earlier path, not the current implementation.

# Native publication adapter, local validation only

This opt-in Rust library slice separates Git conversion, signer custody and
native byte publication. It reuses Heddle's current SourcePack preparation and
receiver validation APIs. It does not enable hosted Git writes or change the
existing loopback/window or immutable Cloudflare routes.

## Build and verify

```sh
export HEDDLE_HOME="$(mktemp -d)"
cargo test --locked -p heddle-git-projection --features gateway-publication
cargo clippy --locked -p heddle-git-projection --features gateway-publication --all-targets -- -D warnings
```

The optional `gateway-publication` feature enables `gateway_publication` and the
transport-neutral `thread-api` source-transfer surface. It does not start Iroh,
open a network connection, read an identity directory, enroll a device or create
credentials during preparation. Tests use isolated synthetic identities.

## Preparation and signing

`gateway_write` separates an immutable prepared Git update from signing and
native acceptance. The caller explicitly supplies the authenticated actor,
publisher key and SourceAuthor envelope before preparation, because those
signed fields also affect causal operation IDs. The result binds the original
native/Git heads, Thread, generation, intended policy scope and canonical source
operations. Immutable source objects may be staged; preparation does not advance
native heads or create an acceptance receipt.

The caller signs externally. Binding those signatures must check exact canonical
operation bytes and publisher, current source-author permission, native
head/generation and the complete resulting history's visibility and budgets.
After an external signing interval, binding requires a freshly opened current
Repository. Its configuration is held in memory; reusing a stale handle does not
reload on-disk default visibility or Git projection settings. Any deliberate
in-memory overrides must be authorized again under the caller's current fence.
No Git author/committer text or arbitrary HTTP actor label creates a native
account identity. The existing local fixture wrapper explicitly uses its
already-provided synthetic signer; the new boundary performs no owner-key
lookup or fallback.

The approved hosted design uses a scoped gateway identity: its signature is
visibly attributed to the gateway, with the authenticated user's account/author
authority cryptographically verified. This API keeps signer custody external.
A real account envelope must be verified against current authority, revocation,
Spool, publisher and Thread audience. Loading a local owner key or substituting
an account label is not an enrollment or ownership transition.

## Exact historical source hydration

A SourcePack contains the selected revision's native source closure. Ancestor
State metadata alone does not contain their historical trees and blobs.
`PreparedHistory::prepare` therefore walks the exact supported same-Thread
capture ancestry and prepares a separate native SourcePack for every required
historical revision. Each pack retains its revision-specific original causal
proofs and reference descriptors. Cumulative State, decoded-content and artifact
budgets cover the entire selection, including repeated historical content.
An additional cumulative 16 MiB metadata limit covers repeated causal originals
and later acceptance signatures. Input metadata is bounded before verification.
Missing historical content fails before publication preparation completes.

The caller supplies the exact Thread/source/destination context, Spool genesis,
sharing-policy frontier and retry operation identity. A fresh disclosure callback
runs before and after hydration; the caller must hold the corresponding native
mutation/authority fence. Each send additionally requires a fresh verifier for
the exact prepared proposal and a returned guard held through the exchange.
Revocation before send must stop before any transport call. Successful
preparation is not reusable future read or write permission.

This initial adapter deliberately refuses unsupported claimed/hosted/imported
provenance that needs independently retained authority carriers. It does not
strip those carriers, treat a retained receipt as fresh authority, or infer a
cross-Thread grant. The full-history API's broader supported local read scope
is separate from this first publication adapter's narrower capture-only scope.

## What receiver validation proves

`PreparedRevision::validate_received` invokes the shared actual receiver code on
complete files in an isolated directory. It verifies native pack/index bytes,
cryptographic original records, selected revision and complete causal/source
closure. It never borrows hidden objects from a shared store. This is stronger
than comparing a predicted digest, but it is still structural validation.

`PreparedHistory::for_git_push` binds the exact signed preparation, original
genesis and current sender head/generation to these source packs. It does not
turn a sender-side fence into a receiver-side transaction.

`PreparedRevision::send` uses the current native publication API and returns its
byte-publication receipt. Publication proposals can receive an explicitly
supplied external acceptance signature without rewriting original author
claims. Neither a validated pack nor a successful upload is a Git acceptance
receipt. No hosted trust transaction, source-head mutation or Artifacts/R2 ref
publication is implied by this adapter.

## The missing hosted Git fence

The current PublishContent protocol makes an already-admitted source capture
available. Metadata replication owns causal heads. Its request has no Git
expected-old-head compare-and-swap field, and the legacy hosted client's
`push_with_expected_head_profiled` ignores that argument. A preflight head read
followed by byte upload cannot close the concurrent-native-writer race.

The gateway must refuse hosted Git acknowledgement until a receiver-owned
acceptance contract atomically binds:

- Authenticated actor, publisher, account-author authority and exact Thread/Spool
- Expected old native/Git heads and current disclosure/policy generation
- Exact prepared source operations, object closure and semantic retry identity
- Signed native admission, durable source-head CAS and durable acceptance receipt

Current local acceptance has a real native generation/CAS transaction. It must
not be exported as a fabricated remote CAS token. Any wire extension belongs in
the shared heddle-api contract first, with matching receiver implementation and
capability discovery. No such upstream API change or deployment is performed by
this local slice.

`SignedGitPush::require_hosted_git_acceptance` unconditionally returns
`HostedHeadFenceUnavailable`. There is no success constructor that lets a caller
turn a sender-side observation or publication receipt into receiver acceptance.

## Remaining integration work

After native acceptance, exact native content must be durably available and the
small versioned Artifacts catalog must publish the corresponding view under its
own CAS. Native acceptance and catalog publication need explicit recoverable
states; Git success waits for verified publication. Current disclosure authority
must cover every hydrated revision, original author/admission constraint,
redaction, ownership and retention fact on reads and writes.

Still required: verified hosted author/signing integration, authority carriers
for broader provenance, live disclosure producer, atomic hosted old-head fence,
durable R2/Artifacts publication/recovery, stable Worker write routes and bounds,
and authorized Container/cloud verification. Real credential creation,
enrollment, resource mutation and deployment remain separate approval gates.

The integration boundaries are:

1. **Author and writer authority:** verify the authenticated account/actor,
   gateway publisher key and capability against the current Thread/Spool scope.
   Preserve the original signed source authors. The current API accepts an
   explicit verifier callback; it supplies no live account-authority client.
2. **Receiver transaction:** extend the shared native acceptance contract with
   an expected-old native/Git fence and semantic operation identity. Author and
   disclosure checks must participate in that transaction, including concurrent
   native writers. A replay must return the same durable outcome under current
   authorization; a byte-upload receipt alone cannot prove it.
3. **Content and catalog recovery:** record native acceptance separately from
   complete content availability and catalog publication. Publish a small exact
   history manifest by compare-and-swap only after every required revision is
   available. Recover committed-but-unpublished outcomes without duplicating
   native captures. Git acknowledgement follows verified publication.
4. **Read visibility:** the receiver's current disclosure generation must cover
   every selected ancestor, dependency, original-author constraint, redaction,
   ownership and retention fact. The read host must validate it before and after
   preparing bytes, including cache hits. Cached bundles and grants are not a
   current authority source.

These are code/service integration gates, separate from approval to enroll a
real scoped identity or activate Cloudflare resources. This checkpoint stops at
the locally verified preparation/validation boundary.
