---
status: accepted
---

# RuntimeProfile is a confidential-runtime facet, not Source History

A Heddle runtime profile is a **distinct confidential-runtime facet**. It is
not a Source History `State` or `Tree`, not a checkoutable thread, and not an
`env/*` source thread. A typed [`RuntimeProfileRef`] names a profile and points
at immutable [`RuntimeProfileState`] versions. Those versions carry named slot
records (ciphertext hashes and per-recipient DEK wraps), recipient and policy
references, attribution, and signed lifecycle records.

This decision locks the canonical model on [heddle#999](https://github.com/HeddleCo/heddle/issues/999).
It **rejects** the superseded spike sketch that stored secrets as ordinary
blobs on an `env/*` checkoutable thread. That design would have made
`heddle start` / checkout / `land` / Git Projection able to select secret
history by name prefix. A prefix is not a law.

The earlier `docs/spikes/encrypted-env-store.md` text that proposed
HKDF-from-signing-seed encryption keys and a daemon that holds exportable
private keys is also superseded. Those remain useful threat-model notes; they
are not the product.

## FacetKind

Durable repository facts belong to a closed `FacetKind`. The first members are
`SourceHistory`, `ConfidentialRuntime`, `Collaboration`, and `AgentTimeline`.
A facet's laws are not inferred from a path, thread name, or object-store
reuse.

Only `SourceHistory` yields `SourceHistoryLaws`. That token is the
compile-time proof that a root may be checked out, landed, or visited by Git
Projection. `ConfidentialRuntime` has none of those laws.

| Law | Source History | Confidential runtime |
| --- | --- | --- |
| Checkout / worktree materialize | yes | no |
| Land / merge into HEAD | yes | no |
| Git Projection visit | yes | no |
| Named thread in `refs/` | yes | no |
| VisibilityTier as confidentiality | possible | **not a substitute** |

`VisibilityTier::Private` is access policy over **plaintext** Source History.
It is not encryption and must not be used as this product.

## Encoding (local, versioned, not on the wire)

Runtime-profile bytes are Heddle-local. JSON, protobuf, and gRPC adapters are
out of scope until confidential-runtime sync is a Weft contract (heddle-api
first). V1 encoding:

- Canonical MessagePack envelopes with an explicit `schema_version`.
- One latest encoder, explicit version dispatch, no blind unversioned decode.
- Content-addressed identities over canonical bytes:
  - `RuntimeProfileStateId` — typed hash prefix `runtime-profile-state`
  - `LifecycleRecordId` — typed hash prefix `runtime-profile-lifecycle`
  - `RecipientId` — typed hash prefix `runtime-profile-recipient`
- `RuntimeProfileId` is a UUIDv7, stable across versions.
- `RuntimeProfileStateId` is **not** a `StateId`. The types do not convert.
  Git Projection and land continue to take `StateId` only.

A `RuntimeProfileRef` is the mutable typed root (atomically replaced):

- `profile_id`, `name`, `facet = ConfidentialRuntime`
- `head` — current `RuntimeProfileStateId`
- attribution and timestamps

A `RuntimeProfileState` is immutable:

- `profile_id`, `parent`, monotonic `version`
- `lifecycle` at the time the version was sealed
- slot records: name, AEAD algorithm, pad bucket, ciphertext hash, DEK wraps
- recipient descriptor ids and an optional policy-broker reference
- attribution

Ciphertext **bytes** may later reuse the content-addressed byte store. V1
keeps them in a dedicated `.heddle/runtime-profiles/` namespace so ownership,
reachability, authorization, sync, purge, and projection stay facet-aware.
Sharing loose objects without a facet-aware GC would let Source History
maintenance treat confidential ciphertext as ordinary blobs.

## Encryption and recipients

V1 is versioned **random-nonce AES-256-GCM** (the same audited AEAD family
weft uses for server-side envelopes). Each slot gets a fresh DEK. The DEK is
wrapped to each recipient. Values are padded to length buckets. Deterministic
encryption and ciphertext dedup are out of scope: they leak modality
(unchanged values, shared credentials across profiles).

Heddle does **not** derive an encryption private key from the signing seed by
default. A selected **provider** creates or imports a versioned recipient key
and exposes a public descriptor **endorsed** by the principal's signing
identity. Algorithms are versioned and agile.

Provider capabilities (v1 names the set; only software is implemented):

| Capability | Custody | V1 |
| --- | --- | --- |
| `software-exportable` | Exportable X25519 key on disk (0600). Explicit weaker-custody fallback. | yes |
| `tpm` / `secure-enclave` / `os-provider` | Hardware or OS key store; broker holds a handle | later |
| `pkcs11` / `remote-hsm` / `kms` | External custody | later |

The policy **broker** (later, separately approved IPC) authorizes scoped,
time-boxed decrypt requests and returns **values, never key material**. The
broker holds provider handles, not exportable private keys. Hardware protects
custody; the broker enforces authorization, revocation, and audit. Strong
agent isolation needs distinct OS/container identities. Same-UID callers are
cooperative, not an adversarial boundary.

Until the broker exists, the library decrypt path that accepts a software
recipient secret is the weaker-custody fallback only. It is not the
agent-facing security boundary. This slice does **not** add `heddle env` or
`heddle runtime-profile` CLI that would pretend otherwise.

## Lifecycle

Signed lifecycle records are first-class, not inferred from the latest state:

`staged → active → superseded → revoked → purge-eligible → purged`

- Creating a profile writes `staged` then `active`.
- Updating slots writes a new immutable version (`staged` then `active`) and
  marks the previous head `superseded`.
- Recipient removal (later) denies future wraps but does **not** un-leak
  history; affected credentials must be rotated.
- Supersession may retain a bounded rollback window. Purge is not complete
  until ciphertext bytes and any pack residue required by policy are gone.
- Decrypt refuses `revoked`, `purge-eligible`, and `purged` versions.

Each transition is a canonical payload signed by the principal's signing
identity (Ed25519 or P-256). The signature endorses the transition; it is not
an encryption key.

## Sync

V1 is local-only. Hosted sync is a later confidential-runtime lane, not
Source History push/pull and not Git.

When sync lands it must stay facet-aware: ciphertext, recipient descriptors,
and policy data travel as confidential-runtime objects. Weft verifies; this
CLI still mints client roots. Weft's existing server-side `SecretStore`
(KEK-anchored provider tokens) is complementary, not this product.

## Projection and capture guards

These guards are part of the accepted model even though `env run` and the
broker are later slices.

1. **Git Projection selects typed Source History roots only.** It never walks
   runtime profile roots, ciphertext, recipient descriptors, or policy data.
   `export_state` / `export_all` require `SourceHistoryLaws`. A
   `RuntimeProfileStateId` cannot be passed where a `StateId` is required.

2. **Checkout and land** resolve threads to Source History `StateId`s. They
   require `SourceHistoryLaws` at the materialize and merge chokepoints. A
   runtime profile cannot be started into a worktree or landed onto HEAD.

3. **`env run` (later)** injects plaintext into a child process only. It must
   not write the source worktree, the object store, command output, or logs.

4. **Source capture (later, before any materialization path)** must reject
   broker-known secret fingerprints and reserved materialization paths.
   `.heddleignore` is defense in depth, not a security boundary.

5. Intentional declassification (for example an `.env.example` schema) must
   be explicit and audited.

## Consequences

- No `env/*` thread convention. No non-checkoutable-thread flag as the
  safety story. The facet is absent from `refs/` and from Source History
  object kinds.
- No second view RPC and no heddle-api change in this slice. Local types
  first.
- Software recipient keys on disk are documented weaker custody. Do not
  market the library decrypt API as agent isolation.
- Do not add `ObjectType::RuntimeProfile` to the pack/transfer graph until
  facet-aware sync exists. Ciphertext must not become an ordinary blob that
  Git Projection or source GC can discover by walking states.

## Phase 1 local-store caveats

These are accepted limits of the library MVP, not the long-term model:

- `purge()` deletes ciphertext files for the **current head version only**.
  Superseded versions keep their ciphertext until a later retention pass.
- `update_slots` is a full slot-set replace, not a merge of named keys.
- The store has no multi-writer file lock. Concurrent local writers are
  undefined until the broker owns mutation.
- Version files are content-addressed and unsigned. Authenticity is on the
  signed lifecycle records and the recipient endorsement, not on each
  `RuntimeProfileState` blob.

## Considered options

**`env/*` source thread of encrypted blobs.** Reuses State/Tree/oplog for
free, but threads are checkout and projection targets. A name-prefix guard
is a convention, not a type. Rejected by heddle#999.

**Derive X25519 from the Ed25519 signing seed.** One identity file, no new
key material. Couples encryption rotation to identity rotation, cannot
support hardware principals, and puts decryption capability in every process
that can sign. Rejected as the default.

**Daemon holds exportable private keys.** A biscuit check is only as real as
the process boundary. Same-UID agents can read the key file. Replaced by
provider handles plus a policy broker that returns values.

**VisibilityTier / Private states.** Plaintext with an audience tag. Not
confidential runtime.

**New pack `ObjectType` in this slice.** Would ripple through pack, transfer,
and proto before any sync story exists. Local dedicated store first.
