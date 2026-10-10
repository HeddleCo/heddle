# Local native Git window

This is the next local integration milestone after the separately preserved
read-only Rust performance checkpoint. It adds ordinary Git push and complete
supported history to one existing, public, same-Thread native source. It does not
turn the pinned `/views/<pin>.git` API into a mutable repository.

## Run and test

Build the examples with the repository's Rust toolchain:

```sh
cargo build --locked -p heddle-git-projection --example gateway_host --example gateway_agent_demo
python3 prototypes/git-gateway/test_window.py
```

The test creates an isolated synthetic native repository and local identity,
starts the Rust server on loopback, and drives real Git clients. Python is test
orchestration only; it does not serve Git or proxy requests.

For an existing synthetic fixture, write a canonical JSON config (recursively
sorted keys, compact separators, one final newline) and run:

```sh
HEDDLE_HOME=/absolute/isolated-fixture-home \
  target/debug/examples/gateway_host --local-window /absolute/window.json \
  --bind 127.0.0.1:18780
```

Config fields are `schema: 1`, `scope: "quiescent-synthetic-local"`, an
`expires_at` Unix time no more than one hour ahead, a lowercase `repository`
name, existing `thread`, absolute `native` and `catalog` paths, distinct
`reader_sha256`, `writer_sha256`, and `service_sha256` values, an authenticated
`actor` label, and a 64-hex `policy_generation`. These are local fixture grants,
not hosted Heddle credentials or authority. The server never creates a signer;
it loads the existing retained local owner signer for that Thread.

The stable URL is `/repositories/<repository>.git`. Every request needs reader
`Authorization: Bearer …` and `X-Gateway-Service-Authorization: Bearer …`.
Receive discovery and push additionally require
`X-Gateway-Write-Authorization: Bearer …`. Git author/committer text never grants
write authority. The trusted actor is recorded in the durable native receipt.
Set `git -c http.postBuffer=16842752` for larger fixture pushes so Git uses an
explicit Content-Length; chunked requests are deliberately refused.
Only this explicit loopback mode enables receive routes. Default hosted mode
remains read-only and refuses missing live disclosure authority.

## Durable acceptance and publication

1. Under the shared native repository mutation lock, authorize the request and
   verify the published native head and its entire public, admitted history.
2. Parse exactly one existing-branch update. Seed a fresh quarantine with only
   its authorized base closure. In a cleaned, resource-limited Git subprocess,
   index the bounded incoming pack, resolve thin bases, and run strict fsck.
3. Convert reachable commits/trees/blobs through Heddle's Rust import code and
   prove reconstructed Git commit bytes and OIDs are unchanged. Enforce bounds,
   fast-forward ancestry, visibility, local ownership, and signed native
   admission. Unknown or private source ancestry is not borrowed from a mirror.
4. Atomically accept signed native originals, authoritative source head,
   possession, and semantic retry receipt under generation CAS. Recheck current
   writer policy inside that transaction. Immutable objects staged before a
   rejected transaction may remain unreachable; they are not published heads.
5. Verify a new empty full-history projection, repair the rebuildable legacy
   Thread ref, and fsync/rename small versioned local catalog metadata and its
   current pointer. Send Git `unpack ok` / `ok` only after this finishes and a
   final authority check passes.

Native acceptance and catalog publication are two stages, not a fictitious
cross-store transaction. A durable pending command precedes acceptance. If
publication fails after native acceptance, readers refuse the inconsistent
head. An authorized writer's next receive discovery replays the durable receipt
and finishes publication before advertising the new head. Thus an ordinary
`git push` retry can recover after a failed response without duplicating native
commits. Replay requires current writer authority and the original semantic
operation scope. A changed policy generation may need an explicit operator
reconciliation; it is not silently blessed.

For a deterministic local publication-failure test only, creating the file
`<catalog>/publication-paused` pauses stage 5. Remove it and retry ordinary Git
push. It cannot enable hosted writes or bypass an authority check.

The catalog here is a small durable filesystem adapter. It contains native
state/Git OID metadata and receipts, not Git trees or packs. Artifacts/R2 write
publication and hosted transactional authority are **not implemented**. The
existing Cloudflare front door is not wired to this local write mode.

## Visibility and compatibility

Reads and writes hold the same repository lock used by supported local
visibility mutations, in addition to native source generation CAS. Every
request revalidates current policy; reads project the complete authorized public
closure into a fresh private Git database. There is no shared mixed-object
store. This local write mode currently has no projection cache; the earlier
warm-parity benchmark measures the immutable read-only cached mode.

Full history means the complete supported closure or an explicit refusal. The
current default is at most 128 native States and 64 MiB of decoded closure;
receive packs are at most 16 MiB / 20,000 objects, with further object/tree/commit
limits. Existing same-Thread commits and merge topology are preserved. Native
context, reviews and admission features remain Heddle-only. Snapshot clones
have different parentless identities and may need a fresh full-history clone.

The initial window supports one existing branch and fast-forward pushes. New
branches/tags, branch deletion, force updates, multiple updates per request,
push options, signed push certificates, arbitrary native cross-Thread
integration dependencies, SHA-256 repositories, gitlinks, and lossy/unfaithful
commit encodings are refused. Complete Git history rejects redacted closures
rather than silently changing historical OIDs. Native original visibility and
current restrictions both apply. Bytes already cloned cannot be revoked.

Before hosted use, implement a real reader/writer disclosure authority, bind
current visibility/admission/ownership/retention to durable acceptance, add
recoverable Artifacts/R2 publication, and test cloud concurrency, interruption,
large histories and resource behavior. Local synthetic grants cannot substitute
for that authority.

## Optional native publication preparation

The [native publication adapter](NATIVE-PUBLICATION.md) separates explicit signing
and exact historical SourcePack preparation from acceptance. It is opt-in local
validation work; it does not connect this window to a hosted write service.
