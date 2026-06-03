# Spike: automatic state/change signing via the biscuit local keypair

**Issue:** heddle#480 · **Status:** design / decision doc · **Audience:** maintainer sign-off
**Scope:** design + analysis only. No production behavior change; this PR adds only this file.

---

## 1. TL;DR

Heddle already ships every cryptographic primitive needed to sign each captured
state automatically: an ed25519 `Signer`, a `State.signature` field excluded from
the identity hash, and a one-call `repo.sign_state()` path. What's missing is the
**wiring** — nothing on the capture/commit path calls it, and the only user-facing
entry point (`review sign`) demands four hand-computed flags. Meanwhile the local
"biscuit identity" the maintainer wants to reuse is, concretely, the **ed25519
device-binding keypair** minted by `heddle auth login` — the same primitive,
already stored on disk, already known to the server by public key.

The recommendation: **reuse the device-binding ed25519 private key as the
state-signing key**, sign `state.compute_hash()` on the capture/commit/merge path
automatically (shape (a): direct ed25519 detached signature stored in
`state.signature`), and keep a biscuit-attestation token (shape (b)) as a
*future, opt-in* layer rather than the v1 substrate. `review sign` stays as the
explicit, multi-party *review-attestation* surface — it solves a different
problem (third-party reviewers signing someone else's state) and should not be
folded into the automatic author-signature flow.

One important feasibility caveat drives the whole design: **the device key only
exists once a user has authenticated against a hosted server.** A purely-local,
git-substrate-only repo has no such key. So automatic signing must degrade
gracefully to "unsigned" (or a locally-generated identity key) rather than
becoming a hard dependency on the hosted product.

---

## 2. What the maintainer means by "the biscuit local keypair"

There are two distinct key objects in play, and the issue framing conflates them.
Grounding both in code:

### 2a. The biscuit *root* keypair — server-side, not reusable locally

`biscuit-auth` is a workspace dependency (`Cargo.toml:46`, `biscuit-auth = "6"`)
but it appears in exactly one crate — the **client** — and only ever in the
**attenuation** role:

- `crates/client/src/device_flow.rs:72` — `UnverifiedBiscuit::from_base64(...)`
  parses a token the client *already holds*, appends an attenuation block
  (`:75 .append(block)`), and re-encodes. The module doc is explicit
  (`device_flow.rs:62-67`): *"Uses `UnverifiedBiscuit` because attenuation
  appends a new block… the new block's signature chains off the parent's keys…
  The CLI never holds the server's signing key."*
- `crates/client/src/grpc_hosted/mod.rs:215` — the actual biscuit is obtained via
  the `mint_biscuit` RPC; the **server** holds the root key and mints.

So the biscuit *signing* key (the root `KeyPair`) is **not available locally** at
capture/commit time. A design that tried to "sign the state with the biscuit key"
in the literal sense is infeasible — the client never possesses that key.

### 2b. The device-binding ed25519 keypair — local, and the real reuse target

What the client *does* hold locally is the **device-binding ed25519 keypair**,
generated during `heddle auth login`:

- `crates/client/src/auth_cmd.rs:67` — `Ed25519Signer::generate()` mints the
  keypair; `:68` exports `public_key`, `:70` exports `private_key_pem`.
- The public key is registered with the server as the **device public key**
  (`auth_cmd.rs:84` `device_public_key: public_key_bytes`), so the server can
  map this pubkey → an authenticated identity.
- The private key PEM is persisted to disk in the credential store:
  `crates/client/src/credentials.rs:41` `struct ServerCredential { … private_key_pem:
  Option<String>, credential_id: Option<String>, … }`, written to
  `~/.heddle/credentials.toml` (`credentials.rs:55-56 credentials_path()`) with
  `write_file_atomic_secret` at mode `0600` (`credentials.rs:80`, and the
  `save_credentials_writes_credential_file_0600` test at `:263`).
- This local key is what *gates biscuit minting*. It signs the proof the server
  checks before minting/rotating a biscuit:
  - per-request proof header `x-heddle-proof` = `sign("{bearer}|{proof_ts}")`
    (`grpc_hosted/mod.rs:106-107`),
  - the mint/rotation challenge `sign("{timestamp}\n{public_key_id}\n")`
    (`grpc_hosted/mod.rs:191`, request built at `:200-211` with
    `Proof::Keypair(KeypairProof{ public_key_id, timestamp, signature })`).

**Conclusion.** "The biscuit local keypair" = the device-binding ed25519 key.
It is (1) local, (2) ed25519 — the *same primitive* as `crypto::Ed25519Signer`,
(3) accessible at capture time (it's in `credentials.toml`), and (4) already
bound to a server-verifiable identity by public key. Reusing **it** is the
key-reuse the maintainer is after — and it requires **no new keys**.

---

## 3. The signing machinery that already exists (and is unused)

Heddle has a complete signing stack. The gap is purely that the capture path
never invokes it.

| Piece | Where | Notes |
|---|---|---|
| `Signer` trait (ed25519/rsa/p256) | `crates/crypto/src/lib.rs:28-33` | `algorithm()`, `public_key()`, `sign()`, `verify()` |
| `Ed25519Signer` | `crates/crypto/src/ed25519.rs:10` | `generate()`, `from_pem()`, `from_seed()`, `to_pem()` — exactly what the device key uses |
| `State.signature: Option<StateSignature>` | `crates/objects/src/object/state_core.rs:213` | the slot an auto-signature lands in |
| `StateSignature { algorithm, public_key, signature }` | `state_core.rs:43-47` | algorithm string + hex pubkey + hex sig |
| `StateSigningExt::sign / verify_signature` | `crates/crypto/src/state_signing.rs:18-33` | signs `self.compute_hash()`, stores into `self.signature` |
| `repo.sign_state(state_id, signer)` | `crates/repo/src/repository_signing.rs:25` | load → `state.sign(signer)` → `put_state` |
| `repo.verify_state_signature(state_id)` | `repository_signing.rs:75` | returns `SignatureStatus::{Valid,Invalid,Unsigned}` |

**Hash hygiene is already correct.** `update_hash` (`state_core.rs:561`) hashes
tree, parents, principal, agent, intent, confidence, created_at, verification,
provenance, context, status — but **not** `self.signature` (verified by reading
`:561-630`; the field is absent from the hasher). So a signature over
`compute_hash()` has no circular dependency: the hash is stable whether or not the
signature is later attached. This is the property that makes auto-signing safe to
bolt on without changing state identity.

---

## 4. Current signing today: `review sign`

`review sign` is **not** author self-signing — it's a *review attestation* RPC,
and it is entirely manual.

- Args (`crates/cli/src/cli/cli_args/commands_review.rs:28-54`): required
  `--kind` (`read` / `agent-preview` / `agent-co-review`,
  `commands_review.rs:56-61`), `--public-key` (hex, required, `:43-45`),
  `--signature` (hex, required, `:46-48`), `--signed-at-unix` (required, `:49-53`),
  plus optional `--justification`, `--symbols`, `--algorithm` (default ed25519).
- Handler (`crates/cli/src/cli/commands/review.rs:283-308`): the CLI does **no
  signing**. It accepts client-computed `public_key`/`signature` bytes, packs them
  into a `SignStateRequest` (`review.rs:283-301`), and forwards to
  `sign_state` on the **state-review gRPC service** (`review.rs:294`). The
  signature is stored server-side and surfaced as an accumulating list
  (`list_signatures`, used by `review next` at `review.rs:~340`).
- What's signed: a `ReviewScope` (whole-change or symbol-level,
  `review.rs:255-270`) over a canonical payload that includes the `signed_at`
  timestamp — the server re-verifies the sig over that exact timestamp within a
  skew window (`commands_review.rs:49-53`). It is **detached** review metadata; it
  does **not** populate `State.signature`.

So today there are *two separate, both-incomplete* notions of signing:
1. **`State.signature`** — the author/identity signature slot. Fully implemented
   in crypto+repo, **never written on capture**. This is the gap #480 fixes.
2. **`review sign`** — multi-party review attestations, stored separately, fully
   manual (you bring your own bytes).

These are complementary, not competing. #480 is about (1).

---

## 5. The marketing gap this closes

Per the 2026-06-03 tapestry audit, the marketing leans on a *"signed merge /
ed25519 attestation"* motif: a `heddle merge --sign` flag and an automatic
ed25519 attestation. Neither ships — `merge --sign` doesn't exist, and `review
sign` only records bytes you computed yourself with four required flags. The
overclaim is the *automatic* and the *merge* parts.

**What becomes literally true once #480 ships (shape (a)):** *every state Heddle
captures — including merges — carries an ed25519 signature over its content hash,
produced automatically from your identity key, verifiable offline.* That is a
defensible, specific claim and the concrete differentiator vs Mesa (zero signing).
See §10 for the exact before/after copy.

---

## 6. Hook points for automatic signing

Two precise insertion points, both in `crates/repo/src/repository_snapshot.rs`,
each immediately before the existing `put_state`:

### 6a. Capture / commit (snapshot)
`repository_snapshot.rs:203` builds `State::new_snapshot(...)`, enriches it
(intent `:205`, confidence `:209`, inherited context `:213`, risk signals
`:228`), then persists at `:241` `self.repo.store.put_state(&state)`. **Auto-sign
slots in between the last enrichment and `put_state`:**

```text
… state = state.with_risk_signals(hash);            // existing, :233
if let Some(signer) = self.repo.identity_signer()? { // NEW
    state.sign(signer.as_ref())?;                    // crypto::StateSigningExt
}
self.repo.store.put_state(&state)?;                  // existing, :241
```

Ordering is critical: signing must be the **last** mutation before `put_state`,
because `state.sign()` signs `compute_hash()` over the then-current fields. Any
field set after signing would invalidate the signature. (Risk-signal computation
at `:228` sets `with_risk_signals`, which *is* in the hash — so it must run
before signing, as shown.)

### 6b. Merge
`repository_snapshot.rs:655` builds `State::new_merge(...)`, enriches with
provenance (`:683`) and unioned context (`:690`), then persists at `:693`. Same
pattern: sign after the last enrichment, before `put_state`. **This is the line
that makes `merge --sign`'s marketing claim true** — merges become signed by
default, no flag.

### 6c. The `identity_signer()` resolver (the one new piece of plumbing)
A small repo-level helper that returns `Option<Box<dyn Signer>>`:

1. **Preferred — reuse the device key.** Resolve the active server credential
   (`credentials::resolve_credential_for_server`, already used by
   `grpc_hosted/mod.rs` rotation at `:151`), read `private_key_pem`
   (`credentials.rs:49`), and build `Ed25519Signer::from_pem(pem)` — the exact
   call `grpc_hosted/mod.rs:92` and `:176` already make. **No new key.** The
   pubkey is the server-registered device key, so `StateSignature.public_key`
   ties directly to a known identity.
2. **Fallback — config-pointed key.** Honor an explicit signing-key path from
   user config (mirroring `remote.auth_proof_key_pem_path` at
   `user_config.rs:135`), loaded via `crypto::load_signer`
   (`crypto/src/lib.rs:38`), for users who want signing without hosted auth.
3. **Degrade — unsigned.** If neither exists (pure local git-substrate repo,
   never logged in), return `None`. Capture proceeds, `state.signature` stays
   `None`, behavior is byte-identical to today. **Auto-signing must never make
   `heddle capture` fail.**

`load_signer` already enforces `0600`-or-tighter key perms
(`crypto/src/lib.rs:59-71 reject_group_or_world_readable_key`); resolving from
`credentials.toml` inherits the same `0600` guarantee (§2b). The signer should be
resolved **once per process/session and cached**, not re-read per capture, to
avoid hammering the credential file on bulk operations.

---

## 7. Signature shape: (a) direct ed25519 vs (b) biscuit attestation

### Shape (a) — direct ed25519 detached signature *(recommended for v1)*
Sign `state.compute_hash()` with the device ed25519 key; store
`StateSignature { algorithm: "ed25519", public_key: hex, signature: hex }` in
`state.signature`. This is *literally what `StateSigningExt::sign` already does*
(`state_signing.rs:19-22`).

- **+ Offline verifiable.** `verify_state_signature_bytes`
  (`state_signature.rs:36`) needs only the state and the embedded pubkey — no
  server, no network. Works in air-gapped review.
- **+ Tiny payload.** 32-byte pubkey + 64-byte sig, hex-encoded (~192 bytes) in
  the state. Negligible.
- **+ Zero new code in the crypto/repo layer** — only the hook + resolver (§6).
- **+ Survives merges trivially** — it's a field on each state (§8).
- **− Identity is "just a pubkey."** Verifying *who* `public_key` belongs to
  requires an out-of-band pubkey→identity map (the server's device registry, or a
  published key). Offline you can prove *"the same key signed all these states"*
  but not *"that key is alice@"* without the registry.
- **− No capability semantics.** It asserts authorship, not authority — it doesn't
  say "this key was allowed to author here."

### Shape (b) — biscuit attestation token
Emit a biscuit that attests `author(P)`, `state(hash)`, `time(T)` and store the
token bytes alongside the state.

- **+ Capability-integrated.** Authorship rides the same token model as
  attenuation (`device_flow.rs`); a verifier already trusting the biscuit root key
  gets identity *and* authority in one object.
- **+ Identity is self-describing** — the token carries the principal facts, not a
  bare pubkey.
- **− Not locally mintable.** Per §2a the client can't mint a biscuit offline —
  it would need a `mint_attestation` RPC round-trip on every capture. That breaks
  offline capture and couples the core capture path to the hosted server. A
  dealbreaker for a tool whose substrate is local git.
- **− Verification needs the root public key.** Offline verify requires
  distributing the biscuit root pubkey; heavier than a self-contained ed25519 sig.
- **− Larger payload**, more moving parts, and it re-introduces the network
  dependency §6c is explicitly designed to avoid.

### Decision
**Ship (a) now.** It reuses the device key, is offline-verifiable, is byte-cheap,
and the entire crypto/repo substrate already exists. **Keep (b) as a documented
future layer** (`heddle attest`, opt-in, for users who want capability-bound
authorship tokens) — *additive* to the ed25519 signature, never a replacement. The
two compose: the ed25519 sig proves integrity+authorship offline; an optional
biscuit attestation adds capability context when a server is in the loop.

---

## 8. Verification & merge survival

### Verification
`repo.verify_state_signature(state_id)` (`repository_signing.rs:75`) already
implements the full check: load state, recompute `compute_hash()` (`:85`),
`verify_state_signature_bytes(sig, &hash)` (`:86`) → `Valid / Invalid / Unsigned`
(`:87-99`). Because the signature is excluded from the hash (§3), the recomputed
hash matches what was signed. **No new verification code is needed** beyond
surfacing it (e.g. a `--verify` on inspect/log, or a `heddle verify <state>`
verb).

**Pubkey → identity.** The embedded `public_key` is resolved to an identity via
the server device registry (the pubkey registered at `auth_cmd.rs:84`). Offline,
the verifier can pin/trust a published pubkey. This is the one piece that isn't
self-contained in shape (a); call it out for sign-off (§11).

### Merge survival
This is where Heddle's attribution-through-merge property matters. A state's
signature covers **that state's** hash, which includes its parents
(`update_hash` hashes `self.parents`, `state_core.rs:572-575`). Consequences:

- Each historical state keeps its own author signature forever — merges don't
  rewrite ancestors, so **a parent's signature stays valid after a merge** (its
  hash didn't change). Walking history, every state self-attests its author.
- The **merge state itself** gets its *own* fresh signature from the merging
  user's key at the §6b hook — i.e. "alice signs that she performed this merge,
  combining parents X and Y." That's the correct semantic: the merge author
  vouches for the merge; the original authors' signatures on the parents remain
  intact and independently verifiable.
- No signature needs to "travel through" a merge — each state is signed in place.
  This sidesteps the hard problem (re-signing combined content) entirely: identity
  is per-state, accumulated along the DAG, exactly like attribution.

---

## 9. Migration: what happens to `review sign`

`review sign` and auto-signing solve **different** problems; keep them separate.

| | Auto state-signature (#480) | `review sign` (today) |
|---|---|---|
| Who signs | the **author**, automatically, on capture | a **reviewer** (often a third party), explicitly |
| What | `state.compute_hash()` | a `ReviewScope` over a payload + timestamp |
| Where stored | `state.signature` (one, on the state) | accumulating list via state-review service |
| Trigger | every capture/commit/merge | deliberate `review sign` invocation |
| Multiplicity | one author sig per state | many review sigs per state |

**Recommendation:**
1. **Keep `review sign` as the explicit multi-party review-attestation path** — it
   is the only way a *non-author* signs someone else's state, and the `review
   next`/`list_signatures` flow depends on its accumulating model. Do **not**
   fold it into auto-signing.
2. **Reduce its friction to match #473's verb consolidation.** The four required
   hand-computed flags (`--public-key/--signature/--signed-at-unix` + `--kind`)
   are the real UX wart. With a resolvable `identity_signer()` (§6c), `review
   sign` should compute the signature **itself** from the local key by default,
   leaving only `--kind` (+ optional `--symbols/--justification`). The
   bring-your-own-bytes flags become an **advanced/escape-hatch** path
   (e.g. `--signature/--public-key` for HSM/offline signers), not the default.
   This is a follow-up, not part of this spike, but it's the natural convergence
   point: *both* surfaces sign with the same resolved identity key.
3. **No deprecation.** Nothing about `review sign`'s contract is wrong; it's
   under-automated. Auto-signing fills the author-signature gap; the review
   surface keeps doing reviews.

---

## 10. Marketing alignment (for tapestry copy correction)

| Claim today (overclaim) | After #480 ships (true) |
|---|---|
| "`heddle merge --sign` — signed merges" | "Merges are signed automatically — every merge state carries an ed25519 signature from the merger's identity key. No flag required." |
| "ed25519 attestation on every change" | "Every captured state is automatically ed25519-signed over its content hash with your identity key, and verifiable offline." |
| (implicit) "signing is built in" | "Signing reuses your existing Heddle identity key — no key management, no extra step. Mesa has none of this." |

Correct, don't merely soften: drop the `--sign` flag framing (it's *automatic*,
flagless) and the standalone "attestation" word for v1 (that's the future biscuit
layer, §7b). The honest, strong claim is **automatic, offline-verifiable, ed25519
author signatures on every state including merges, keyed to your Heddle identity.**

---

## 11. Security & trust analysis

**What is signed.** `state.compute_hash()` — a blake3 content hash over tree,
parents, principal, agent, intent, confidence, created_at, verification,
provenance, context, status (`state_core.rs:561-630`). The signature therefore
binds *authorship + the full semantic state*, not just file bytes. The signature
field itself is excluded, so no circularity.

**Key exposure surface.** Reusing the device key means **one ed25519 private key
now serves two roles**: (1) hosted-auth proof / biscuit-mint gating, (2)
state-signing. Analysis:

- *Storage exposure is unchanged.* The key already lives in
  `~/.heddle/credentials.toml` at `0600` (§2b); auto-signing reads it more often
  but adds no new at-rest copy. Cache the loaded signer in-process (§6c) to avoid
  repeated disk reads.
- *Key-reuse-across-purposes risk.* The two uses sign **disjoint, unambiguous
  payloads**: auth proofs sign `"{bearer}|{ts}"` / `"{ts}\n{pubkey_id}\n"`
  (`grpc_hosted/mod.rs:107,191`); state-signing signs a 32-byte blake3 content
  hash. There is no payload-confusion path (an attacker can't get a state
  signature to validate as an auth proof or vice-versa — different lengths,
  different structures, no shared prefix). This is acceptable, but **recommend a
  domain-separation tag** (e.g. sign `"heddle-state-v1" || hash` instead of bare
  `hash`) to make non-collision explicit and future-proof against new signing
  uses. *Open question for sign-off (§12).* Note: adding a tag is a one-line
  change to `state_signature_from_signer` but it changes the signed bytes, so it
  must be decided **before** v1 ships — retrofitting splits the verify path.
- *Compromise blast radius.* If the device key leaks, the attacker can both
  impersonate the user to the server (already true today) **and** forge author
  signatures on states. Mitigation: signatures are only as trustworthy as the
  device-key custody, and the server-side device registry can **revoke** a
  compromised pubkey — verifiers checking pubkey→identity against the registry
  then reject forgeries minted after revocation. Pure-offline verifiers without
  registry access can't see revocation; that's the inherent limit of offline
  verification and should be documented, not hidden.

**Replay / freshness.** A state signature is over the content hash, which includes
`created_at` and parents — so a signature can't be lifted onto a *different*
state. It *can* be re-presented for the *same* state (that's fine — it's a
statement about that immutable state, not a session token). Unlike `review sign`
(which binds a timestamp + skew window for liveness, `commands_review.rs:49-53`),
author signatures don't need anti-replay: they assert a permanent fact about an
immutable object.

**Failure mode discipline.** Signing must be **best-effort, non-fatal** (§6c
step 3). A missing key, an unreadable credential, or a signer error must log a
warning and proceed unsigned — never fail `capture`. (Mirror the existing
risk-signal `tracing::warn!(…); continuing` pattern at
`repository_snapshot.rs:236`.) Auto-signing is an enrichment, not a gate.

**No new keys.** The design introduces zero new key material in the common path:
it reuses the device ed25519 key (§2b). The only "new key" case is the config-
pointed fallback (§6c step 2), which is an explicit user-supplied key, not
something Heddle generates silently.

---

## 12. Open questions for maintainer sign-off

1. **Domain-separation tag (§11).** Sign `"heddle-state-v1" || hash` rather than
   bare `hash`? Cleaner cryptographic hygiene given key reuse, but must be decided
   *before* v1 — it changes the signed bytes and the verify path. Recommend: yes.
2. **Pure-local identity (§6c step 3).** For a git-substrate-only repo with no
   hosted login, do we (a) leave states unsigned, (b) auto-generate a local-only
   identity key on first capture, or (c) require explicit `heddle identity init`?
   Recommend (a) for v1 (no surprise key generation), revisit (b)/(c) later.
3. **Pubkey→identity resolution offline (§8).** What's the canonical offline trust
   anchor — a `heddle key publish` registry, pinned keys, or "online-only identity
   resolution"? Affects how strong the offline claim can be in marketing.
4. **`review sign` auto-compute follow-up (§9).** Confirm the convergence: should
   `review sign` default to signing with the resolved identity key (only `--kind`
   required), keeping `--signature/--public-key` as an advanced escape hatch?
   File as a separate issue under #473's verb-consolidation umbrella?
5. **Biscuit attestation layer (§7b).** Park shape (b) as a future opt-in
   `heddle attest`, or drop it entirely? Recommend: park, document as additive.
6. **Verify surface.** Where does verification get exposed — `heddle verify
   <state>`, a `--verify` flag on inspect/log, or fsck integration (the
   `fsck_checks/state.rs` path already imports `StateSigningExt`)? Out of scope
   for #480 but needed to make the signatures *useful*.

---

## 13. Appendix — implementation surface (for the eventual impl issue)

Not part of this spike; sketch only, to scope the follow-up:

- **New:** `Repository::identity_signer(&self) -> Result<Option<Box<dyn Signer>>>`
  (resolver, §6c) — the one genuinely new piece. ~30 lines, reuses
  `credentials::resolve_credential_for_server` + `Ed25519Signer::from_pem`.
- **Edit:** `repository_snapshot.rs` — two ~3-line insertions before `put_state`
  at `:241` and `:693` (§6a/§6b).
- **Maybe:** domain-separation tag in `state_signature.rs:23
  state_signature_from_signer` (§12 Q1).
- **Reuse as-is:** `StateSigningExt::sign`, `verify_state_signature`,
  `StateSignature`, the whole `crypto` crate.
- **Tests:** capture-then-verify roundtrip (extends
  `repository_signing.rs:143 test_sign_state`); unsigned-degrade path; merge-state
  carries its own sig; parent sigs survive a merge.

The smallness of this surface is the headline: Heddle is *one resolver + two hook
lines* away from automatic, offline-verifiable, key-reuse signing on every state.
</content>
</invoke>
