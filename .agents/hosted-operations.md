# Hosted Operations

## Current Hosted Model

`hosted` owns the hosted runtime. `heddle` is the client-facing tool that connects to it.

The hosted model supports:

- hierarchical namespaces (`user` and `org`)
- hosted repositories addressed by namespace path
- hosted grants with inherited namespace roles
- admin access over both the Heddle wire protocol and the minimal HTTP admin API

Role order is:

- `reader`
- `developer`
- `maintainer`
- `admin`
- `owner`

## Authorization Rules

Hosted admin actions require both:

1. `Permission::Admin` on the token
2. token scope that covers the target namespace or repository

If the caller also has hosted grants, effective hosted role must be sufficient for the action.

## Control Plane Guidance

- Treat Postgres as the source of truth for hosted control-plane metadata when `hosted` config enables it.
- Keep shared admission and coordination state out of process memory for horizontally scaled deployments.
- Use Postgres-backed ephemeral coordination for distributed locks, TTL KV, and bounded semaphore slots.
- Keep repository object storage externalized (S3-compatible storage in hosted mode).

## Operational Endpoints

The HTTP surface has two distinct auth tiers:

**Admin endpoints** (`/api/namespaces`, `/api/repositories`, `/api/grants`) — require a server-issued admin token or equivalent `hosted` configuration with `Permission::Admin`.

**Content endpoints** (`/api/content/*`) — accept either the admin static token **or** a Biscuit issued by `POST /api/auth/login`. Content routes are intercepted before the admin auth gate in `handle_connection`.

**Health / metrics:**
- `GET /health`
- `GET /ready`
- `GET /metrics`

**Content API** (requires auth, scoped to repo visibility):
- `POST /api/auth/login` — issue a Biscuit for a subject (admin only); body `{"subject":"..."}`; returns `{"token":"...","subject":"..."}`
- `GET /api/content/refs?repo=<full_path>` — HEAD hash, threads list, markers list
- `GET /api/content/log?repo=<full_path>[&ref=<ref>][&limit=N]` — state walk from ref (default HEAD, limit 20)
- `GET /api/content/tree?repo=<full_path>[&ref=<ref>][&path=<subpath>]` — tree entries at ref/path
- `GET /api/content/blob?repo=<full_path>&path=<file>[&ref=<ref>]` — blob content; text or base64 for binary
- `GET /api/content/state?repo=<full_path>&id=<state_id>` — full state JSON
- `GET /api/content/diff?repo=<full_path>&id=<state_id>` — line-level diff vs parent (via `similar` crate)
- `GET /api/content/actions?repo=<full_path>&id=<state_id>` — all Action objects targeting this state

All content GET endpoints accept `?repo=<full_path>` as a query param (not a path segment) to avoid slash ambiguity. Values are percent-decoded server-side.

**Biscuit auth details:**
- Tokens are Biscuit bearer tokens signed by the hosted server key.
- Browser, CLI, service-account, and attenuated agent tokens share the same Biscuit verifier.
- Revocation is session/blocklist based and enforced after signature verification.

Return structured denial reasons where possible so callers can distinguish scope failure from hosted-role failure.

## Railway-Oriented Deployment Notes

- Baseline hosted stack: Railway compute + Railway Postgres + Railway S3-compatible object storage.
- Prefer additive migrations and externally shared coordination primitives.
- For local Postgres integration tests, `railway dev up` plus an explicit database URL is the most reliable path.
- Container builds can use a multistage Docker build with a distroless runtime image, but the runtime image must include the binary's shared library dependencies and stay ABI-compatible with the builder base.
