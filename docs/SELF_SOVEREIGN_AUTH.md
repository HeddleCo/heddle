# Self-sovereign auth: minting and attenuating your own Biscuit

> A *self-sovereign* Heddle client is one that holds its own root
> keypair and mints its own Biscuit tokens locally, without a server
> round-trip. It can also attenuate those tokens — appending narrower
> checks before handing them to a sub-agent — entirely offline. The
> verifier on the other side validates the chain against whatever
> trust anchor it has been configured with.

This walkthrough covers the local-only flow. For the hosted flow
(where the server is the trust anchor and the CLI receives a Biscuit
via `MintBiscuit` then attenuates it for sub-agents), see
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

If you have a hosted Biscuit (issued via `MintBiscuit` or a device
flow), you should attenuate *that* token instead. See `.agents/agent-attenuation.md`.

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

Two surfaces are involved:

| Surface | Crate | What it does |
|---|---|---|
| Mint | [`biscuit-auth`](https://docs.rs/biscuit-auth) (workspace dep, pinned to `"6"` in [`Cargo.toml`](../Cargo.toml)) | `KeyPair::new()` to generate a root key; `Biscuit::builder()` to build the authority block. |
| Attenuate | [`heddle_client::device_flow`](../crates/client/src/device_flow.rs) (re-exported as `heddle_client::auth`) | `attenuate_for_agent`, `time_bounded`, `read_only_repo_agent`. |

[`heddle-crypto`](../crates/crypto/) is a *different* crypto surface:
it covers the signers (`Ed25519Signer`, `P256Signer`) that Heddle uses
to sign repository **states**, not Biscuit authority blocks. The two
crates are intentionally separate — state signing and capability tokens
are independent concerns.

## End-to-end example

The example below mints a self-sovereign root token, attenuates it
for a sub-agent, and shows the result parses back as a multi-block
Biscuit. It's grounded in the same APIs the unit tests in
[`crates/client/src/device_flow.rs`](../crates/client/src/device_flow.rs)
exercise.

`Cargo.toml`:

```toml
[dependencies]
anyhow = "1"
biscuit-auth = "6"
chrono = "0.4"
heddle-client = "0.2"
```

`src/main.rs`:

```rust
use anyhow::Result;
use biscuit_auth::{Biscuit, KeyPair};
use chrono::{Duration, Utc};
use heddle_client::auth::{AgentAttenuation, attenuate_for_agent, read_only_repo_agent};

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

    // 2. Attenuate (simple): hand a sub-agent a read-only token
    //    restricted to one repo for 2 hours.
    let agent_b64 = read_only_repo_agent(
        &parent_b64,
        "agent-doc-review",
        "org/acme/heddle",
        /* duration_hours = */ 2,
    )?;

    // 3. Attenuate (custom): build the restriction set field-by-field
    //    for a sub-agent that can call two specific RPCs on two repos.
    let custom_b64 = attenuate_for_agent(
        &parent_b64,
        AgentAttenuation {
            agent_id: "agent-cross-repo".to_string(),
            expires_at: Utc::now() + Duration::hours(1),
            allowed_operations: Some(vec![
                "GetState".to_string(),
                "GetCompare".to_string(),
            ]),
            allowed_resources: Some(vec![
                ("repo".to_string(), "org/acme/heddle".to_string()),
                ("repo".to_string(), "org/acme/docs".to_string()),
            ]),
        },
    )?;

    println!("parent  {} bytes", parent_b64.len());
    println!("agent   {} bytes ({} blocks)", agent_b64.len(), block_count(&agent_b64)?);
    println!("custom  {} bytes ({} blocks)", custom_b64.len(), block_count(&custom_b64)?);
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
agent   M bytes (2 blocks)
custom  K bytes (2 blocks)
```

The attenuated tokens have one more block than the parent: the
authority block (minted at step 1) plus the attenuation block
(appended at step 2 or 3). Further attenuation by the sub-agent
appends additional blocks.

## What gets emitted in the attenuation block

Each restriction translates to a Biscuit Datalog clause that the
verifier evaluates with the per-request facts (`time`, `operation`,
`resource`) injected by the server. The shape is documented in
[`.agents/agent-attenuation.md`](../.agents/agent-attenuation.md)
and the construction lives in
[`build_attenuation_block`](../crates/client/src/device_flow.rs)
in `device_flow.rs`. In short:

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

The hosted Heddle server is configured with its own root key and
will reject Biscuits minted by a different root. If you need tokens
that the hosted server will accept, use `MintBiscuit` or the device
flow — see `.agents/agent-attenuation.md`.

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
- [`crates/client/src/device_flow.rs`](../crates/client/src/device_flow.rs)
  — the attenuation API surface and its unit tests.
- [biscuit-auth documentation](https://docs.rs/biscuit-auth) —
  upstream details on the Biscuit format, Datalog semantics, and
  `KeyPair`/`Biscuit::builder` APIs used at mint time.
