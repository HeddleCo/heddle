# Encrypted, versioned, agent-scoped env/secret store

**Status:** spike · **Tracking:** heddle#999 · **Review:** Fable-reviewed against the real object model (corrects an earlier over-optimistic sketch). Complementary to weft's server-side `SecretStore` (envelope AES-GCM for provider tokens) — different trust anchor, see §8.

Solve the `.env` problem natively: a private, versioned, **agent-scoped** secret store built on heddle's
existing primitives — content-addressed blobs, threads, states, oplog, redaction/purge, biscuit
attenuation, and the local daemon. The value over `sops`/`age`-in-git is not encrypted storage per se
(they have it) — it's **attributable, oplog-audited, signed secret history with real redaction** and
**agent-scoped, time-boxed, daemon-mediated decryption**. Those two are the reason to build it *in
heddle* rather than ship a `sops` tutorial.

## 1. Object layout — NO new object type

The earlier sketch proposed a new `SecretState` object kind. That is the expensive path: the
`ObjectType` enum (`crates/objects/src/store/pack/mod.rs`) is closed (`Blob/Tree/State/Action/Delta`)
and a new kind ripples through the pack container, wire protocol, grpc proto, and the
exhaustiveness-gated `OpRecord` enum. **The model already gives us the cheap path:**

- **Ciphertext = ordinary `Blob`s** (BLAKE3 content-addressed, `crates/objects/src/object/hash.rs`).
- **Env slots = tree paths** — e.g. `env/production` thread's tree holds `DATABASE_URL`,
  `STRIPE_KEY`, … each an encrypted blob at a path.
- **One `State` per version**, on a dedicated **`env/*` thread** (`env/production`, `env/staging`,
  `env/local`). Every change is a new immutable state with a fresh `ChangeId` — full history,
  attribution (principal/agent), diff, and rollback for free via the existing state + oplog machinery.

Nothing new in the object model. The store *is* a thread whose tree happens to contain
recipient-encrypted blobs.

**Caveat — threads are checkout targets.** `heddle goto env/production` would replace the working
tree with the secrets tree; `env/*` threads must be flagged non-checkoutable (or the CLI must refuse
`goto` on them) so a "slot" is never accidentally materialized as a worktree.

## 2. Encryption identity (decision 1)

heddle's local identity is an **Ed25519 signing** key (`crates/crypto/src/ed25519.rs`) — there is **no
encryption primitive in the tree today** (crypto crate is `ed25519-dalek` + `p256`, signing only). You
cannot encrypt *to* an Ed25519 key directly.

- **Software keys:** derive an **X25519 encryption subkey via domain-separated HKDF from the Ed25519
  seed** (`SigningKey::from_bytes` exposes it). This is cleaner than age's birational ssh-ed25519
  conversion — no cross-primitive reuse of the *same* keypair; a versioned HKDF context string
  (`"heddle-env-x25519-v1"`) gives domain separation. One local identity, one derived encryption key,
  no new key file.
- **Non-extractable identities (P-256 / hardware / enclave):** derivation-from-seed is impossible.
  Fallback: a **standalone X25519 keypair, endorsed (signed) by the identity key**, published as the
  principal's encryption pubkey. Slightly weakens the "no new key material" elegance but keeps hardware
  principals first-class.
- **Rotation coupling (decide explicitly):** deriving the encryption key from the identity seed means
  **rotating the identity re-encrypts every secret**. Default stance: the derived encryption key gets a
  **versioned derivation index** (`…-v1`, `…-v2`) so the encryption key can rotate on its own cadence
  without forcing an identity rotation, and vice-versa. Old versions stay decryptable by the old
  derived key until purged (§5).

## 3. Symmetric encryption (decision 2)

- **Random-nonce AEAD** (XChaCha20-Poly1305 or AES-256-GCM — match weft's `secrets/envelope.rs`
  choice for one audited primitive across the codebase). Per-secret **DEK**, wrapped to each
  recipient's X25519 key (age-style recipients), fresh random nonce per encryption.
- **Dedup is an explicit NON-GOAL.** Deterministic encryption would preserve BLAKE3 dedup but **leak
  plaintext equality** — which vars are unchanged across a "rotation" (rotation-theater detection),
  which two environments share a credential (prod-password == staging-password is sensitive), which
  two users hold the same token. Env files are kilobytes; forfeit dedup without a second thought.
