# Agent Attenuation

> Spawn an agent without a server round trip. Hand it a Biscuit that
> can only ever be a strict subset of yours.

## Mental model

A Biscuit is a chain of cryptographic blocks. The first (authority)
block is signed by Heddle; every subsequent block is appended by
whoever currently holds the token, no server contact required. The
verifier runs every block's checks on every request — so a child
block can only narrow authority, never widen it.

When you spawn a sub-agent, you don't ask the server for a new
token. You append a new block to your own. The agent gets the
attenuated bytes; the server validates the chain on the agent's
first call.

```
                  ┌─────────────────┐
  server ────┤ authority block │  signed by Heddle
                  ├─────────────────┤
  parent agent ───┤   block 1       │  parent's attenuation (e.g. "expires in 4h")
                  ├─────────────────┤
  sub-agent  ─────┤   block 2       │  child's narrower attenuation
                  └─────────────────┘
```

Revocation works the same way. The server's revocation cache is
keyed on the `session()` fact in the authority block; a sub-agent
inherits that fact, so revoking the parent's session id rejects
every descendant on the next request.

## API: `cli::auth`

The CLI surface is small: one struct, one main function, two
convenience constructors.

```rust
use cli::auth::{
    AgentAttenuation, attenuate_for_agent, time_bounded, read_only_repo_agent,
};

// Simplest: time-bounded sub-agent that inherits everything else
// from the parent.
let attenuated = time_bounded(
    &parent_token_b64,
    "agent-doc-pr-42",
    chrono::Utc::now() + chrono::Duration::hours(4),
)?;

// Read-only sub-agent on a single repo.
let attenuated = read_only_repo_agent(
    &parent_token_b64,
    "agent-explorer",
    "org/acme/heddle",
    /* duration_hours = */ 2,
)?;

// Custom: build the AgentAttenuation field-by-field.
let attenuated = attenuate_for_agent(
    &parent_token_b64,
    AgentAttenuation {
        agent_id: "agent-custom".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(8),
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
```

## Restriction semantics

Every restriction emits a Biscuit `check if ...` clause. The
verifier runs each check against the world (which contains the
parent's facts plus whatever the verifier injects per-request:
`time(now)`, `operation($name)`, `resource($kind, $path)` when
applicable). A check that finds no binding is a hard reject — that's
the secure default.

| Restriction | Datalog form | Default behaviour when no fact present |
|---|---|---|
| `expires_at` | `check if time($now), $now < <ts>` | Verifier always injects `time`, so always evaluated |
| `allowed_operations: Some([...])` | `check if operation($op), $op == "X" \|\| ...` | Reject (no operation fact → check fails closed) |
| `allowed_resources: Some([...])` | `check if resource($k, $p), (...path matches...)` | Reject (no resource fact → check fails closed) |

The path-prefix matcher accepts an exact match or any nested path:
an entry of `("repo", "org/acme")` covers `repo:org/acme`,
`repo:org/acme/heddle`, `repo:org/acme/docs`, etc. Sibling namespaces
(`repo:org/other`) are not covered.

## Cookbook

### 1. Read-only inspector for a single repo

```rust
let attenuated = read_only_repo_agent(
    &parent,
    "agent-pr-review",
    "org/acme/heddle",
    4,  // hours
)?;
// Hand `attenuated` to the agent.
```

The `read_only_repo_agent` constructor allowlists the read RPCs
(`GetState`, `GetTree`, `GetBlob`, `GetCompare`, `GetDiff`,
`ListRefs`, `ListStates`, `ListContext`) and pins the resource list
to the repo path. Any write RPC fails with `PermissionDenied`.

### 2. Time-bounded background agent

```rust
let attenuated = time_bounded(
    &parent,
    "agent-overnight-build",
    chrono::Utc::now() + chrono::Duration::hours(12),
)?;
```

No operation or resource narrowing — the agent can do whatever the
parent could, but only for the next 12 hours.

### 3. Multi-repo writer

```rust
use cli::auth::{AgentAttenuation, attenuate_for_agent};

let attenuated = attenuate_for_agent(
    &parent,
    AgentAttenuation {
        agent_id: "agent-cross-repo".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(2),
        // No operation allowlist → can do anything the parent can.
        allowed_operations: None,
        // But only on these two repos.
        allowed_resources: Some(vec![
            ("repo".to_string(), "org/acme/heddle".to_string()),
            ("repo".to_string(), "org/acme/docs".to_string()),
        ]),
    },
)?;
```

### 4. Sub-sub-agent (further attenuation)

```rust
// Parent attenuates for the agent.
let agent_token = read_only_repo_agent(&parent, "agent-1", "org/acme/heddle", 8)?;

// Inside the agent process, attenuate further for a sub-agent.
let sub_agent_token = attenuate_for_agent(
    &agent_token,
    AgentAttenuation {
        agent_id: "agent-1.1".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        // Narrower than the parent: only GetState.
        allowed_operations: Some(vec!["GetState".to_string()]),
        // Narrower than the parent: a single repo.
        allowed_resources: Some(vec![
            ("repo".to_string(), "org/acme/heddle".to_string()),
        ]),
    },
)?;
```

The verifier runs all three blocks' checks on every request from
the sub-sub-agent. Effective authority is the *intersection* of
every layer.

## Revocation

There is no separate "revoke this agent" RPC. Two paths:

1. **Time bound.** Set a tight `expires_at` and let the agent's
   token expire naturally. Cleanest path for short-lived agents.
2. **Cascade revoke.** Call `RevokeSession` on the parent's
   `session_id`. The server pushes the rev_id into the
   blocklist and broadcasts via Postgres LISTEN/NOTIFY. Every
   descendant of the parent is rejected on its next call (within
   ~100ms across multi-instance deployments). Note: this also
   kills the parent's own session — usually what you want.

For a multi-week sub-agent that needs its *own* revocation surface
without taking the parent down, model it as a service account
(its own keypair, its own anchor row in `service_accounts`). The
boundary is:

- **Ephemeral** (hours to days, lifetime bounded by parent task) →
  client-side attenuation, no server registration.
- **Persistent** (weeks to months, organizational principal) →
  service account with keypair, autonomous renewal via
  `MintBiscuit + KeypairProof`.

## What you can't do

- **Widen authority.** A child block can only emit *additional*
  checks. There is no way to add rights the parent didn't have.
- **Remove a parent's checks.** If the parent restricts itself to
  read-only, a child that "needs write" is simply impossible — the
  child must come from a different parent or directly from the
  server's mint.
- **Hide an attenuation block.** Every block is visible to the
  verifier; an agent can't strip checks before presenting the
  token.
- **Re-sign the chain.** The server's public key is the trust
  anchor. The server rejects any chain whose authority block
  doesn't trace back to it.

## Where the server enforces this

- Authority + attenuation blocks: `crates/server/src/biscuit.rs::attenuate`
- Verifier rule pack: `crates/server/src/biscuit/rules.biscuit`
- Per-request facts (`time`, `operation`, `resource`) injected in:
  `crates/server/src/biscuit.rs::authorize`
- Revocation cache (cascade-revoke via parent rev_id):
  `crates/server/src/biscuit/cache.rs`
- Integration tests for chain-three-deep, expiry, and cascade
  revoke: `tests/grpc_hosted_integration.rs::agent_attenuation_*`

## Token size

Each attenuation block adds ~100–200 bytes. A 5-deep chain is still
under 2 KB — comfortable inside an HTTP `Authorization` header or a
WebSocket upgrade URL. The server has no per-token size limit
beyond the Biscuit library's defaults.
