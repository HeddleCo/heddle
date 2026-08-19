# Self-sovereign auth: minting and attenuating your own Biscuit

> A *self-sovereign* Heddle client is one that holds its own root
> keypair and mints its own Biscuit tokens locally, without a server
> round-trip. It can also attenuate those tokens — appending narrower
> checks before handing them to a sub-agent — entirely offline. The
> verifier on the other side validates the chain against whatever
> trust anchor it has been configured with.

This walkthrough covers the local-only flow. For the hosted flow
(where the client mints the root and Weft registers the public key,
then the CLI attenuates offline for sub-agents), see
[`.agents/agent-attenuation.md`](../.agents/agent-attenuation.md).

## When this applies

The self-sovereign path is the right shape when:

- You're running Heddle **without** the hosted control plane and need
  an auth token shape compatible with the same verifier rules the
  hosted server uses.
- You're testing or developing against the attenuation surface and
  want a parent token without standing up a server.
- You're integrating Heddle into a system that already has its own
  identity layer (e.g. a workspace tool, a daemon) and that system
  is the trust anchor — not Heddle's hosted server.

If you already have a hosted Biscuit (client-minted at signup, claim,
passkey finish, device login, or anon), you should attenuate *that*
token instead. See `.agents/agent-attenuation.md`. Weft never remints.

## Concept

A Biscuit is a chain of cryptographic blocks. The first block — the
*authority* block — is signed by a root keypair. Every later block
is appended by the current holder. The verifier replays every
block's checks on every request, so a later block can only narrow
authority, never widen it.

```
┌─────────────────┐
│ authority block │  signed by the client's own keypair (self-sovereign)
├─────────────────┤
│   block 1       │  attenuation (e.g. "expires in 4h")
├─────────────────┤
│   block 2       │  further attenuation (e.g. "read-only on repo X")
└─────────────────┘
```

Self-sovereign minting just means the authority block's signing key
is owned by the client process itself, rather than being held by a
hosted server. The attenuation machinery is identical either way.

## Where the code lives

Self-sovereign minting uses
[`biscuit-auth`](https://docs.rs/biscuit-auth) directly. Heddle does not expose a
general-purpose capability-token library. Its hosted attenuation policy belongs
to the application and lives in the CLI's private
[`hosted_runtime/device_flow.rs`](../crates/cli/src/hosted_runtime/device_flow.rs)
module. Use `heddle auth derive-agent` for that policy-controlled flow.

[`heddle-crypto`](../crates/crypto/) is a *different* crypto surface:
it covers the signers (`Ed25519Signer`, `P256Signer`) that Heddle uses
to sign repository **states**, not Biscuit authority blocks. The two
crates are intentionally separate — state signing and capability tokens
are independent concerns.

## Minting a local authority token

The example below creates a self-sovereign root token with the upstream
`biscuit-auth` API. Any service accepting the result must define and test its
own attenuation policy and request facts; Heddle's hosted policy is not a
public integration surface.

`Cargo.toml`:

```toml
[dependencies]
anyhow = "1"
biscuit-auth = "6"
chrono = "0.4"
```

`src/main.rs`:

```rust
use anyhow::Result;
use biscuit_auth::{Biscuit, KeyPair};
use chrono::{Duration, Utc};

fn main() -> Result<()> {
    // 1. Mint: generate the client's own root keypair and build an
    //    authority block. This is the "self-sovereign" step — no
    //    server round-trip.
    let root = KeyPair::new();
    let parent_expiry = Utc::now() + Duration::hours(8);
    let parent_b64 = Biscuit::builder()
        .fact(r#"user("alice")"#)?
        .fact(r#"session("local-sess-1")"#)?
        .fact(format!("expires_at({})", parent_expiry.to_rfc3339()).as_str())?
        .check(format!("check if time($now), $now < {}", parent_expiry.to_rfc3339()).as_str())?
        .build(&root)?
        .to_base64()?;

    println!("parent  {} bytes", parent_b64.len());
    println!("{} authority block", block_count(&parent_b64)?);
    Ok(())
}

fn block_count(token_b64: &str) -> Result<usize> {
    let parsed = biscuit_auth::UnverifiedBiscuit::from_base64(token_b64.as_bytes())?;
    Ok(parsed.block_count())
}
```

Running this prints something like:

```
parent  N bytes
1 authority block
```

Use the CLI flow below when the token is intended for hosted Heddle:

```bash
heddle auth derive-agent \
  --server grpc.heddle.sh \
  --agent-id agent-doc-review \
  --ttl 7200 \
  --scope repo:org/acme/heddle \
  --allow GetState
```

## What gets emitted in the attenuation block

Each restriction translates to a Biscuit Datalog clause that the
verifier evaluates with the per-request facts (`time`, `operation`,
`resource`) injected by the server. The shape is documented in
[`.agents/agent-attenuation.md`](../.agents/agent-attenuation.md)
and the CLI-owned construction lives in
[`hosted_runtime/device_flow.rs`](../crates/cli/src/hosted_runtime/device_flow.rs).
In short:

| Field | Datalog | Default when fact missing |
|---|---|---|
| `expires_at` | `check if time($now), $now < <ts>` | Verifier always injects `time`, so always evaluated. |
| `allowed_operations: Some([...])` | `check if operation($op), $op == "X" \|\| ...` | Reject (fail-closed). |
| `allowed_resources: Some([...])` | `check if resource($k, $p), ($k == "..." && ($p == "..." \|\| $p.starts_with("...")))` | Reject (fail-closed). |

The resource matcher accepts an exact path or any descendant: an
entry of `("repo", "org/acme")` covers `repo:org/acme`,
`repo:org/acme/heddle`, and `repo:org/acme/docs`, but not
`repo:org/other`.

## What the verifier needs

A self-sovereign Biscuit only validates against a verifier
configured with the *matching* root public key. That contract is
out of band: the system that consumes these tokens (your own
service, a test harness, a non-hosted Heddle deployment) must be
configured with `root.public()` from step 1.

Weft verifies a hosted Biscuit against the public key it registered
for that account. The client is the authority-block signer. Existing
accounts attenuate offline; Weft never remints. See
`.agents/agent-attenuation.md`.

## What you can't do

The attenuation rules are the same as in the hosted flow:

- **Widen authority.** A child block can only add checks. There is
  no way to add rights the parent didn't have.
- **Remove a parent's checks.** Every parent block's check still
  runs on every request.
- **Hide a block.** Every block is visible to the verifier.
- **Re-sign the chain.** The verifier's trust anchor is the root
  public key. Re-signing with a different key produces a chain that
  no verifier will accept.

## Token size

Each attenuation block adds ~100–200 bytes (consistent with the
hosted flow figures in `.agents/agent-attenuation.md`). A 5-deep
chain is still under 2 KB.

## See also

- [`.agents/agent-attenuation.md`](../.agents/agent-attenuation.md)
  — hosted-flow cookbook (read-only inspector, time-bounded agent,
  multi-repo writer, sub-sub-agent chain).
- [`crates/cli/src/hosted_runtime/device_flow.rs`](../crates/cli/src/hosted_runtime/device_flow.rs)
  — the CLI-owned attenuation policy and its unit tests.
- [biscuit-auth documentation](https://docs.rs/biscuit-auth) —
  upstream details on the Biscuit format, Datalog semantics, and
  `KeyPair`/`Biscuit::builder` APIs used at mint time.