- **Pad value lengths to buckets** — ciphertext length ≈ value length otherwise, and tree-diff
  structure already reveals *which* slots changed between versions. Padding blunts the length leak; the
  which-slot-changed leak is accepted (or mitigated by a single opaque per-version blob if it matters).

## 4. Access control — key-in-daemon (decision 4)

A biscuit check is **only as real as the boundary that enforces it.** If the CLI holds the X25519 key
on the same disk and self-verifies, a compromised agent just reads the key file — the biscuit is
auditing theater. It becomes genuine defense-in-depth in exactly one local topology:

- **The daemon holds the key and performs decryption.** Agents talk to the daemon over the existing
  unix-socket grpc (`crates/daemon`) with **attenuated biscuits** and receive **values, never key
  material** — scoped, time-boxed (`expires_at` verifier-injected per request, per
  `docs/SELF_SOVEREIGN_AUTH.md`), and oplog-audited.
- **Biscuit vocabulary extension** (`crates/client/src/device_flow.rs`): today's caveats are
  `agent_id` / `allowed_operations` (gRPC method names) / `allowed_resources` (`kind ∈ {repo,
  namespace}`). Add a resource kind for env slots (`kind: "env-slot", path: "env/production"`) and a
  decrypt operation, on **both** the client builder and every verifier. Extending biscuit *facts* is
  cheap; extending every *verifier* consistently is the work.
- **Honest threat-model framing:** this does **not** beat root or same-UID malware (nothing local
  does). It beats **"every spawned agent can read every secret"** — the confused-deputy problem that is
  heddle's *actual* threat model (agents running under your identity). "This subagent may read
  `env/staging` for 30 minutes and nothing else" is expressible here and nowhere in `sops`.

**Without key-in-daemon, drop the biscuit layer** and document honestly that it's file-permission
security.

## 5. Rotation = revocation = purge, one enforced flow (decision 3)

Immutable history is a real liability for secrets: every historical version encrypted to a principal
stays decryptable by that principal's key **forever**; one key compromise unlocks *all* history, not
just the current value. Removing a recipient does not un-leak what they could already read.

heddle is better-armed than a plain git store: it already has **signed redaction**
(`crates/cli/src/cli/commands/redact.rs` — `OpRecord::Redact`, blob-hash fan-out across renames/branches
with `--all-states`) and **physical purge** (`crates/cli/src/cli/commands/purge.rs`, with the honest
self-documented `blob_remains_in_pack` caveat: purge removes loose bytes but does **not** repack, so
purged bytes can survive in packfiles until an operator rewrites packs).

**So define one flow, and enforce the links:**
- **Recipient removal ⇒ rotate.** Removing a principal surfaces *"these N secrets they could read are
  now presumed leaked — rotate them"* and refuses to call the job done until they are. (This enforced
  link is itself a **differentiating feature** no `sops`-alike offers.)
- **Rotation ⇒ purge.** Writing a new value invokes **redact + purge** on the superseded ciphertext
  blobs (accepting the documented pack-residue caveat; a `--repack` follow-through closes it for the
  paranoid).
- **`env/*` projection-exclusion + mandatory ignore — BEFORE v1.** The git overlay's export walks
  **all** threads (`export_all`, `crates/cli/src/git_projection_engine/git_core.rs`) — an unguarded
  `env/production` thread becomes a pushed git branch full of ciphertext. There is no thread-exclusion
  policy today; add one so `env/*` never projects. Inverse footgun: a decrypted `.env` materialized in
  the worktree gets captured by the next `heddle snap` into a *normal, unencrypted* state — so the
  ephemeral-injection path (§6) must never write to the worktree, and `.env`/`env/*` outputs get
  mandatory `.heddleignore` coverage. **Both land before any v1 or the store leaks through its own
  overlay.**

## 6. Injection — ephemeral only

Decrypted values must **never** flow through the annotation/context path. Annotations
(`crates/cli/src/cli/commands/context/`) are persisted, content-addressed, versioned objects that also
surface in JSON command output and the daemon grpc surface — routing secrets there writes plaintext
into the immutable store. Instead:

