# Hosted native Git runtime

The feature-gated `gateway_host --hosted CONFIG --bind 0.0.0.0:8080` executable composes the native client, existing Heddle SourcePack publication, the receiver-owned Git fence, and Artifacts Git catalog publication. The hosted Worker entry point is activation-disabled by default. This document describes code contracts; use the verification report for executed proof.

## Trust and state

1. The Git user obtains a short-lived transport session through a genuine Biscuit proof-of-possession exchange. The session identifies the actor; it is not a native signing key.
2. The Container uses an explicitly supplied, independently enrolled gateway identity. Its effective proof key must equal its source publisher. It signs as that gateway, while retaining the authenticated Git actor separately.
3. Each operation checks the current account/root/grants and the complete retained native disclosure closure. Immutable catalog metadata cannot authorize access. The native receiver repeats current authority and expected-old native State **and generation** checks inside its transaction.
4. Native acceptance commits originals, source availability and a semantic idempotency receipt atomically. A failed catalog publication does not roll back native acceptance. The journal recovers the exact native receipt and pending R2 data before acknowledging Git success.
5. The catalog contains a small versioned manifest. R2 contains native SourcePack artifacts. A served Git database is reconstructed as a private, selected closure, never a shared all-repository object store.

The explicit writer-only `POST /repositories/<slug>.git/native-refresh` operation takes the known current native State and expected catalog pin. The server proves that State is current before preparing an exact native-only refresh. This repairs an initialized Git window after native head or metadata changes. It cannot replace a pending push or produce a Git-push acceptance receipt. If a native operation legitimately rewound or replaced the native head, an explicit refresh reflects that native result and may require Git consumers to reconcile a non-fast-forward fetch; it does not make force-push available through receive-pack.

If native work advances after a Git push was durably accepted, explicit writer reconciliation retains that acceptance as an immutable audit outcome rather than weakening its generation fence. The original writer supplies the exact pending operation, observed current native head/generation and catalog pin. Current authority must cover both the accepted and current complete closures. Reconciliation accepts only the prior catalog pin or the byte-exact deterministic pending commit, rechecks authority, and records a terminal accepted-superseded outcome. It never sends a Git success report. A subsequent explicit native refresh publishes the current history. Ordinary retries cannot turn a terminal superseded outcome into a successful push. An explicitly selected `unresolved-superseded` outcome instead preserves acceptance as UNKNOWN, requires a strictly advanced native generation and the unchanged prior catalog, and refuses to erase a known receipt. A network/lookup error never selects that outcome automatically. Reconciliation is an operator API: the operator must inspect the exact pending journal and current native/catalog fences; a Git-only client is not expected to infer them. See the final verification ledger for the tested reconciliation paths.

## Operational configuration

Configuration is canonical JSON, with exact tenant/Spool/repository/Thread scope, a pinned native endpoint, fixed HTTPS authority origin, descriptor root, explicit gateway credential/key/SourceAuthor file paths, an Artifacts Git URL/credential file, an internal service-authorization digest, and a disposable scratch directory. The process runs as non-root. No ambient identity discovery, enrollment, or persistent credential creation occurs.

The native client uses Iroh transport. Cloudflare's HTTP outbound interception does not imply that QUIC is allowed. The hosted Container therefore requires a separate explicit native-network enablement gate; no HTTP-only egress guarantee is claimed for it. Live credentials, enrollment, infrastructure and deployment require their own approval.

## Diagnostics

Secret-free JSON lines on stderr use `gateway_phase` and `gateway_count` events. Correlation IDs are process-local integers. Phase labels cover current history authority, cold native hydration, native artifact restoration, Git reconstruction/protocol, receiver native acceptance/CAS and Artifacts catalog CAS. Request and source/Git payload sizes are counted; no tokens, keys, paths, account IDs, repository names, commit messages or content appear in diagnostic fields.

`wall_ms` is elapsed wall time. `process_cpu_ms` and `children_cpu_ms` are process and completed-child CPU deltas. Linux `disk_read_bytes` / `disk_write_bytes` are process I/O counters; `io_read_chars` / `io_write_chars` count syscall bytes and are **not network byte accounting**. `process_rss_bytes` is a point-in-time RSS sample and `process_high_water_bytes` is process-lifetime peak RSS. Nested spans overlap and should not be summed. Unsupported `/proc` counters are zero. These diagnostics are not trusted billable usage records.

Async native operations share a 120-second request deadline. Existing fixed HTTP and Git-child deadlines remain. Bounded synchronous validation checks the aggregate deadline at authority/response boundaries; it is not a preemptive CPU sandbox. The hosted path currently reconstructs a native projection per request. Its private transport uses HGF1 bounded binary frames: a small canonical metadata header and raw parts, rather than base64 source strings. Worker Git output is streamed; Rust still buffers and validates Git output before the final authority check. Decoded native source plans remain bounded buffers, so streaming does not imply unbounded-repository support. Prior immutable-view warm-cache benchmark numbers must not be attributed to this path until it is separately measured.

## Bounded compatibility

The current public-only window requires complete supported SHA-1 history. Hard limits include 128 native States (including the synthetic seed in applicable admission checks), 10,000 cumulative entries, 64 MiB cumulative history content, and 16 MiB per blob. Limits reject the operation instead of silently truncating history. SourcePack and metadata budgets are also checked over the entire old-plus-new closure before native acceptance.

Writes target one existing same-Thread branch and are fast-forward only. Branch creation/deletion, force pushes, multiple ref updates, tag publication, shallow/partial source history, Gitlinks/submodules and lossy imported commits remain outside this window. Native cross-Thread features require their own explicit authorization and are not inferred from Git ancestry. Git LFS pointer files are ordinary source bytes; this gateway does not implement the LFS object protocol.

See the repository corpus report for tested unusual names, modes, commits, graph shapes and limit boundaries. Do not infer production-scale support from the small historical performance fixtures.

## Isolated black-box fixture build

The non-default `gateway-fixture` feature implies `gateway-publication` and adds
an optional `fixture: {"ca_pem": "/explicit/test-ca.pem", "native_address":
"127.0.0.1:PORT"}` configuration field. The native descriptor and witness
authority stays the canonical `https://native-fixture.example`; an explicit
fixture-only resolver routes that exact hostname to the supplied loopback socket.
Its TLS identity, signed root and native canonical-authority checks remain intact.
The supplied test CA is the only trust source for this mapping, proxies are
disabled, and no OS DNS or hosts configuration is changed. The listener, session
authority and catalog endpoints are literal loopback. Certificate and hostname
verification remain enabled. Production builds exclude these fields and routing
seams. This is for synthetic Weft/QUIC/PostgreSQL integration, not activation.

## Binary transport contract

`application/vnd.heddle.native-frame-v1` begins with ASCII `HGF1`, a four-byte
big-endian metadata length, and canonical JSON `{payload, parts:[{name,length}]}`.
Raw named parts immediately follow in declared order. The header is at most
256 KiB; request and response totals retain the existing 112 MiB and 144 MiB
transport ceilings. These are defensive parsing limits, not hosted capacity
claims. Semantic native artifact/metadata/Git budgets remain tighter.

Allowed names are `proof`, `request`, `output`, `manifest`, and
`artifact/<sha256>`. Typed metadata markers must reference exactly the declared
part set. Duplicate names, incompatible artifact hashes, noncanonical JSON,
unknown markers, truncation and trailing bytes are rejected. Authorization and
native validation are unchanged by this transport format. Source buffers are
released after R2 staging before publication rereads them. The final test report
records the actual integrated memory envelope separately from core corpus limits.