- **`heddle env run --thread env/production -- <cmd>`** — the daemon decrypts, injects values as
  **process environment variables** into the child, and they live only for that process. No worktree
  file, no store object, no log line. Optionally `heddle env export` to a shell eval (with a loud
  "this puts plaintext in your shell history" guard).

## 7. Bootstrap / distribution — the actual `.env` pain

Storage/versioning is the easy 70%; **key distribution is the 30% that defines every secrets tool**,
and "local-first, nothing pushed" is in direct tension with "team secrets" (inherently a distribution
problem). This design must answer:

- **New dev / recipient:** has no heddle identity until provisioned; once provisioned, an existing
  holder must re-encrypt the relevant DEKs to the new recipient's X25519 pubkey **and** the new
  ciphertext must sync somewhere they can fetch it. So ciphertext syncs (weft) and there is a
  `heddle env grant <principal>` ceremony.
- **weft as the pubkey registry** — principals publish their (endorsed) X25519 encryption pubkeys, so
  `heddle env grant <principal>` resolves keys without manual key-file swapping. **This is where heddle
  beats `sops`'s paste-keys-into-`.sops.yaml` UX**, and weft already has the identity/handle spine + the
  envelope-encryption machinery on its side.
- **Ciphertext sync** through weft push/pull (the `env/*` thread syncs like any other, but never
  projects to git — §5).
- **CI:** a machine principal whose X25519 key is delivered via the CI platform's own secret mechanism
  (GitHub Actions secret). Yes, you bootstrap the secret-store from the CI platform's secrets — every
  tool bottoms out here; the spike just says so plainly and ships a recipe.

Without these three (registry, sync, CI recipe) this is a beautifully versioned safe with no way to
hand out combinations.

## 8. Relationship to weft's `SecretStore`

Complementary, not competing:
- **weft `SecretStore`** (`weft-server/src/secrets/`): server-side, at-rest **envelope** encryption
  (AES-256-GCM, DEK-per-secret, swappable `KekProvider`) — trust anchor is the **KEK**. For provider
  OAuth tokens etc.
- **This store:** client-side **recipient** encryption — trust anchor is the **principal identity**.
  For user/team env secrets.

If `env/*` ciphertext syncs through weft, both schemes coexist; docs must be ruthless about which layer
protects what (weft never sees plaintext of this store — it stores opaque recipient-encrypted blobs).

## 9. Phased plan

1. **P0 — primitives + guards (must precede any real secret):** `env/*` thread convention + non-checkoutable
   flag; projection-exclusion for `env/*`; mandatory `.heddleignore`; the X25519-from-Ed25519 HKDF
   derivation (+ endorsed-key fallback) in `crates/crypto`.
2. **P1 — local store:** `heddle env set/get/list`, random-nonce AEAD + per-recipient DEK wrap,
   ciphertext-as-blobs on the `env/*` thread, version history via states. Single-recipient (self) only.
3. **P2 — daemon decrypt + `env run`:** key-in-daemon, values-over-socket, `heddle env run -- cmd`
   ephemeral injection. No biscuit gating yet (self only).
4. **P3 — agent-scoping:** biscuit vocabulary extension (`env-slot` resource + decrypt op), attenuated
   time-boxed decrypt, oplog audit of each decrypt.
5. **P4 — multi-recipient + bootstrap:** weft pubkey registry, `heddle env grant/revoke`, ciphertext
   sync, the rotation=revocation=purge enforced flow, CI-principal recipe.

## 10. Differentiators (the case FOR building)

- **Attributable, oplog-audited, signed secret history with real redaction/purge** — "who changed
  `DATABASE_URL`, when, under what session, prove it" *and* "actually remove the leaked bytes." A real
  gap over `sops`-in-git.
- **Agent-scoped, time-boxed, daemon-mediated decrypt** — genuinely novel for the agent-native use case
  heddle exists for; unexpressible in any `sops`-alike.

NOT differentiators (same solved-and-hard problem as everyone): encrypted versioned storage per se,
multi-recipient encryption, and the key-distribution core — which heddle currently solves for **zero**
of its principals, hence P4 is the make-or-break.
